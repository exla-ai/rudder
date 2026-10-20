// The installable Slack app (OAuth v2, one bot token per installing org or
// account), as opposed to slack.ts, which is the single shared-channel bot
// configured by env. Pure helpers only: HTTP goes through an injected fetch so
// tests never reach Slack.

export const SLACK_SCOPES = ["chat:write", "chat:write.public", "commands", "channels:read", "groups:read", "im:write"];

export type SlackInstall = {
  accessToken: string;
  botUserId: string | null;
  scope: string | null;
  teamId: string;
  teamName: string;
};

export type SlackChannel = { id: string; name: string };

type FetchImpl = typeof fetch;

export function slackAuthorizeUrl(params: { clientId: string; redirectUri: string; state: string }): string {
  const url = new URL("https://slack.com/oauth/v2/authorize");
  url.searchParams.set("client_id", params.clientId);
  url.searchParams.set("scope", SLACK_SCOPES.join(","));
  url.searchParams.set("redirect_uri", params.redirectUri);
  url.searchParams.set("state", params.state);
  return url.toString();
}

async function slackJson(res: Response): Promise<Record<string, unknown>> {
  const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;
  return data && typeof data === "object" ? data : {};
}

export async function exchangeSlackCode(fetchImpl: FetchImpl, params: {
  clientId: string;
  clientSecret: string;
  code: string;
  redirectUri: string;
}): Promise<SlackInstall> {
  const res = await fetchImpl("https://slack.com/api/oauth.v2.access", {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      client_id: params.clientId,
      client_secret: params.clientSecret,
      code: params.code,
      redirect_uri: params.redirectUri,
    }).toString(),
  });
  const data = await slackJson(res);
  if (!data.ok || typeof data.access_token !== "string") {
    throw new Error(`slack oauth failed: ${String(data.error ?? res.status)}`);
  }
  const team = (data.team ?? {}) as Record<string, unknown>;
  return {
    accessToken: data.access_token,
    botUserId: typeof data.bot_user_id === "string" ? data.bot_user_id : null,
    scope: typeof data.scope === "string" ? data.scope : null,
    teamId: String(team.id ?? ""),
    teamName: String(team.name ?? "Slack"),
  };
}

// Public channels plus the private ones the bot was invited to, sorted by name.
// Stops at `max` so a huge workspace cannot hold the request open.
export async function listSlackChannels(fetchImpl: FetchImpl, token: string, max = 1000): Promise<SlackChannel[]> {
  const channels: SlackChannel[] = [];
  let cursor = "";
  for (let page = 0; page < 10 && channels.length < max; page += 1) {
    const url = new URL("https://slack.com/api/conversations.list");
    url.searchParams.set("types", "public_channel,private_channel");
    url.searchParams.set("exclude_archived", "true");
    url.searchParams.set("limit", "200");
    if (cursor) {
      url.searchParams.set("cursor", cursor);
    }
    const res = await fetchImpl(url.toString(), { headers: { Authorization: `Bearer ${token}` } });
    const data = await slackJson(res);
    if (!data.ok) {
      throw new Error(`slack conversations.list failed: ${String(data.error ?? res.status)}`);
    }
    for (const raw of (data.channels ?? []) as Array<Record<string, unknown>>) {
      if (typeof raw.id === "string" && typeof raw.name === "string") {
        channels.push({ id: raw.id, name: raw.name });
      }
    }
    cursor = String(((data.response_metadata ?? {}) as Record<string, unknown>).next_cursor ?? "");
    if (!cursor) {
      break;
    }
  }
  return channels.slice(0, max).sort((a, b) => a.name.localeCompare(b.name));
}

export async function postSlack(fetchImpl: FetchImpl, token: string, channel: string, text: string): Promise<void> {
  const res = await fetchImpl("https://slack.com/api/chat.postMessage", {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "content-type": "application/json; charset=utf-8" },
    body: JSON.stringify({ channel, text, mrkdwn: true, unfurl_links: false, unfurl_media: false }),
  });
  const data = await slackJson(res);
  if (!data.ok) {
    throw new Error(`slack chat.postMessage failed: ${String(data.error ?? res.status)}`);
  }
}

export async function revokeSlackToken(fetchImpl: FetchImpl, token: string): Promise<void> {
  await fetchImpl("https://slack.com/api/auth.revoke", {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "content-type": "application/x-www-form-urlencoded" },
    body: "",
  });
}

// Slack mrkdwn treats &, < and > as control characters.
export function slackEscape(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

export function slackLink(url: string, label: string): string {
  return `<${url}|${slackEscape(label).replace(/\|/g, "/")}>`;
}

export type SlashCommand =
  | { action: "help" }
  | { action: "link" }
  | { action: "status" }
  | { action: "projects" }
  | { action: "start"; project: string; task: string }
  | { action: "usage"; project: string };

export function parseSlashCommand(rawText: string): SlashCommand {
  const text = rawText.trim();
  const lower = text.toLowerCase();
  if (!text || lower === "help" || lower === "?") {
    return { action: "help" };
  }
  if (lower === "link" || lower === "login" || lower === "connect") {
    return { action: "link" };
  }
  if (lower === "status" || lower === "needs" || lower === "inbox") {
    return { action: "status" };
  }
  if (lower === "projects" || lower === "list" || lower === "ls") {
    return { action: "projects" };
  }
  const match = text.match(/^(\S+)\s+([\s\S]+)$/);
  if (!match) {
    return { action: "usage", project: text };
  }
  return { action: "start", project: match[1], task: match[2].trim() };
}

export const SLASH_HELP_TEXT = [
  "*Rudder* from Slack:",
  "• `/rudder <project> <task>` starts an agent on that project",
  "• `/rudder status` shows the agents that need you",
  "• `/rudder projects` lists your projects",
  "• `/rudder link` connects your Slack user to your Rudder account",
].join("\n");

export type MatchableProject = { workspaceId: string; name: string; repo?: string | null };

// A project is named by its name or repo slug (owner/name or just name),
// case-insensitive, or by a prefix that only one project has.
export function matchProject<T extends MatchableProject>(projects: T[], query: string): { match?: T; ambiguous?: T[] } {
  const q = query.trim().toLowerCase();
  if (!q) {
    return {};
  }
  const keys = (project: T): string[] => {
    const out = [project.name.toLowerCase()];
    if (project.repo) {
      const repo = project.repo.toLowerCase();
      out.push(repo, repo.split("/").pop() ?? repo);
    }
    return out;
  };
  const exact = projects.filter((project) => keys(project).includes(q));
  if (exact.length === 1) {
    return { match: exact[0] };
  }
  if (exact.length > 1) {
    return { ambiguous: exact };
  }
  const prefix = projects.filter((project) => keys(project).some((key) => key.startsWith(q)));
  if (prefix.length === 1) {
    return { match: prefix[0] };
  }
  return prefix.length > 1 ? { ambiguous: prefix } : {};
}
