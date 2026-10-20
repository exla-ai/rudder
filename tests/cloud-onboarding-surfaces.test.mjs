// Onboarding surfaces (cloud/src/onboarding.ts) and product analytics
// (cloud/src/product-analytics.ts): validation, storage and the event
// whitelist + sanitizer, with an injected fetch so nothing leaves the machine.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const built =
  fs.existsSync(path.join(cloudDir, "dist", "onboarding.js")) &&
  fs.existsSync(path.join(cloudDir, "dist", "product-analytics.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = built
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const onboarding = built ? await import(path.join(cloudDir, "dist", "onboarding.js")) : null;
const analytics = built ? await import(path.join(cloudDir, "dist", "product-analytics.js")) : null;
const Database = built ? createRequire(path.join(cloudDir, "package.json"))("better-sqlite3") : null;

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

test("a fresh account has no surfaces and has not finished", () => {
  const store = onboarding.createOnboardingStore(new Database(":memory:"));
  assert.deepEqual(store.onboardingSummary("a"), { surfaces: [], completedAt: null });
});

test("surfaces are validated, deduped and kept in canonical order", () => {
  assert.deepEqual(onboarding.normalizeSurfaces(["phone", "web", "phone", "terminal"]), ["web", "terminal", "phone"]);
  assert.deepEqual(onboarding.normalizeSurfaces([]), []);
  assert.throws(() => onboarding.normalizeSurfaces(["web", "watch"]), (e) => e.status === 400);
  assert.throws(() => onboarding.normalizeSurfaces("web"), (e) => e.status === 400);
  assert.throws(() => onboarding.normalizeSurfaces([1]), (e) => e.status === 400);
});

test("complete stamps completedAt once and cannot be undone", () => {
  let clock = Date.parse("2026-10-20T12:00:00Z");
  const store = onboarding.createOnboardingStore(new Database(":memory:"), { now: () => clock });
  assert.deepEqual(store.updateOnboarding("a", { surfaces: ["mac", "web"] }), { surfaces: ["web", "mac"], completedAt: null });
  const done = store.updateOnboarding("a", { complete: true });
  assert.deepEqual(done, { surfaces: ["web", "mac"], completedAt: "2026-10-20T12:00:00.000Z" });
  clock += 60_000;
  const again = store.updateOnboarding("a", { surfaces: ["terminal"], complete: true });
  assert.deepEqual(again, { surfaces: ["terminal"], completedAt: "2026-10-20T12:00:00.000Z" });
  assert.throws(() => store.updateOnboarding("a", { complete: false }), (e) => e.status === 400);
  assert.throws(() => store.updateOnboarding("a", { complete: "yes" }), (e) => e.status === 400);
  assert.equal(store.onboardingSummary("a").completedAt, "2026-10-20T12:00:00.000Z");
  assert.deepEqual(store.onboardingSummary("b"), { surfaces: [], completedAt: null });
});

test("handleOnboardingApi routes GET and POST and reports errors as JSON", async () => {
  const store = onboarding.createOnboardingStore(new Database(":memory:"));
  const captured = [];
  const deps = { store, sendJson, readJsonBody, capture: (id, name, props) => captured.push({ id, name, props }) };
  const account = { accountId: "a" };
  const url = (p) => new URL(`http://x${p}`);

  assert.equal(await onboarding.handleOnboardingApi(req("GET"), fakeRes(), url("/api/app/orgs"), account, deps), false);

  let res = fakeRes();
  assert.equal(await onboarding.handleOnboardingApi(req("GET"), res, url("/api/app/onboarding"), account, deps), true);
  assert.deepEqual([res.status, res.body], [200, { surfaces: [], completedAt: null }]);

  res = fakeRes();
  await onboarding.handleOnboardingApi(req("POST", { surfaces: ["web", "nope"] }), res, url("/api/app/onboarding"), account, deps);
  assert.equal(res.status, 400);
  assert.match(res.body.error, /unknown surface/);

  res = fakeRes();
  await onboarding.handleOnboardingApi(req("POST", "BAD"), res, url("/api/app/onboarding"), account, deps);
  assert.equal(res.status, 400);

  res = fakeRes();
  await onboarding.handleOnboardingApi(req("POST", { surfaces: ["phone", "web"], complete: true }), res, url("/api/app/onboarding"), account, deps);
  assert.equal(res.status, 200);
  assert.deepEqual(res.body.surfaces, ["web", "phone"]);
  assert.ok(res.body.completedAt);
  assert.deepEqual(captured, [
    { id: "a", name: "onboarding_completed", props: { surface_count: 2, surface_web: true, surface_mac: false, surface_terminal: false, surface_phone: true } },
  ]);

  // Completing again does not re-fire the event.
  await onboarding.handleOnboardingApi(req("POST", { complete: true }), fakeRes(), url("/api/app/onboarding"), account, deps);
  assert.equal(captured.length, 1);

  res = fakeRes();
  await onboarding.handleOnboardingApi(req("DELETE"), res, url("/api/app/onboarding"), account, deps);
  assert.equal(res.status, 405);
});

// Product analytics.

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
  const pa = analytics.createProductAnalytics({ env: { RUDDER_ANALYTICS_URL: "http://catch.test/" }, fetch: fn });
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
  assert.equal(body.properties.backend, "codex");
  assert.equal(body.properties.path, undefined);
  assert.ok(calls[0].signal);
  assert.equal(await pa.capture("acct-1", "not_an_event", {}), false);
  assert.equal(calls.length, 1);
});

test("capture is off under tests, with RUDDER_TELEMETRY=0, and never throws", async () => {
  const { fn, calls } = fakeFetch();
  const underTest = analytics.createProductAnalytics({ env: { NODE_ENV: "test" }, fetch: fn });
  assert.equal(underTest.enabled, false);
  assert.equal(await underTest.capture("a", "task_started"), false);
  const off = analytics.createProductAnalytics({ env: { RUDDER_TELEMETRY: "0", RUDDER_ANALYTICS_URL: "http://catch.test" }, fetch: fn });
  assert.equal(off.enabled, false);
  assert.equal(await off.capture("a", "task_started"), false);
  assert.equal(calls.length, 0);
  // Production default host when nothing overrides it.
  const prod = analytics.createProductAnalytics({ env: {}, fetch: fn });
  assert.equal(prod.enabled, true);
  await prod.capture("a", "task_started");
  assert.equal(calls[0].url, "https://us.i.posthog.com/i/v0/e/");
  const broken = analytics.createProductAnalytics({
    env: { RUDDER_ANALYTICS_URL: "http://catch.test" },
    fetch: async () => {
      throw new Error("network down");
    },
  });
  assert.equal(await broken.capture("a", "task_started"), false);
});

test("handleEventApi answers 204 for known events and 400 for unknown ones", async () => {
  const { fn, calls } = fakeFetch();
  const pa = analytics.createProductAnalytics({ env: { RUDDER_ANALYTICS_URL: "http://catch.test" }, fetch: fn });
  const deps = { sendJson, readJsonBody };
  const account = { accountId: "acct" };
  const url = (p) => new URL(`http://x${p}`);

  assert.equal(await pa.handleEventApi(req("POST"), fakeRes(), url("/api/app/onboarding"), account, deps), false);

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
  await pa.handleEventApi(req("POST", "BAD"), res, url("/api/app/event"), account, deps);
  assert.equal(res.status, 400);

  res = fakeRes();
  await pa.handleEventApi(req("GET"), res, url("/api/app/event"), account, deps);
  assert.equal(res.status, 405);
  assert.equal(calls.length, 1);
});

test("every contract event name is whitelisted", () => {
  const names = [
    "onboarding_viewed", "onboarding_surface_toggled", "onboarding_completed", "org_created", "org_invite_created",
    "org_invite_accepted", "org_member_role_changed", "org_member_removed", "integration_connect_clicked",
    "integration_connected", "integration_disconnected", "model_picked", "agent_panel_opened", "plan_panel_opened",
    "task_started", "download_mac_clicked", "install_cli_copied", "phone_link_opened", "linear_issue_started",
  ];
  assert.deepEqual([...analytics.PRODUCT_EVENTS].sort(), [...names].sort());
});
