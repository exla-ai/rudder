// Shared setup for the Linear task tests: a real createIntegrations on an
// in-memory SQLite, served over HTTP, with one injected fetch that fakes
// Linear (OAuth + GraphQL), OpenAI Responses and the Anthropic Messages API.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { existsSync } from "node:fs";
import http from "node:http";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const moduleUrl = new URL("../../cloud/dist/integrations.js", import.meta.url);
const sqlitePath = new URL("../../cloud/node_modules/better-sqlite3/package.json", import.meta.url);
const sdkPath = new URL("../../cloud/node_modules/@anthropic-ai/sdk/package.json", import.meta.url);
export const ready = [moduleUrl, sqlitePath, sdkPath].every((url) => existsSync(fileURLToPath(url)));
export const test = ready
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud/dist not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const { createIntegrations } = ready ? await import(moduleUrl.href) : {};
const Database = ready ? createRequire(fileURLToPath(sqlitePath))("better-sqlite3") : null;
export const taskgen = ready ? await import(new URL("../../cloud/dist/linear-tasks.js", import.meta.url).href) : {};
export const linear = ready ? await import(new URL("../../cloud/dist/linear.js", import.meta.url).href) : {};

export const OPENAI_KEY = "sk-test-OPENAI-SECRET-0123456789";
export const ANTHROPIC_KEY = "sk-ant-test-ANTHROPIC-SECRET-0123";
export const WEBHOOK_SECRET = "lin_wh_test_secret";
export const LINEAR_ORG = "org-lin-1";
export const BASE_ENV = {
  RUDDER_SECRETS_KEY: "test-secrets-key-0123456789abcdef",
  RUDDER_LINEAR_CLIENT_ID: "linear-client",
  RUDDER_LINEAR_CLIENT_SECRET: "linear-secret",
  RUDDER_LINEAR_WEBHOOK_SECRET: WEBHOOK_SECRET,
  OPENAI_API_KEY: OPENAI_KEY,
};

export const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

export const TASKS = [
  { title: "Add retry to the upload client", description: "Uploads fail on flaky networks.", done_when: "The upload test with a dropped connection passes." },
  { title: "Show upload progress", description: "Users cannot tell an upload is running.", done_when: "The upload page shows a percentage while uploading." },
  { title: "Log upload failures", description: "Failures are silent today.", done_when: "A failed upload writes one warning line." },
];

export function openaiResponse(payload) {
  return json({
    id: "resp_1",
    object: "response",
    status: "completed",
    output: [
      { type: "reasoning", id: "rs_1", summary: [] },
      { type: "message", id: "msg_1", role: "assistant", content: [{ type: "output_text", text: typeof payload === "string" ? payload : JSON.stringify(payload) }] },
    ],
  });
}

export function anthropicResponse(payload, stopReason = "end_turn") {
  return json({
    id: "msg_01",
    type: "message",
    role: "assistant",
    model: "claude-haiku-5-5",
    content: [{ type: "text", text: typeof payload === "string" ? payload : JSON.stringify(payload) }],
    stop_reason: stopReason,
    stop_sequence: null,
    usage: { input_tokens: 10, output_tokens: 20 },
  });
}

const STATES = [
  { id: "st-backlog", name: "Backlog", type: "backlog", position: 0 },
  { id: "st-todo", name: "Todo", type: "unstarted", position: 1 },
  { id: "st-review", name: "In Review", type: "started", position: 3 },
  { id: "st-progress", name: "In Progress", type: "started", position: 2 },
  { id: "st-done", name: "Done", type: "completed", position: 4 },
  { id: "st-cancel", name: "Canceled", type: "canceled", position: 5 },
];

export function fakeUpstream() {
  const calls = [];
  const linearState = {
    teams: [{ id: "team-eng", key: "ENG", name: "Engineering" }, { id: "team-ops", key: "OPS", name: "Ops" }],
    projects: [
      { id: "lp-libra", name: "Libra", teams: { nodes: [{ id: "team-eng", key: "ENG", name: "Engineering" }] } },
      { id: "lp-offstage", name: "Offstage", teams: { nodes: [{ id: "team-eng", key: "ENG", name: "Engineering" }] } },
      { id: "lp-rudder", name: "Rudder", teams: { nodes: [{ id: "team-ops", key: "OPS", name: "Ops" }, { id: "team-eng", key: "ENG", name: "Engineering" }] } },
    ],
    labels: [],
    issues: new Map(),
    created: [],
    comments: [],
    stateUpdates: [],
    labelNames: new Map(),
    nextIssue: 1,
  };
  const handlers = {
    linearDown: false,
    // Tracking decisions (the linear_track schema); tests replace this.
    classify: () => openaiResponse({ action: "skip", issue_identifier: null, title: null, reason: "not tracked by default" }),
    openai: (body, headers) => (body?.text?.format?.name === "linear_track" ? handlers.classify(body, headers) : openaiResponse({ tasks: TASKS })),
    anthropic: () => anthropicResponse({ tasks: TASKS }),
  };
  const stateName = (type) => STATES.find((s) => s.type === type)?.name ?? "Todo";
  const gql = (query, variables) => {
    if (query.includes("$filter: IssueFilter")) {
      const filter = variables.filter ?? {};
      const closed = filter.state?.type?.nin ?? [];
      const nodes = [...linearState.issues.values()]
        .filter((issue) => (filter.project ? issue.projectId === filter.project.id.eq : issue.teamId === filter.team?.id?.eq))
        .filter((issue) => !closed.includes(issue.stateType))
        .map((issue) => ({ id: issue.id, identifier: issue.identifier, title: issue.title, url: issue.url, priority: 0, state: { name: stateName(issue.stateType), type: issue.stateType }, team: { key: issue.identifier.split("-")[0] } }));
      return { issues: { nodes } };
    }
    if (query.includes("organization")) return { organization: { id: LINEAR_ORG, name: "Acme Linear", urlKey: "acme" } };
    if (query.includes("issueLabelCreate")) {
      const label = { id: `lab-new-${linearState.labels.length + 1}`, name: variables.input.name, team: { id: variables.input.teamId } };
      linearState.labels.push(label);
      return { issueLabelCreate: { success: true, issueLabel: { id: label.id, name: label.name } } };
    }
    if (query.includes("issueLabels")) {
      const team = linearState.teams.find((t) => t.id === variables.teamId) ?? null;
      const nodes = linearState.labels.filter((l) => l.name.toLowerCase() === String(variables.name).toLowerCase());
      return { team, issueLabels: { nodes } };
    }
    if (query.includes("issueCreate")) {
      const n = linearState.nextIssue++;
      const team = linearState.teams.find((t) => t.id === variables.input.teamId);
      const issue = { id: `iss-${n}`, identifier: `${team?.key ?? "X"}-${n}`, title: variables.input.title, url: `https://linear.app/acme/issue/${team?.key}-${n}`, description: variables.input.description, stateType: "unstarted", projectId: variables.input.projectId ?? null, teamId: variables.input.teamId };
      linearState.created.push(variables.input);
      linearState.issues.set(issue.id, issue);
      return { issueCreate: { success: true, issue } };
    }
    if (query.includes("issueUpdate")) {
      linearState.stateUpdates.push({ id: variables.id, stateId: variables.input.stateId });
      const issue = linearState.issues.get(variables.id);
      if (issue) issue.stateType = STATES.find((s) => s.id === variables.input.stateId)?.type ?? issue.stateType;
      return { issueUpdate: { success: true } };
    }
    if (query.includes("commentCreate")) {
      linearState.comments.push(variables.input);
      return { commentCreate: { success: true } };
    }
    if (query.includes("labels { nodes { name } }")) {
      return { issue: { labels: { nodes: (linearState.labelNames.get(variables.id) ?? []).map((name) => ({ name })) } } };
    }
    if (query.includes("team { states")) {
      const issue = linearState.issues.get(variables.id);
      return { issue: issue ? { id: issue.id, state: { id: "x", type: issue.stateType }, team: { states: { nodes: STATES } } } : null };
    }
    if (query.includes("issue(id")) {
      const issue = linearState.issues.get(variables.id);
      return { issue: issue ? { id: issue.id, identifier: issue.identifier, title: issue.title, url: issue.url, priority: 0, description: issue.description ?? "", state: { name: "Todo", type: issue.stateType }, team: { key: issue.identifier.split("-")[0] } } : null };
    }
    if (query.includes("projects(first")) return { projects: { nodes: linearState.projects } };
    if (query.includes("teams(first")) return { teams: { nodes: linearState.teams } };
    throw new Error(`unexpected linear query ${query.slice(0, 80)}`);
  };
  const fetchImpl = async (input, init = {}) => {
    const url = String(input instanceof Request ? input.url : input);
    const body = typeof init.body === "string" ? init.body : "";
    const headers = Object.fromEntries(new Headers(init.headers ?? {}).entries());
    calls.push({ url, method: init.method ?? "GET", body, headers });
    if (url === "https://api.openai.com/v1/responses") return handlers.openai(JSON.parse(body), headers);
    if (url.startsWith("https://api.anthropic.com/v1/messages")) return handlers.anthropic(JSON.parse(body), headers);
    if (url === "https://api.linear.app/oauth/token") {
      return json({ access_token: "lin_SECRET_ACCESS", refresh_token: "lin_refresh", expires_in: 86400, scope: "read,write" });
    }
    if (url === "https://api.linear.app/graphql") {
      if (handlers.linearDown) throw new Error("network down");
      const { query, variables } = JSON.parse(body);
      return json({ data: gql(query, variables ?? {}) });
    }
    throw new Error(`unexpected fetch ${url}`);
  };
  return { calls, fetchImpl, linearState, handlers };
}

export const PROJECTS = [
  { workspaceId: "ws-libra", name: "libra", repo: "exla-ai/libra", agents: [] },
  { workspaceId: "ws-offstage", name: "offstage", repo: "exla-ai/offstage", agents: [] },
  { workspaceId: "ws-rudder", name: "exla-ai-rudder", repo: "exla-ai/rudder", agents: [] },
];

export async function setup({ env = BASE_ENV, needsDebounceMs = 0 } = {}) {
  const database = new Database(":memory:");
  const upstream = fakeUpstream();
  const roles = new Map([["org1:alice", "admin"], ["org1:bob", "member"]]);
  const delivered = [];
  const events = [];
  const logs = [];
  const clock = { t: Date.now() };
  const deps = {
    database,
    env,
    fetch: upstream.fetchImpl,
    baseURL: "https://cloud.test",
    now: () => clock.t,
    orgRole: (accountId, orgId) => roles.get(`${orgId}:${accountId}`) ?? null,
    orgIdsForAccount: (accountId) => [...roles.keys()].filter((key) => key.endsWith(`:${accountId}`)).map((key) => key.split(":")[0]),
    listProjects: () => PROJECTS,
    deliverTask: async (accountId, workspaceId, text) => { delivered.push({ accountId, workspaceId, text }); },
    capture: (accountId, name, props) => events.push({ accountId, name, props }),
    log: (message) => logs.push(message),
    needsDebounceMs,
  };
  const integrations = createIntegrations(deps);
  /** A second control plane on the same database, as after a restart. */
  const restart = () => createIntegrations(deps);
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://localhost");
    const accountId = req.headers["x-account"];
    const handled = accountId
      ? await integrations.handleAppApi(req, res, url, { accountId })
      : await integrations.handlePublicApi(req, res, url);
    if (!handled) { res.writeHead(404); res.end(); }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (method, path, { account, body, headers = {} } = {}) => {
    const res = await fetch(base + path, {
      method,
      redirect: "manual",
      headers: { ...(account ? { "x-account": account } : {}), ...(body !== undefined && typeof body !== "string" ? { "content-type": "application/json" } : {}), ...headers },
      body: body === undefined ? undefined : typeof body === "string" ? body : JSON.stringify(body),
    });
    const text = await res.text();
    let data = null;
    try { data = JSON.parse(text); } catch {}
    return { status: res.status, data, location: res.headers.get("location") };
  };
  const close = () => new Promise((resolve) => server.close(resolve));
  return { integrations, database, upstream, delivered, events, logs, call, close, clock, restart };
}

export async function connectLinear(ctx, account, org) {
  const start = await ctx.call("POST", "/api/app/integrations/linear/start", { account, body: org ? { org } : {} });
  assert.equal(start.status, 200, JSON.stringify(start.data));
  const state = new URL(start.data.url).searchParams.get("state");
  const cb = await ctx.call("GET", `/api/integrations/linear/callback?code=c&state=${encodeURIComponent(state)}`);
  assert.match(cb.location, /connected=linear/);
}

export function signWebhook(body, secret = WEBHOOK_SECRET) {
  return createHmac("sha256", secret).update(body).digest("hex");
}

export async function sendWebhook(ctx, payload, { signature, raw } = {}) {
  const body = raw ?? JSON.stringify(payload);
  return ctx.call("POST", "/api/integrations/linear/webhook", {
    body,
    headers: { "content-type": "application/json", "linear-signature": signature ?? signWebhook(body) },
  });
}

export function issuePayload(ctx, data, extra = {}) {
  return {
    action: "create",
    type: "Issue",
    organizationId: LINEAR_ORG,
    webhookTimestamp: ctx.clock.t,
    webhookId: "wh-1",
    ...extra,
    data: {
      id: "iss-100",
      identifier: "ENG-100",
      title: "Add retry",
      teamId: "team-eng",
      projectId: "lp-rudder",
      labelIds: ["lab-1"],
      labels: [{ id: "lab-1", name: "rudder", color: "#5E6AD2" }],
      state: { id: "st-todo", name: "Todo", type: "unstarted" },
      ...data,
    },
  };
}

export function seedIssue(ctx, issue) {
  ctx.upstream.linearState.issues.set(issue.id, { url: `https://linear.app/acme/issue/${issue.identifier}`, description: "", stateType: "unstarted", ...issue });
}

/** A tracking decision as the fake OpenAI returns it (the linear_track schema). */
export function trackDecision({ action, issue = null, title = null, reason = "test" }) {
  return openaiResponse({ action, issue_identifier: issue, title, reason });
}

/** All text the server logged or returned, to check no key leaked. */
export function assertNoSecrets(texts) {
  const all = texts.join("\n");
  for (const secret of [OPENAI_KEY, ANTHROPIC_KEY, WEBHOOK_SECRET, "lin_SECRET_ACCESS"]) {
    assert.ok(!all.includes(secret), `a secret leaked: ${secret.slice(0, 8)}...`);
  }
}
