// Linear: OAuth, the few GraphQL calls Rudder makes, and the task text an issue
// becomes. HTTP goes through an injected fetch so tests never reach Linear.

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

export async function linearWorkspace(fetchImpl: FetchImpl, token: string): Promise<{ name: string; urlKey: string | null }> {
  const data = await linearGraphql<{ organization?: { name?: string; urlKey?: string } }>(
    fetchImpl,
    token,
    "query { organization { name urlKey } }",
  );
  return { name: data.organization?.name ?? "Linear", urlKey: data.organization?.urlKey ?? null };
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

export const LINEAR_DESCRIPTION_LIMIT = 4000;

export function linearTaskText(issue: LinearIssue): string {
  let description = (issue.description ?? "").trim();
  if (description.length > LINEAR_DESCRIPTION_LIMIT) {
    description = `${description.slice(0, LINEAR_DESCRIPTION_LIMIT).trimEnd()}\n[description truncated]`;
  }
  return [`Linear ${issue.identifier}: ${issue.title}`, description, issue.url].filter(Boolean).join("\n\n");
}
