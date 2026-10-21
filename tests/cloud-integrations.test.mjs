import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { existsSync } from "node:fs";
import http from "node:http";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

// Same skip rule as tests/slack.test.mjs: the cloud/ subproject has its own
// build and dependencies (better-sqlite3), so a fresh checkout skips.
const moduleUrl = new URL("../cloud/dist/integrations.js", import.meta.url);
const sqlitePath = new URL("../cloud/node_modules/better-sqlite3/package.json", import.meta.url);
const ready = existsSync(fileURLToPath(moduleUrl)) && existsSync(fileURLToPath(sqlitePath));
const test = ready
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud/dist not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const { createIntegrations, verifyState } = ready ? await import(moduleUrl.href) : {};
const Database = ready ? createRequire(fileURLToPath(sqlitePath))("better-sqlite3") : null;
const { matchProject, parseSlashCommand } = ready ? await import(new URL("../cloud/dist/slack-app.js", import.meta.url).href) : {};
const { linearTaskText } = ready ? await import(new URL("../cloud/dist/linear.js", import.meta.url).href) : {};

const SIGNING = "slack-signing-secret";
const ENV = {
  RUDDER_SECRETS_KEY: "test-secrets-key-0123456789abcdef",
  RUDDER_SLACK_CLIENT_ID: "slack-client",
  RUDDER_SLACK_CLIENT_SECRET: "slack-secret",
  RUDDER_SLACK_SIGNING_SECRET: SIGNING,
  RUDDER_LINEAR_CLIENT_ID: "linear-client",
  RUDDER_LINEAR_CLIENT_SECRET: "linear-secret",
};

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

function fakeUpstream() {
  const calls = [];
  const fetchImpl = async (input, init = {}) => {
    const url = String(input);
    const body = typeof init.body === "string" ? init.body : "";
    calls.push({ url, method: init.method ?? "GET", body, headers: init.headers ?? {} });
    if (url.startsWith("https://slack.com/api/oauth.v2.access")) {
      const code = new URLSearchParams(body).get("code");
      if (code === "bad") return json({ ok: false, error: "invalid_code" });
      return json({ ok: true, access_token: "xoxb-SECRET-BOT-TOKEN", bot_user_id: "UBOT", scope: "chat:write", team: { id: "T1", name: "Acme" } });
    }
    if (url.startsWith("https://slack.com/api/conversations.list")) {
      const cursor = new URL(url).searchParams.get("cursor");
      if (!cursor) return json({ ok: true, channels: [{ id: "C2", name: "eng" }], response_metadata: { next_cursor: "p2" } });
      return json({ ok: true, channels: [{ id: "C1", name: "agents" }], response_metadata: { next_cursor: "" } });
    }
    if (url.startsWith("https://slack.com/api/chat.postMessage")) return json({ ok: true, ts: "1.1" });
    if (url.startsWith("https://slack.com/api/auth.revoke")) return json({ ok: true });
    if (url.startsWith("https://hooks.slack.com/")) return new Response("ok");
    if (url === "https://api.linear.app/oauth/token") {
      return json({ access_token: "lin_SECRET_ACCESS", refresh_token: "lin_refresh", expires_in: 86400, scope: "read,write" });
    }
    if (url === "https://api.linear.app/graphql") {
      const { query, variables } = JSON.parse(body);
      if (query.includes("organization")) return json({ data: { organization: { name: "Acme Linear", urlKey: "acme" } } });
      if (query.includes("commentCreate")) return json({ data: { commentCreate: { success: true } } });
      if (query.includes("issue(id")) {
        assert.equal(variables.id, "iss-1");
        return json({ data: { issue: { id: "iss-1", identifier: "ENG-7", title: "Fix login", url: "https://linear.app/acme/issue/ENG-7", priority: 2, description: "It breaks on Safari.", state: { name: "Todo", type: "unstarted" }, team: { key: "ENG" } } } });
      }
      const node = (id, identifier, title) => ({ id, identifier, title, url: `https://linear.app/x/${identifier}`, priority: 1, state: { name: "Todo", type: "unstarted" }, team: { key: "ENG" } });
      return json({ data: {
        viewer: { assignedIssues: { nodes: [node("iss-1", "ENG-7", "Fix login")] } },
        issues: { nodes: [node("iss-2", "ENG-8", "Docs"), node("iss-1", "ENG-7", "Fix login")] },
        searchIssues: { nodes: [node("iss-3", "ENG-9", "Login copy")] },
      } });
    }
    if (url === "https://api.linear.app/oauth/revoke") return new Response("", { status: 200 });
    throw new Error(`unexpected fetch ${url}`);
  };
  return { calls, fetchImpl };
}

async function setup({ env = ENV, now, needsDebounceMs = 0 } = {}) {
  const database = new Database(":memory:");
  const upstream = fakeUpstream();
  const roles = new Map([["org1:alice", "admin"], ["org1:bob", "member"]]);
  const delivered = [];
  const events = [];
  let projects = [{ workspaceId: "ws1", name: "rudder", repo: "exla-ai/rudder", agents: [] }];
  const clock = { t: now ?? Date.now() };
  const integrations = createIntegrations({
    database,
    env,
    fetch: upstream.fetchImpl,
    baseURL: "https://cloud.test",
    now: () => clock.t,
    orgRole: (accountId, orgId) => roles.get(`${orgId}:${accountId}`) ?? null,
    orgIdsForAccount: (accountId) => [...roles.keys()].filter((key) => key.endsWith(`:${accountId}`)).map((key) => key.split(":")[0]),
    listProjects: () => projects,
    deliverTask: async (accountId, workspaceId, text) => { delivered.push({ accountId, workspaceId, text }); },
    capture: (accountId, name, props) => events.push({ accountId, name, props }),
    log: () => {},
    needsDebounceMs,
  });
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
  return { integrations, database, upstream, delivered, events, call, close, clock, setProjects: (next) => { projects = next; } };
}

async function connectSlack(ctx, account, org) {
  const start = await ctx.call("POST", "/api/app/integrations/slack/start", { account, body: org ? { org } : {} });
  assert.equal(start.status, 200, JSON.stringify(start.data));
  const state = new URL(start.data.url).searchParams.get("state");
  const cb = await ctx.call("GET", `/api/integrations/slack/callback?code=good&state=${encodeURIComponent(state)}`);
  return { start, state, cb };
}

function signedCommand(ctx, fields) {
  const body = new URLSearchParams({ command: "/rudder", response_url: "https://hooks.slack.com/commands/x", ...fields }).toString();
  const ts = String(Math.floor(ctx.clock.t / 1000));
  const sig = "v0=" + createHmac("sha256", SIGNING).update(`v0:${ts}:${body}`).digest("hex");
  return ctx.call("POST", "/api/integrations/slack/commands", {
    body,
    headers: { "content-type": "application/x-www-form-urlencoded", "x-slack-request-timestamp": ts, "x-slack-signature": sig },
  });
}

test("slack oauth round trip stores an encrypted token and lists channels", async () => {
  const ctx = await setup();
  try {
    const { start, cb } = await connectSlack(ctx, "alice");
    const authorize = new URL(start.data.url);
    assert.equal(authorize.origin + authorize.pathname, "https://slack.com/oauth/v2/authorize");
    assert.equal(authorize.searchParams.get("redirect_uri"), "https://cloud.test/api/integrations/slack/callback");
    assert.match(authorize.searchParams.get("scope"), /chat:write,.*commands/);
    assert.equal(cb.status, 302);
    assert.equal(cb.location, "/app/settings?connected=slack");
    const exchange = ctx.upstream.calls.find((c) => c.url.includes("oauth.v2.access"));
    assert.equal(new URLSearchParams(exchange.body).get("client_secret"), "slack-secret");

    const dump = JSON.stringify(ctx.database.prepare("select * from rudder_integrations").all());
    assert.ok(!dump.includes("xoxb-SECRET-BOT-TOKEN"), "token must not be stored in plaintext");

    const summary = await ctx.call("GET", "/api/app/integrations", { account: "alice" });
    assert.deepEqual(summary.data.slack, { configured: true, connected: true, team: { id: "T1", name: "Acme" }, channel: null });
    assert.equal(summary.data.linear.connected, false);
    assert.equal(summary.data.canManage, true);

    const channels = await ctx.call("GET", "/api/app/integrations/slack/channels", { account: "alice" });
    assert.deepEqual(channels.data.channels, [{ id: "C1", name: "agents" }, { id: "C2", name: "eng" }]);
    const bot = ctx.upstream.calls.find((c) => c.url.includes("conversations.list"));
    assert.equal(bot.headers.Authorization, "Bearer xoxb-SECRET-BOT-TOKEN");

    const chosen = await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "alice", body: { channel: "#agents" } });
    assert.deepEqual(chosen.data, { channel: { id: "C1", name: "agents" } });
    const missing = await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "alice", body: { channel: "nope" } });
    assert.equal(missing.status, 404);
    const after = await ctx.call("GET", "/api/app/integrations", { account: "alice" });
    assert.deepEqual(after.data.slack.channel, { id: "C1", name: "agents" });

    const del = await ctx.call("DELETE", "/api/app/integrations/slack", { account: "alice" });
    assert.equal(del.status, 200);
    assert.ok(ctx.upstream.calls.some((c) => c.url.includes("auth.revoke")));
    assert.equal((await ctx.call("GET", "/api/app/integrations", { account: "alice" })).data.slack.connected, false);
    assert.deepEqual(ctx.events.map((e) => e.name), ["integration_connected", "integration_disconnected"]);
  } finally {
    await ctx.close();
  }
});

test("bad, tampered, expired and cancelled oauth states are refused", async () => {
  const ctx = await setup();
  try {
    const start = await ctx.call("POST", "/api/app/integrations/slack/start", { account: "alice", body: {} });
    const state = new URL(start.data.url).searchParams.get("state");
    const [body] = state.split(".");
    const tampered = `${body}.${"A".repeat(43)}`;
    let res = await ctx.call("GET", `/api/integrations/slack/callback?code=good&state=${encodeURIComponent(tampered)}`);
    assert.equal(res.location, "/app/settings?error=bad_state&provider=slack");
    res = await ctx.call("GET", `/api/integrations/linear/callback?code=good&state=${encodeURIComponent(state)}`);
    assert.equal(res.location, "/app/settings?error=bad_state&provider=linear", "a slack state cannot finish a linear connect");
    res = await ctx.call("GET", "/api/integrations/slack/callback?error=access_denied");
    assert.equal(res.location, "/app/settings?error=cancelled&provider=slack");
    res = await ctx.call("GET", `/api/integrations/slack/callback?code=bad&state=${encodeURIComponent(state)}`);
    assert.equal(res.location, "/app/settings?error=exchange_failed&provider=slack");
    ctx.clock.t += 11 * 60 * 1000;
    res = await ctx.call("GET", `/api/integrations/slack/callback?code=good&state=${encodeURIComponent(state)}`);
    assert.equal(res.location, "/app/settings?error=bad_state&provider=slack");
    assert.equal(ctx.database.prepare("select count(*) as n from rudder_integrations").get().n, 0);
    assert.equal(typeof verifyState, "function");
  } finally {
    await ctx.close();
  }
});

test("without RUDDER_SECRETS_KEY or client ids connect is a 503", async () => {
  const noKey = await setup({ env: { ...ENV, RUDDER_SECRETS_KEY: "" } });
  try {
    const res = await noKey.call("POST", "/api/app/integrations/slack/start", { account: "alice", body: {} });
    assert.equal(res.status, 503);
    assert.match(res.data.error, /RUDDER_SECRETS_KEY/);
  } finally {
    await noKey.close();
  }
  const unconfigured = await setup({ env: { RUDDER_SECRETS_KEY: "k" } });
  try {
    const summary = await unconfigured.call("GET", "/api/app/integrations", { account: "alice" });
    assert.equal(summary.data.slack.configured, false);
    assert.equal(summary.data.linear.configured, false);
    assert.equal((await unconfigured.call("POST", "/api/app/integrations/linear/start", { account: "alice", body: {} })).status, 503);
  } finally {
    await unconfigured.close();
  }
});

test("org integrations: admins connect, members read, outsiders get 404", async () => {
  const ctx = await setup();
  try {
    assert.equal((await ctx.call("POST", "/api/app/integrations/slack/start", { account: "bob", body: { org: "org1" } })).status, 403);
    assert.equal((await ctx.call("POST", "/api/app/integrations/slack/start", { account: "eve", body: { org: "org1" } })).status, 404);
    const { cb } = await connectSlack(ctx, "alice", "org1");
    assert.equal(cb.location, "/app/settings?connected=slack&org=org1");
    const asMember = await ctx.call("GET", "/api/app/integrations?org=org1", { account: "bob" });
    assert.equal(asMember.data.slack.connected, true);
    assert.equal(asMember.data.canManage, false);
    assert.equal((await ctx.call("GET", "/api/app/integrations?org=org1", { account: "eve" })).status, 404);
    assert.equal((await ctx.call("DELETE", "/api/app/integrations/slack?org=org1", { account: "bob" })).status, 403);
    assert.equal((await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "bob", body: { org: "org1", channel: "C1" } })).status, 403);
    // The org connection is not alice's personal one.
    assert.equal((await ctx.call("GET", "/api/app/integrations", { account: "alice" })).data.slack.connected, false);
  } finally {
    await ctx.close();
  }
});

test("linear: connect, list issues assigned first, start one as a task and comment", async () => {
  const ctx = await setup();
  try {
    const start = await ctx.call("POST", "/api/app/integrations/linear/start", { account: "alice", body: {} });
    const authorize = new URL(start.data.url);
    assert.equal(authorize.origin + authorize.pathname, "https://linear.app/oauth/authorize");
    assert.equal(authorize.searchParams.get("scope"), "read,write,issues:create,comments:create");
    const state = authorize.searchParams.get("state");
    const cb = await ctx.call("GET", `/api/integrations/linear/callback?code=c&state=${encodeURIComponent(state)}`);
    assert.equal(cb.location, "/app/settings?connected=linear");
    const dump = JSON.stringify(ctx.database.prepare("select * from rudder_integrations").all());
    assert.ok(!dump.includes("lin_SECRET_ACCESS") && !dump.includes("lin_refresh"));
    assert.deepEqual((await ctx.call("GET", "/api/app/integrations", { account: "alice" })).data.linear, {
      configured: true, connected: true, workspace: { name: "Acme Linear" },
    });

    const issues = await ctx.call("GET", "/api/app/linear/issues", { account: "alice" });
    assert.deepEqual(issues.data.issues.map((i) => i.identifier), ["ENG-7", "ENG-8"]);
    assert.deepEqual(issues.data.issues[0], { id: "iss-1", identifier: "ENG-7", title: "Fix login", url: "https://linear.app/x/ENG-7", state: "Todo", priority: 1, team: "ENG" });
    const searched = await ctx.call("GET", "/api/app/linear/issues?q=login", { account: "alice" });
    assert.deepEqual(searched.data.issues.map((i) => i.identifier), ["ENG-7", "ENG-9"]);

    assert.equal((await ctx.call("POST", "/api/app/linear/start", { account: "alice", body: { issueId: "iss-1", workspace: "nope" } })).status, 404);
    const started = await ctx.call("POST", "/api/app/linear/start", { account: "alice", body: { issueId: "iss-1", workspace: "ws1" } });
    assert.equal(started.status, 200, JSON.stringify(started.data));
    assert.equal(started.data.text, "Linear ENG-7: Fix login\n\nIt breaks on Safari.\n\nhttps://linear.app/acme/issue/ENG-7");
    assert.deepEqual(ctx.delivered, [{ accountId: "alice", workspaceId: "ws1", text: started.data.text }]);
    const comment = ctx.upstream.calls.find((c) => c.body.includes("commentCreate"));
    assert.deepEqual(JSON.parse(comment.body).variables.input, { issueId: "iss-1", body: "Rudder started an agent on this in rudder." });
    assert.ok(ctx.events.some((e) => e.name === "linear_issue_started"));

    // A token past its expiry is refreshed before use.
    ctx.clock.t += 2 * 86400 * 1000;
    await ctx.call("GET", "/api/app/linear/issues", { account: "alice" });
    const refresh = ctx.upstream.calls.filter((c) => c.url === "https://api.linear.app/oauth/token").pop();
    assert.equal(new URLSearchParams(refresh.body).get("grant_type"), "refresh_token");
  } finally {
    await ctx.close();
  }
});

test("linear task text trims a long description", () => {
  const text = linearTaskText({ identifier: "A-1", title: "T", url: "https://u", description: "x".repeat(5000) });
  assert.ok(text.startsWith("Linear A-1: T\n\n"));
  assert.ok(text.includes("[description truncated]"));
  assert.ok(text.endsWith("\n\nhttps://u"));
  assert.ok(text.length < 4100);
});

test("slash command: signature, link, membership, status and start", async () => {
  const ctx = await setup();
  try {
    const unsigned = await ctx.call("POST", "/api/integrations/slack/commands", {
      body: "text=status&team_id=T1&user_id=U1",
      headers: { "content-type": "application/x-www-form-urlencoded", "x-slack-request-timestamp": String(Math.floor(ctx.clock.t / 1000)), "x-slack-signature": "v0=deadbeef" },
    });
    assert.equal(unsigned.status, 401);

    let res = await signedCommand(ctx, { text: "status", team_id: "T1", user_id: "U1" });
    assert.match(res.data.text, /not connected to this Slack workspace/);

    await connectSlack(ctx, "alice", "org1");
    res = await signedCommand(ctx, { text: "status", team_id: "T1", user_id: "U1" });
    assert.equal(res.data.response_type, "ephemeral");
    assert.match(res.data.text, /\/rudder link/);

    res = await signedCommand(ctx, { text: "link", team_id: "T1", user_id: "U1", user_name: "eve" });
    const code = res.data.text.match(/\/app\/link\/slack\/(\S+)/)[1];
    assert.ok(res.data.text.includes("https://cloud.test/app/link/slack/"));
    const peek = await ctx.call("GET", `/api/app/link/slack/${code}`, { account: "eve" });
    assert.deepEqual(peek.data.team, { id: "T1", name: "Acme" });
    assert.equal((await ctx.call("POST", `/api/app/link/slack/${code}`, { account: "eve" })).status, 200);
    assert.equal((await ctx.call("POST", `/api/app/link/slack/${code}`, { account: "eve" })).status, 404, "codes are single use");

    // eve is linked but not in org1, which owns the install.
    res = await signedCommand(ctx, { text: "status", team_id: "T1", user_id: "U1" });
    assert.match(res.data.text, /not in the org/);

    res = await signedCommand(ctx, { text: "link", team_id: "T1", user_id: "U2" });
    const bobCode = res.data.text.match(/\/app\/link\/slack\/(\S+)/)[1];
    ctx.clock.t += 11 * 60 * 1000;
    assert.equal((await ctx.call("POST", `/api/app/link/slack/${bobCode}`, { account: "bob" })).status, 410);
    res = await signedCommand(ctx, { text: "link", team_id: "T1", user_id: "U2" });
    const bobCode2 = res.data.text.match(/\/app\/link\/slack\/(\S+)/)[1];
    await ctx.call("POST", `/api/app/link/slack/${bobCode2}`, { account: "bob" });

    ctx.setProjects([
      { workspaceId: "ws1", name: "rudder", repo: "exla-ai/rudder", agents: [
        { id: "r1", title: "Fix tests", status: "waiting" },
        { id: "r2", title: "Busy", status: "working" },
      ] },
      { workspaceId: "ws2", name: "site", repo: "exla-ai/site", agents: [] },
    ]);
    res = await signedCommand(ctx, { text: "status", team_id: "T1", user_id: "U2" });
    assert.match(res.data.text, /Fix tests/);
    assert.ok(!res.data.text.includes("Busy"));
    assert.ok(res.data.text.includes("https://cloud.test/app/p/ws1/a/r1"));

    res = await signedCommand(ctx, { text: "nothing do it", team_id: "T1", user_id: "U2" });
    assert.match(res.data.text, /No project called/);
    assert.match(res.data.text, /`site`/);

    res = await signedCommand(ctx, { text: "SI ship the landing page", team_id: "T1", user_id: "U2" });
    assert.match(res.data.text, /Starting on \*site\*/);
    await ctx.integrations.idle();
    assert.deepEqual(ctx.delivered, [{ accountId: "bob", workspaceId: "ws2", text: "ship the landing page" }]);
    const followUp = ctx.upstream.calls.find((c) => c.url.startsWith("https://hooks.slack.com/"));
    assert.match(JSON.parse(followUp.body).text, /Started an agent on \*site\*/);

    res = await signedCommand(ctx, { text: "", team_id: "T1", user_id: "U2" });
    assert.match(res.data.text, /\/rudder <project> <task>/);
  } finally {
    await ctx.close();
  }
});

test("project matching and slash parsing", () => {
  const projects = [
    { workspaceId: "a", name: "rudder", repo: "exla-ai/rudder" },
    { workspaceId: "b", name: "rudder-site", repo: "exla-ai/rudder-site" },
    { workspaceId: "c", name: "api", repo: "acme/backend" },
  ];
  assert.equal(matchProject(projects, "Rudder").match.workspaceId, "a");
  assert.equal(matchProject(projects, "exla-ai/rudder-site").match.workspaceId, "b");
  assert.equal(matchProject(projects, "backend").match.workspaceId, "c");
  assert.equal(matchProject(projects, "rudder-s").match.workspaceId, "b");
  assert.equal(matchProject(projects, "rud").match, undefined);
  assert.equal(matchProject(projects, "rud").ambiguous.length, 2);
  assert.deepEqual(parseSlashCommand("rudder fix it now"), { action: "start", project: "rudder", task: "fix it now" });
  assert.deepEqual(parseSlashCommand("rudder"), { action: "usage", project: "rudder" });
  assert.deepEqual(parseSlashCommand(" LINK "), { action: "link" });
});

test("notifications: baseline first, one post per transition, deduped", async () => {
  const ctx = await setup();
  try {
    await connectSlack(ctx, "alice");
    await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "alice", body: { channel: "C1" } });
    await connectSlack(ctx, "alice", "org1");
    await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "alice", body: { org: "org1", channel: "C2" } });
    const posts = () => ctx.upstream.calls.filter((c) => c.url.includes("chat.postMessage")).map((c) => JSON.parse(c.body));
    const state = (agents) => [{ workspaceId: "ws1", name: "rudder", agents }];

    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "waiting" }]));
    assert.equal(posts().length, 0, "the first state seen is a baseline");

    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "working" }]));
    assert.equal(posts().length, 0);
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "review" }]));
    assert.deepEqual(posts().map((p) => p.channel).sort(), ["C1", "C2"], "personal and org channels both get it");
    assert.match(posts()[0].text, /\*rudder\* · Fix tests ready for review/);
    assert.ok(posts()[0].text.includes("<https://cloud.test/app/p/ws1/a/r1|Open in Rudder>"));

    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "review" }]));
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "done" }]));
    assert.equal(posts().length, 2, "same status (review/done are both ready) is not reposted");

    // A pending ask counts as needs-you even while the status says working.
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "working", asks: [{ id: "a" }] }]));
    assert.equal(posts().length, 4);
    assert.match(posts()[2].text, /needs you/);

    // A new agent that arrives failing is news; a second account is its own baseline.
    await ctx.integrations.onHomeState("alice", state([{ id: "r2", title: "Deploy", status: "failed" }]));
    assert.equal(posts().length, 6);
    await ctx.integrations.onHomeState("bob", state([{ id: "r9", title: "x", status: "failed" }]));
    assert.equal(posts().length, 6);

    // A private agent's title goes to the person's own channel only; the org
    // channel (teammates read it) says "Private agent".
    await ctx.integrations.onHomeState("alice", state([{ id: "r3", title: "Secret plan", status: "working", visibility: "private" }]));
    await ctx.integrations.onHomeState("alice", state([{ id: "r3", title: "Secret plan", status: "failed", visibility: "private" }]));
    const privatePosts = posts().slice(6);
    assert.equal(privatePosts.length, 2);
    assert.match(privatePosts.find((p) => p.channel === "C1").text, /Secret plan/);
    assert.ok(!privatePosts.find((p) => p.channel === "C2").text.includes("Secret plan"));
    assert.match(privatePosts.find((p) => p.channel === "C2").text, /Private agent/);

    // Slack errors are swallowed.
    ctx.upstream.calls.length = 0;
    const broken = createIntegrations({
      database: ctx.database, env: ENV, fetch: async () => { throw new Error("network down"); }, baseURL: "https://cloud.test",
      orgRole: () => null, orgIdsForAccount: () => [], listProjects: () => [], deliverTask: async () => {}, log: () => {},
    });
    await broken.onHomeState("alice", state([{ id: "r1", status: "working" }]));
    await broken.onHomeState("alice", state([{ id: "r1", status: "failed" }]));
  } finally {
    await ctx.close();
  }
});

test("notifications: a needs-you blip shorter than the hold never posts; one that stays does, with its reason", async () => {
  const ctx = await setup({ needsDebounceMs: 150 });
  try {
    await connectSlack(ctx, "alice");
    await ctx.call("POST", "/api/app/integrations/slack/channel", { account: "alice", body: { channel: "C1" } });
    const posts = () => ctx.upstream.calls.filter((c) => c.url.includes("chat.postMessage")).map((c) => JSON.parse(c.body));
    const state = (agents) => [{ workspaceId: "ws1", name: "rudder", agents }];
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "working" }]));
    // An Esc interrupt flashes needs-you, then the next turn starts.
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "waiting", attention: "interrupted", needsYou: true, reason: "interrupted" }]));
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "working" }]));
    await ctx.integrations.idle();
    await new Promise((r) => setTimeout(r, 250));
    assert.equal(posts().length, 0, "the blip was not news");
    // A permission prompt that stays past the hold is.
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "waiting", needsYou: true, reason: "permission" }]));
    assert.equal(posts().length, 0, "held, not posted at once");
    await ctx.integrations.idle();
    assert.equal(posts().length, 1);
    assert.match(posts()[0].text, /Fix tests needs you: needs your permission/);
    // An open PR is done, not needs-you.
    await ctx.integrations.onHomeState("alice", state([{ id: "r1", title: "Fix tests", status: "review", needsYou: false, reason: null, pr: { state: "open" } }]));
    assert.match(posts()[1].text, /Fix tests done, PR open/);
  } finally {
    await ctx.close();
  }
});
