// Linear: OAuth, the few GraphQL calls Rudder makes, and the task text an issue
// becomes. HTTP goes through an injected fetch so tests never reach Linear.

import { createHmac, timingSafeEqual } from "node:crypto";

export const LINEAR_SCOPES = ["read", "write", "issues:create", "comments:create"];
export const LINEAR_GRAPHQL_URL = "https://api.linear.app/graphql";
export const LINEAR_TOKEN_URL = "https://api.linear.app/oauth/token";

type FetchImpl = typeof fetch;

export type LinearToken = {
  accessToken: string;
  refreshToken: string | null;
  // Epoch ms, or null for a token that does not expire.
  expiresAt: number | null;
  scope: string | null;
};

export type LinearIssue = {
  id: string;
  identifier: string;
  title: string;
  url: string;
  state: string | null;
  stateType: string | null;
  priority: number | null;
  team: string | null;
  description?: string | null;
};

export function linearAuthorizeUrl(params: { clientId: string; redirectUri: string; state: string }): string {
  const url = new URL("https://linear.app/oauth/authorize");
  url.searchParams.set("client_id", params.clientId);
  url.searchParams.set("redirect_uri", params.redirectUri);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("scope", LINEAR_SCOPES.join(","));
  url.searchParams.set("state", params.state);
  url.searchParams.set("prompt", "consent");
  return url.toString();
}

function tokenFromResponse(data: Record<string, unknown>, now: number, previous?: LinearToken): LinearToken {
  if (typeof data.access_token !== "string" || !data.access_token) {
    throw new Error(`linear oauth failed: ${String(data.error_description ?? data.error ?? "no access_token")}`);
  }
  const expiresIn = Number(data.expires_in);
  return {
    accessToken: data.access_token,
    refreshToken: typeof data.refresh_token === "string" ? data.refresh_token : previous?.refreshToken ?? null,
    expiresAt: Number.isFinite(expiresIn) && expiresIn > 0 ? now + expiresIn * 1000 : null,
    scope: typeof data.scope === "string" ? data.scope : Array.isArray(data.scope) ? data.scope.join(",") : null,
  };
}

async function postForm(fetchImpl: FetchImpl, body: Record<string, string>): Promise<Record<string, unknown>> {
  const res = await fetchImpl(LINEAR_TOKEN_URL, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams(body).toString(),
  });
  const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;
  if (!res.ok) {
    throw new Error(`linear oauth failed: ${String(data.error_description ?? data.error ?? res.status)}`);
  }
  return data;
}

export async function exchangeLinearCode(fetchImpl: FetchImpl, params: {
  clientId: string;
  clientSecret: string;
  code: string;
  redirectUri: string;
  now: number;
}): Promise<LinearToken> {
  const data = await postForm(fetchImpl, {
    grant_type: "authorization_code",
    code: params.code,
    redirect_uri: params.redirectUri,
    client_id: params.clientId,
    client_secret: params.clientSecret,
  });
  return tokenFromResponse(data, params.now);
}

export async function refreshLinearToken(fetchImpl: FetchImpl, params: {
  clientId: string;
  clientSecret: string;
  token: LinearToken;
  now: number;
}): Promise<LinearToken> {
  if (!params.token.refreshToken) {
    throw new Error("linear token expired; reconnect Linear");
  }
  const data = await postForm(fetchImpl, {
    grant_type: "refresh_token",
    refresh_token: params.token.refreshToken,
    client_id: params.clientId,
    client_secret: params.clientSecret,
  });
  return tokenFromResponse(data, params.now, params.token);
}

export async function revokeLinearToken(fetchImpl: FetchImpl, token: string): Promise<void> {
  await fetchImpl("https://api.linear.app/oauth/revoke", {
    method: "POST",
    headers: { Authorization: `Bearer ${token}` },
  });
}

export async function linearGraphql<T = Record<string, unknown>>(
  fetchImpl: FetchImpl,
  token: string,
  query: string,
  variables: Record<string, unknown> = {},
): Promise<T> {
  const res = await fetchImpl(LINEAR_GRAPHQL_URL, {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  const data = (await res.json().catch(() => ({}))) as { data?: T; errors?: Array<{ message?: string }> };
  if (!res.ok || data.errors?.length || !data.data) {
    const message = data.errors?.map((error) => error.message).filter(Boolean).join("; ");
    throw Object.assign(new Error(`linear: ${message || `HTTP ${res.status}`}`), { status: res.status === 401 ? 401 : 502 });
  }
  return data.data;
}

const ISSUE_FIELDS = "id identifier title url priority state { name type } team { key }";
const OPEN_FILTER = '{ state: { type: { nin: ["completed", "canceled"] } } }';

function toIssue(raw: Record<string, unknown>): LinearIssue {
  const state = (raw.state ?? null) as Record<string, unknown> | null;
  const team = (raw.team ?? null) as Record<string, unknown> | null;
  return {
    id: String(raw.id),
    identifier: String(raw.identifier ?? ""),
    title: String(raw.title ?? ""),
    url: String(raw.url ?? ""),
    state: state?.name ? String(state.name) : null,
    stateType: state?.type ? String(state.type) : null,
    priority: typeof raw.priority === "number" ? raw.priority : null,
    team: team?.key ? String(team.key) : null,
    ...(raw.description !== undefined ? { description: raw.description == null ? null : String(raw.description) } : {}),
  };
}

function nodes(value: unknown): Array<Record<string, unknown>> {
  const list = (value as { nodes?: unknown } | null)?.nodes;
  return Array.isArray(list) ? (list as Array<Record<string, unknown>>) : [];
}

// The workspace id is what Linear webhooks name (organizationId), so a webhook
// can find the install it belongs to.
export async function linearWorkspace(
  fetchImpl: FetchImpl,
  token: string,
): Promise<{ id: string | null; name: string; urlKey: string | null }> {
  const data = await linearGraphql<{ organization?: { id?: string; name?: string; urlKey?: string } }>(
    fetchImpl,
    token,
    "query { organization { id name urlKey } }",
  );
  return {
    id: data.organization?.id ?? null,
    name: data.organization?.name ?? "Linear",
    urlKey: data.organization?.urlKey ?? null,
  };
}

// Open issues assigned to the token's user first, then recently updated open
// issues (or search results when `q` is given), deduped.
export async function listLinearIssues(fetchImpl: FetchImpl, token: string, q: string, limit = 50): Promise<LinearIssue[]> {
  const query = q.trim();
  const data = await linearGraphql<Record<string, unknown>>(
    fetchImpl,
    token,
    query
      ? `query($q: String!) {
          viewer { assignedIssues(first: 50, filter: ${OPEN_FILTER}, orderBy: updatedAt) { nodes { ${ISSUE_FIELDS} } } }
          searchIssues(term: $q, first: 50) { nodes { ${ISSUE_FIELDS} } }
        }`
      : `query {
          viewer { assignedIssues(first: 50, filter: ${OPEN_FILTER}, orderBy: updatedAt) { nodes { ${ISSUE_FIELDS} } } }
          issues(first: 50, filter: ${OPEN_FILTER}, orderBy: updatedAt) { nodes { ${ISSUE_FIELDS} } }
        }`,
    query ? { q: query } : {},
  );
  const needle = query.toLowerCase();
  const matches = (issue: LinearIssue) =>
    !needle || issue.title.toLowerCase().includes(needle) || issue.identifier.toLowerCase().includes(needle);
  const assigned = nodes((data.viewer as Record<string, unknown> | undefined)?.assignedIssues).map(toIssue).filter(matches);
  const rest = nodes(query ? data.searchIssues : data.issues).map(toIssue);
  const seen = new Set<string>();
  const out: LinearIssue[] = [];
  for (const issue of [...assigned, ...rest]) {
    if (seen.has(issue.id)) {
      continue;
    }
    seen.add(issue.id);
    out.push(issue);
    if (out.length >= limit) {
      break;
    }
  }
  return out;
}

export async function getLinearIssue(fetchImpl: FetchImpl, token: string, id: string): Promise<LinearIssue | null> {
  const data = await linearGraphql<{ issue?: Record<string, unknown> | null }>(
    fetchImpl,
    token,
    `query($id: String!) { issue(id: $id) { ${ISSUE_FIELDS} description } }`,
    { id },
  );
  return data.issue ? toIssue(data.issue) : null;
}

export async function commentOnLinearIssue(fetchImpl: FetchImpl, token: string, issueId: string, body: string): Promise<void> {
  await linearGraphql(
    fetchImpl,
    token,
    "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
    { input: { issueId, body } },
  );
}

/** Open issues of one Linear project (or, without one, one team), most recently updated first. */
export async function listOpenLinearIssues(
  fetchImpl: FetchImpl,
  token: string,
  scope: { projectId?: string | null; teamId?: string | null },
  limit = 100,
): Promise<LinearIssue[]> {
  const where = scope.projectId ? { project: { id: { eq: scope.projectId } } } : { team: { id: { eq: scope.teamId ?? "" } } };
  const data = await linearGraphql<{ issues?: unknown }>(
    fetchImpl,
    token,
    `query($filter: IssueFilter) { issues(first: ${Math.max(1, Math.min(250, Math.floor(limit)))}, filter: $filter, orderBy: updatedAt) { nodes { ${ISSUE_FIELDS} } } }`,
    { filter: { ...where, state: { type: { nin: ["completed", "canceled"] } } } },
  );
  return nodes(data.issues).map(toIssue);
}

export type LinearTeam = { id: string; key: string; name: string };

export async function listLinearTeams(fetchImpl: FetchImpl, token: string): Promise<LinearTeam[]> {
  const data = await linearGraphql<{ teams?: unknown }>(fetchImpl, token, "query { teams(first: 100) { nodes { id key name } } }");
  return nodes(data.teams)
    .map((raw) => ({ id: String(raw.id ?? ""), key: String(raw.key ?? ""), name: String(raw.name ?? "") }))
    .filter((team) => team.id)
    .sort((a, b) => a.name.localeCompare(b.name));
}

export type LinearProject = { id: string; name: string; teams: LinearTeam[] };

export async function listLinearProjects(fetchImpl: FetchImpl, token: string): Promise<LinearProject[]> {
  const data = await linearGraphql<{ projects?: unknown }>(
    fetchImpl,
    token,
    "query { projects(first: 100) { nodes { id name teams(first: 20) { nodes { id key name } } } } }",
  );
  return nodes(data.projects)
    .map((raw) => ({
      id: String(raw.id ?? ""),
      name: String(raw.name ?? ""),
      teams: nodes(raw.teams).map((team) => ({ id: String(team.id ?? ""), key: String(team.key ?? ""), name: String(team.name ?? "") })),
    }))
    .filter((project) => project.id)
    .sort((a, b) => a.name.localeCompare(b.name));
}

export const RUDDER_LABEL = "rudder";
const RUDDER_LABEL_COLOR = "#5E6AD2";

/**
 * The `rudder` label an issue in `teamId` can carry: the team's own label, else
 * a workspace label of that name, else a new team label. Also returns the
 * team (Linear errors when the id is not a team this token can see).
 */
export async function findOrCreateRudderLabel(
  fetchImpl: FetchImpl,
  token: string,
  teamId: string,
): Promise<{ labelId: string; created: boolean; team: LinearTeam }> {
  const data = await linearGraphql<{ team?: Record<string, unknown> | null; issueLabels?: unknown }>(
    fetchImpl,
    token,
    `query($teamId: String!, $name: String!) {
      team(id: $teamId) { id key name }
      issueLabels(first: 50, filter: { name: { eqIgnoreCase: $name } }) { nodes { id name team { id } } }
    }`,
    { teamId, name: RUDDER_LABEL },
  );
  if (!data.team?.id) {
    throw Object.assign(new Error("Linear team not found"), { status: 404 });
  }
  const team = { id: String(data.team.id), key: String(data.team.key ?? ""), name: String(data.team.name ?? "") };
  const labels = nodes(data.issueLabels).filter((label) => String(label.name ?? "").toLowerCase() === RUDDER_LABEL);
  const teamOf = (label: Record<string, unknown>) => (label.team as { id?: unknown } | null | undefined)?.id ?? null;
  const found = labels.find((label) => teamOf(label) === team.id) ?? labels.find((label) => teamOf(label) === null);
  if (found?.id) {
    return { labelId: String(found.id), created: false, team };
  }
  const created = await linearGraphql<{ issueLabelCreate?: { success?: boolean; issueLabel?: { id?: string } | null } }>(
    fetchImpl,
    token,
    "mutation($input: IssueLabelCreateInput!) { issueLabelCreate(input: $input) { success issueLabel { id name } } }",
    { input: { name: RUDDER_LABEL, teamId: team.id, color: RUDDER_LABEL_COLOR } },
  );
  const labelId = created.issueLabelCreate?.issueLabel?.id;
  if (!created.issueLabelCreate?.success || !labelId) {
    throw Object.assign(new Error("linear: could not create the rudder label"), { status: 502 });
  }
  return { labelId: String(labelId), created: true, team };
}

export type CreatedLinearIssue = { id: string; identifier: string; title: string; url: string };

export async function createLinearIssue(
  fetchImpl: FetchImpl,
  token: string,
  input: { teamId: string; projectId?: string; title: string; description: string; labelIds?: string[] },
): Promise<CreatedLinearIssue> {
  const data = await linearGraphql<{ issueCreate?: { success?: boolean; issue?: Record<string, unknown> | null } }>(
    fetchImpl,
    token,
    "mutation($input: IssueCreateInput!) { issueCreate(input: $input) { success issue { id identifier title url } } }",
    { input },
  );
  const issue = data.issueCreate?.issue;
  if (!data.issueCreate?.success || !issue?.id) {
    throw Object.assign(new Error("linear: issue was not created"), { status: 502 });
  }
  return {
    id: String(issue.id),
    identifier: String(issue.identifier ?? ""),
    title: String(issue.title ?? ""),
    url: String(issue.url ?? ""),
  };
}

export type LinearWorkflowState = { id: string; name: string; type: string; position: number };

/** An issue's current state type and its team's workflow states, in one call. */
export async function linearIssueWorkflow(
  fetchImpl: FetchImpl,
  token: string,
  issueId: string,
): Promise<{ stateType: string | null; states: LinearWorkflowState[] } | null> {
  const data = await linearGraphql<{ issue?: Record<string, unknown> | null }>(
    fetchImpl,
    token,
    `query($id: String!) { issue(id: $id) { id state { id type } team { states { nodes { id name type position } } } } }`,
    { id: issueId },
  );
  if (!data.issue) {
    return null;
  }
  const state = data.issue.state as { type?: unknown } | null | undefined;
  const team = data.issue.team as { states?: unknown } | null | undefined;
  return {
    stateType: typeof state?.type === "string" ? state.type : null,
    states: nodes(team?.states).map((raw) => ({
      id: String(raw.id ?? ""),
      name: String(raw.name ?? ""),
      type: String(raw.type ?? ""),
      position: Number(raw.position ?? 0),
    })),
  };
}

/** The team's first state of a type ("started", "completed") by board position. */
export function firstStateOfType(states: LinearWorkflowState[], type: string): LinearWorkflowState | null {
  return states.filter((state) => state.type === type && state.id).sort((a, b) => a.position - b.position)[0] ?? null;
}

export async function setLinearIssueState(fetchImpl: FetchImpl, token: string, issueId: string, stateId: string): Promise<void> {
  await linearGraphql(
    fetchImpl,
    token,
    "mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }",
    { id: issueId, input: { stateId } },
  );
}

/** Label names on an issue, for a webhook payload that sent only label ids. */
export async function linearIssueLabelNames(fetchImpl: FetchImpl, token: string, issueId: string): Promise<string[]> {
  const data = await linearGraphql<{ issue?: { labels?: unknown } | null }>(
    fetchImpl,
    token,
    "query($id: String!) { issue(id: $id) { labels { nodes { name } } } }",
    { id: issueId },
  );
  return nodes(data.issue?.labels).map((label) => String(label.name ?? ""));
}

/**
 * Linear signs a webhook with `Linear-Signature`: hex HMAC-SHA256 of the raw
 * body under the webhook's signing secret. The payload's webhookTimestamp (ms)
 * must be within a minute of now, so a captured delivery cannot be replayed.
 */
export const LINEAR_WEBHOOK_MAX_SKEW_MS = 60_000;

export function verifyLinearSignature(secret: string, rawBody: string, signature: string | undefined): boolean {
  if (!secret || !signature || !/^[0-9a-f]+$/i.test(signature.trim())) {
    return false;
  }
  const expected = createHmac("sha256", secret).update(rawBody).digest();
  const given = Buffer.from(signature.trim(), "hex");
  return given.length === expected.length && timingSafeEqual(given, expected);
}

export function linearWebhookFresh(timestamp: unknown, now: number): boolean {
  const at = Number(timestamp);
  return Number.isFinite(at) && at > 0 && Math.abs(now - at) <= LINEAR_WEBHOOK_MAX_SKEW_MS;
}

const nameKey = (value: string) => value.toLowerCase().replace(/[^a-z0-9]/g, "");
const nameTokens = (value: string) => value.toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);

/**
 * The Rudder project a Linear project or team most likely is, by name: an
 * exact name or repo match first ("Libra" -> libra), then a name whose last
 * word matches ("Rudder" -> exla-ai-rudder). A tie is no suggestion.
 */
export function suggestRudderProject<T extends { workspaceId: string; name: string; repo?: string | null }>(
  linearName: string,
  projects: T[],
): T | null {
  const target = nameKey(linearName);
  if (!target) {
    return null;
  }
  const score = (project: T): number => {
    const repoName = (project.repo ?? "").split("/").pop() ?? "";
    const names = [project.name, repoName].filter(Boolean);
    if (names.some((name) => nameKey(name) === target)) {
      return 3;
    }
    if (names.some((name) => nameKey(nameTokens(name).slice(-1)[0] ?? "") === target)) {
      return 2;
    }
    if (names.some((name) => nameTokens(name).some((token) => token === target))) {
      return 1;
    }
    return 0;
  };
  const ranked = projects.map((project) => ({ project, score: score(project) })).filter((entry) => entry.score > 0);
  const best = Math.max(0, ...ranked.map((entry) => entry.score));
  const top = ranked.filter((entry) => entry.score === best);
  return top.length === 1 ? top[0].project : null;
}

export const LINEAR_DESCRIPTION_LIMIT = 4000;

export function linearTaskText(issue: LinearIssue): string {
  let description = (issue.description ?? "").trim();
  if (description.length > LINEAR_DESCRIPTION_LIMIT) {
    description = `${description.slice(0, LINEAR_DESCRIPTION_LIMIT).trimEnd()}\n[description truncated]`;
  }
  return [`Linear ${issue.identifier}: ${issue.title}`, description, issue.url].filter(Boolean).join("\n\n");
}
