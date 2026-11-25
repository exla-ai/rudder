// Product analytics (cloud/src/product-analytics.ts): the event whitelist, the
// props sanitizer, the internal flag, device events before sign-in and the
// cloud machine watch, with an injected fetch so nothing leaves the machine.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built = fs.existsSync(path.join(cloudDir, "dist", "product-analytics.js"));
const test = built
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const analytics = built ? await import(path.join(cloudDir, "dist", "product-analytics.js")) : null;

function fakeRes() {
  return {
    status: null,
    body: undefined,
    writeHead(status) {
      this.status = status;
    },
    end() {},
  };
}
const sendJson = (res, status, body) => {
  res.status = status;
  res.body = body;
};
const req = (method, body) => ({ method, body });
const readJsonBody = async (r) => {
  if (r.body === "BAD") throw new SyntaxError("bad json");
  return r.body ?? {};
};

const noTeam = () => false;

function fakeFetch() {
  const calls = [];
  const fn = async (url, init) => {
    calls.push({ url, body: JSON.parse(init.body), signal: init.signal });
    return { ok: true };
  };
  return { fn, calls };
}

test("sanitizeProductProps keeps primitives and drops paths, tokens and emails", () => {
  const props = analytics.sanitizeProductProps({
    backend: "claude",
    model: "claude-opus-4-5",
    count: 3,
    ok: true,
    none: null,
    path: "/Users/viraat/code/rudder",
    home: "~/code",
    win: "C:\\Users\\x",
    rel: "../secrets",
    email: "viraat@exla.ai",
    gh: "ghp_abcdefghijklmnop",
    rdr: "rdr_abc123",
    slack: "xoxb-123-456",
    longToken: "A".repeat(40),
    creds: "https://user:pass@example.com",
    nested: { a: 1 },
    list: [1, 2],
    nan: Number.NaN,
    "bad key": "x",
    fn: () => 1,
  });
  assert.deepEqual(props, { backend: "claude", model: "claude-opus-4-5", count: 3, ok: true, none: null });
  assert.deepEqual(analytics.sanitizeProductProps("nope"), {});
  assert.deepEqual(analytics.sanitizeProductProps(null), {});
  const many = Object.fromEntries(Array.from({ length: 30 }, (_, i) => [`k${i}`, i]));
  assert.equal(Object.keys(analytics.sanitizeProductProps(many)).length, 20);
  assert.equal(analytics.sanitizeProductProps({ s: "word ".repeat(100) }).s.length, 200);
});

test("capture posts a whitelisted event to PostHog with a hashed id", async () => {
  const { fn, calls } = fakeFetch();
  const pa = analytics.createProductAnalytics({ env: { RUDDER_ANALYTICS_URL: "http://catch.test/" }, fetch: fn, internal: (id) => id === "team-1" });
  assert.equal(pa.enabled, true);
  assert.equal(await pa.capture("acct-1", "model_picked", { backend: "codex", path: "/etc/passwd" }), true);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "http://catch.test/i/v0/e/");
  const body = calls[0].body;
  assert.equal(body.event, "model_picked");
  assert.match(body.api_key, /^phc_/);
  assert.equal(body.distinct_id, analytics.analyticsDistinctId("acct-1"));
  assert.notEqual(body.distinct_id, "acct-1");
  assert.match(body.distinct_id, /^[0-9a-f]{24}$/);
  assert.equal(body.properties.$process_person_profile, false);
  assert.equal(body.properties.internal, false);
  assert.equal(body.properties.backend, "codex");
  assert.equal(body.properties.path, undefined);
  assert.ok(calls[0].signal);
  assert.equal(await pa.capture("acct-1", "not_an_event", {}), false);
  assert.equal(calls.length, 1);
  // The server decides `internal`; a prop of that name cannot override it.
  await pa.capture("team-1", "model_picked", { internal: false });
  assert.equal(calls[1].body.properties.internal, true);
  await pa.capture("acct-1", "model_picked", { internal: true });
  assert.equal(calls[2].body.properties.internal, false);
});

test("isInternalEmail: exla.ai addresses and admins only", () => {
  const admins = new Set(["owner@example.com"]);
  assert.equal(analytics.isInternalEmail("Someone@Exla.ai", admins), true);
  assert.equal(analytics.isInternalEmail("owner@example.com", admins), true);
  assert.equal(analytics.isInternalEmail("someone@exla.ai.evil.com", admins), false);
  assert.equal(analytics.isInternalEmail("someone@notexla.ai", admins), false);
  assert.equal(analytics.isInternalEmail(null, admins), false);
});

test("capture is off under tests, with RUDDER_TELEMETRY=0, and never throws", async () => {
  const { fn, calls } = fakeFetch();
  const underTest = analytics.createProductAnalytics({ env: { NODE_ENV: "test" }, fetch: fn, internal: noTeam });
  assert.equal(underTest.enabled, false);
  assert.equal(await underTest.capture("a", "task_started"), false);
  const off = analytics.createProductAnalytics({ env: { RUDDER_TELEMETRY: "0", RUDDER_ANALYTICS_URL: "http://catch.test" }, fetch: fn, internal: noTeam });
  assert.equal(off.enabled, false);
  assert.equal(await off.capture("a", "task_started"), false);
  assert.equal(calls.length, 0);
  // Production default host when nothing overrides it.
  const prod = analytics.createProductAnalytics({ env: {}, fetch: fn, internal: noTeam });
  assert.equal(prod.enabled, true);
  await prod.capture("a", "task_started");
  assert.equal(calls[0].url, "https://us.i.posthog.com/i/v0/e/");
  const broken = analytics.createProductAnalytics({
    internal: noTeam,
    env: { RUDDER_ANALYTICS_URL: "http://catch.test" },
    fetch: async () => {
      throw new Error("network down");
    },
  });
  assert.equal(await broken.capture("a", "task_started"), false);
});

test("handleEventApi answers 204 for known events and 400 for unknown ones", async () => {
  const { fn, calls } = fakeFetch();
  const pa = analytics.createProductAnalytics({ env: { RUDDER_ANALYTICS_URL: "http://catch.test" }, fetch: fn, internal: noTeam });
  const deps = { sendJson, readJsonBody };
  const account = { accountId: "acct" };
  const url = (p) => new URL(`http://x${p}`);

  assert.equal(await pa.handleEventApi(req("POST"), fakeRes(), url("/api/app/orgs"), account, deps), false);

  let res = fakeRes();
  assert.equal(await pa.handleEventApi(req("POST", { name: "agent_panel_opened", props: { tab: "diff" } }), res, url("/api/app/event"), account, deps), true);
  assert.equal(res.status, 204);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(calls.length, 1);
  assert.equal(calls[0].body.properties.tab, "diff");

  res = fakeRes();
  await pa.handleEventApi(req("POST", { name: "drop_tables" }), res, url("/api/app/event"), account, deps);
  assert.deepEqual([res.status, res.body], [400, { error: "unknown event" }]);

  res = fakeRes();
  await pa.handleEventApi(req("POST", { name: "voice_call_started" }), res, url("/api/app/event"), account, deps);
  assert.equal(res.status, 400, "a client cannot post an event the server counts itself");

  res = fakeRes();
  await pa.handleEventApi(req("POST", "BAD"), res, url("/api/app/event"), account, deps);
  assert.equal(res.status, 400);

  res = fakeRes();
  await pa.handleEventApi(req("GET"), res, url("/api/app/event"), account, deps);
  assert.equal(res.status, 405);
  assert.equal(calls.length, 1);
});

test("every contract event name is whitelisted", () => {
  const names = [
    "tutorial_viewed", "tutorial_step", "tutorial_completed", "tutorial_skipped", "integration_connect_clicked",
    "model_picked", "agent_panel_opened", "plan_panel_opened",
    "download_mac_clicked", "install_cli_copied", "phone_link_opened", "chat_model_switched",
    "all_agents_started", "feature_used", "screen_viewed", "session_started", "session_ended", "client_error",
  ];
  const serverOnly = [
    "app_active", "app_error", "message_sent", "message_edited", "prompt_answered", "agent_action", "agent_branched",
    "chat_harness_switched", "plan_approved", "agent_pulled", "voice_session_started", "voice_tool_used", "voice_call_started", "voice_call_ended",
    "signed_in", "home_created", "home_ready", "agent_login_started", "agent_signed_in", "agent_started",
    "message_failed", "task_started", "integration_connected", "integration_disconnected",
    "org_created", "org_invite_created", "org_invite_accepted", "org_member_role_changed", "org_member_removed",
  ];
  assert.deepEqual([...analytics.PRODUCT_EVENTS].sort(), [...names, ...serverOnly].sort());
  assert.deepEqual([...analytics.SERVER_ONLY_EVENTS].sort(), [...serverOnly].sort());
});

test("error routes keep their shape and lose ids; long model ids are kept", () => {
  assert.equal(analytics.routeTemplate("/api/app/send"), "send");
  assert.equal(analytics.routeTemplate("/api/app/pair/finish"), "pair/finish");
  assert.equal(analytics.routeTemplate("/api/app/link/slack/AbC123xyz_9"), "link/slack");
  assert.equal(analytics.routeTemplate("/api/app/orgs/org_8f2a/members"), "orgs/:id");
  assert.equal(analytics.routeTemplate("/api/app/"), "app");
  const kept = analytics.sanitizeProductProps({
    route: analytics.routeTemplate("/api/app/integrations/slack/channels"),
    model: "openrouter/anthropic/claude-sonnet-4.5",
    other: "openrouter/anthropic/claude-sonnet-4.5",
    via: "slack",
  });
  assert.deepEqual(kept, { route: "integrations/slack", model: "openrouter/anthropic/claude-sonnet-4.5", via: "slack" });
  assert.equal(analytics.sanitizeProductProps({ model: "sk-ant-0123456789abcdef0123456789abcdef" }).model, undefined);
  assert.equal(analytics.reasonCode("access_denied"), "access_denied");
  assert.equal(analytics.reasonCode("Someone typed this"), "other");
});

test("handleAnonEventApi takes device events with a uuid, hashed, within a budget", async () => {
  const { fn, calls } = fakeFetch();
  const pa = analytics.createProductAnalytics({ env: { RUDDER_ANALYTICS_URL: "http://catch.test" }, fetch: fn, internal: noTeam });
  const deps = { sendJson, readJsonBody };
  const url = new URL("http://x/api/app/anon-event");
  const device = "0b6f6a3e-4c1d-4d2a-9a51-2f0e8c7d1b23";

  assert.equal(await pa.handleAnonEventApi(req("POST"), fakeRes(), new URL("http://x/api/app/event"), deps), false);
  let res = fakeRes();
  await pa.handleAnonEventApi(req("POST", { name: "app_first_opened", device, props: { platform: "browser" } }), res, url, deps);
  assert.equal(res.status, 204);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(calls.length, 1);
  assert.equal(calls[0].body.event, "app_first_opened");
  assert.equal(calls[0].body.distinct_id, analytics.anonymousDistinctId(device));
  assert.doesNotMatch(calls[0].body.distinct_id, new RegExp(device));
  assert.equal(calls[0].body.properties.platform, "browser");
  assert.equal(calls[0].body.properties.internal, undefined);

  for (const [body, status] of [
    [{ name: "model_picked", device }, 400],
    [{ name: "signin_started", device }, 400],
    [{ name: "signin_viewed", device: "not-a-uuid" }, 400],
    [{ name: "signin_viewed" }, 400],
  ]) {
    res = fakeRes();
    await pa.handleAnonEventApi(req("POST", body), res, url, deps);
    assert.equal(res.status, status, JSON.stringify(body));
  }
  for (let i = 1; i < 30; i += 1) {
    await pa.handleAnonEventApi(req("POST", { name: "signin_viewed", device }), fakeRes(), url, deps);
  }
  res = fakeRes();
  await pa.handleAnonEventApi(req("POST", { name: "signin_viewed", device }), res, url, deps);
  assert.equal(res.status, 429);
  // The server's own anonymous event, keyed by a login attempt.
  assert.equal(await pa.captureAnonymous("login-1", "signin_started", { platform: "mac" }), true);
  assert.equal(await pa.captureAnonymous("login-1", "model_picked", {}), false);
});

test("createHomeWatch counts new agents and new agent sign-ins, not what a restart already knew", () => {
  const events = [];
  let clock = Date.parse("2026-11-20T12:00:00Z");
  const watch = analytics.createHomeWatch((accountId, name, props) => events.push({ accountId, name, props }), () => clock);
  const oldHome = clock - 5 * 24 * 3600_000;
  const agent = (id, extra = {}) => ({ id, backend: "codex", model: "gpt-5.5", effort: null, account: "main", createdAt: clock - 3600_000, ...extra });
  const login = (id, ok = true) => ({ id, ok, plan: "max", account: "someone@example.com" });

  // First state after a restart: old agents and old sign-ins are seeded quietly;
  // an agent started two minutes ago is counted (the old process may have missed it).
  watch("home-1", "acct", {
    projects: [{ agents: [agent("a1"), agent("a2", { createdAt: clock - 120_000, backend: "claude", model: "claude-opus-5-5" })] }],
    credentials: { claude: [login("main")], codex: [] },
    homeCreatedAt: oldHome,
  });
  assert.deepEqual(events.map((e) => [e.name, e.props.harness, e.props.model]), [["agent_started", "claude", "claude-opus-5-5"]]);
  events.length = 0;

  clock += 60_000;
  watch("home-1", "acct", {
    projects: [
      { agents: [agent("a1"), agent("a2"), agent("a3", { account: "acct-2", planId: "p1" })] },
      { coordinator: true, agents: [agent("c1", { backend: "claude", model: null })] },
    ],
    credentials: { claude: [login("main"), login("acct-2")], codex: [login("main")], opencode: [login("main", false)] },
    homeCreatedAt: oldHome,
  });
  assert.deepEqual(events.map((e) => e.name), ["agent_started", "agent_started", "agent_signed_in", "agent_signed_in"]);
  const [a3, c1, claude2, codex] = events.map((e) => e.props);
  assert.deepEqual(a3, { harness: "codex", model: "gpt-5.5", effort: "auto", mode: "default", account: "added", planned: true, all_agents: false, agents: 4, projects: 1 });
  assert.equal(c1.all_agents, true);
  assert.equal(c1.model, "default");
  assert.deepEqual(claude2, { harness: "claude", account: "added", plan: "max", accounts: 2 });
  assert.equal(codex.harness, "codex");
  assert.ok(events.every((e) => e.accountId === "acct" && !JSON.stringify(e.props).includes("@")));
  events.length = 0;

  // Same state again, or a check in progress (credentials null): nothing new.
  watch("home-1", "acct", { projects: [{ agents: [agent("a1")] }], credentials: null, homeCreatedAt: oldHome });
  assert.equal(events.length, 0);

  // A machine created minutes ago announces the sign-ins it came up with.
  watch("home-2", "acct-new", { projects: [], credentials: { claude: [login("main")] }, homeCreatedAt: clock - 120_000 });
  assert.deepEqual(events.map((e) => [e.name, e.accountId]), [["agent_signed_in", "acct-new"]]);
});
