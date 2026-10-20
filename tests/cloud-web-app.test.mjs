// The web app's new control-plane routes, through the REAL server: the surface
// picker, orgs and invites, analytics, the Slack/Linear surface without
// credentials, and the pull route without a cloud machine. The modules have
// their own unit tests; this one proves server.ts wires them and that two
// accounts see what they should of each other.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import http from "node:http";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.resolve(here, "..", "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "dist", "orgs.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = cloudReady
  ? nodeTest
  : (name, fn) =>
      nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const require = createRequire(path.join(cloudDir, "package.json"));
const Database = cloudReady ? require("better-sqlite3") : null;

const tokenHash = (token) => createHash("sha256").update(token).digest("hex").slice(0, 32);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function waitFor(fn, { timeout = 10000, interval = 100 } = {}) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // keep polling
    }
    if (Date.now() - start > timeout) throw new Error("waitFor timed out");
    await sleep(interval);
  }
}

test("web app routes: picker, orgs, invites, analytics, integrations, pull", async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cloud-web-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  // A stand-in PostHog: product events the server sends land here.
  const events = [];
  const sink = http.createServer((req, res) => {
    let body = "";
    req.on("data", (d) => { body += d; });
    req.on("end", () => {
      try { events.push(JSON.parse(body)); } catch { /* ignore */ }
      res.end("{}");
    });
  });
  await new Promise((r) => sink.listen(0, "127.0.0.1", r));
  t.after(() => sink.close());
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    env: {
      ...process.env,
      NODE_ENV: "test",
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      RUDDER_SECRETS_KEY: "",
      RUDDER_SLACK_CLIENT_ID: "",
      RUDDER_LINEAR_CLIENT_ID: "",
      SLACK_BOT_TOKEN: "",
      RUDDER_DIGEST: "0",
      RUDDER_ANALYTICS_URL: `http://127.0.0.1:${sink.address().port}`,
      RUDDER_TELEMETRY: "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let serverLog = "";
  child.stdout.on("data", (d) => { serverLog += d.toString(); });
  child.stderr.on("data", (d) => { serverLog += d.toString(); });
  t.after(() => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  const seed = db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)");
  seed.run(tokenHash("rdr_alice"), "acct-alice", "alice@example.com", now, now);
  seed.run(tokenHash("rdr_bob"), "acct-bob", "bob@example.com", now, now);
  db.close();

  const call = async (who, method, pathname, body) => {
    const res = await fetch(`${base}${pathname}`, {
      method,
      headers: { authorization: `Bearer rdr_${who}`, "content-type": "application/json", "x-rudder-platform": "mac" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* not json */ }
    return { status: res.status, json, text };
  };
  const ok = async (...args) => {
    const r = await call(...args);
    assert.ok(r.status >= 200 && r.status < 300, `${args[1]} ${args[2]} -> ${r.status} ${r.text}\n${serverLog.slice(-2000)}`);
    return r.json;
  };

  // Unauthenticated app calls are refused.
  assert.equal((await fetch(`${base}/api/app/orgs`)).status, 401);

  // Opening the app counts as active once a day per platform, never by a
  // passive poll, under an anonymous id.
  await ok("bob", "GET", "/api/app/state?passive=1");
  await ok("alice", "GET", "/api/app/state");
  await ok("alice", "GET", "/api/app/state");
  const active = await waitFor(() => events.filter((e) => e.event === "app_active").length ? events.filter((e) => e.event === "app_active") : null);
  await sleep(200);
  assert.equal(events.filter((e) => e.event === "app_active").length, 1, "once per day, not per poll");
  assert.equal(active[0].properties.platform, "mac");
  assert.doesNotMatch(JSON.stringify(active[0]), /alice|acct-/);

  // Surface picker: starts empty, saves a multi-select, completes once.
  let state = await ok("alice", "GET", "/api/app/state");
  assert.deepEqual(state.onboarding, { surfaces: [], completedAt: null });
  assert.deepEqual(state.orgs, []);
  await ok("alice", "POST", "/api/app/onboarding", { surfaces: ["phone", "web", "mac"] });
  const done = await ok("alice", "POST", "/api/app/onboarding", { complete: true });
  assert.deepEqual(done.surfaces, ["web", "mac", "phone"]);
  assert.ok(done.completedAt);
  assert.equal((await call("alice", "POST", "/api/app/onboarding", { surfaces: ["fax"] })).status, 400);

  // Analytics: whitelisted names are accepted, others refused.
  assert.equal((await call("alice", "POST", "/api/app/event", { name: "agent_panel_opened" })).status, 204);
  assert.equal((await call("alice", "POST", "/api/app/event", { name: "made_up_event" })).status, 400);

  // Orgs: Alice creates one and is its admin; Bob cannot see it.
  const { org } = await ok("alice", "POST", "/api/app/orgs", { name: "Exla" });
  assert.equal(org.role, "admin");
  assert.equal((await call("bob", "GET", `/api/app/orgs/${org.id}`)).status, 404);

  // Bob cannot invite himself; Alice invites, Bob previews and accepts.
  assert.ok((await call("bob", "POST", `/api/app/orgs/${org.id}/invites`, { role: "admin" })).status >= 400);
  const { invite } = await ok("alice", "POST", `/api/app/orgs/${org.id}/invites`, { role: "member" });
  const token = invite.url.split("/app/join/")[1];
  assert.ok(token);
  const preview = await ok("bob", "GET", `/api/app/invites/${token}`);
  assert.equal(preview.org.name, "Exla");
  const accepted = await ok("bob", "POST", `/api/app/invites/${token}/accept`);
  assert.equal(accepted.org.role, "member");
  assert.equal((await call("bob", "POST", `/api/app/invites/${token}/accept`)).status, 200, "accepting again is idempotent for a member");

  state = await ok("bob", "GET", "/api/app/state");
  assert.equal(state.orgs.length, 1);
  assert.equal(state.orgs[0].memberCount, 2);

  // A member sees the roster but not the invites, and cannot change roles.
  const bobView = await ok("bob", "GET", `/api/app/orgs/${org.id}`);
  assert.equal(bobView.role, "member");
  assert.deepEqual(bobView.invites, []);
  assert.equal(bobView.members.length, 2);
  assert.ok((await call("bob", "POST", `/api/app/orgs/${org.id}/members/acct-alice`, { role: "member" })).status >= 400);

  // The last admin cannot leave or demote themself.
  assert.equal((await call("alice", "DELETE", `/api/app/orgs/${org.id}/members/acct-alice`)).status, 409);
  assert.equal((await call("alice", "POST", `/api/app/orgs/${org.id}/members/acct-alice`, { role: "member" })).status, 409);
  // Promote Bob; now Alice may step down, and Bob may remove her.
  await ok("alice", "POST", `/api/app/orgs/${org.id}/members/acct-bob`, { role: "admin" });
  await ok("alice", "POST", `/api/app/orgs/${org.id}/members/acct-alice`, { role: "member" });
  await ok("bob", "DELETE", `/api/app/orgs/${org.id}/members/acct-alice`);
  assert.equal((await call("alice", "GET", `/api/app/orgs/${org.id}`)).status, 404);

  // Integrations without server credentials: readable, not connectable.
  const integ = await ok("bob", "GET", `/api/app/integrations?org=${org.id}`);
  assert.equal(integ.slack.configured, false);
  assert.equal(integ.linear.configured, false);
  assert.equal(integ.canManage, true);
  assert.equal((await call("bob", "POST", "/api/app/integrations/slack/start", { org: org.id })).status, 503);
  assert.equal((await call("alice", "GET", `/api/app/integrations?org=${org.id}`)).status, 404);

  // Slack's public endpoints need no session but do need a valid signature.
  const slash = await fetch(`${base}/api/integrations/slack/commands`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: "command=%2Frudder&text=status",
  });
  assert.ok(slash.status === 401 || slash.status === 503, `unsigned slash command -> ${slash.status}`);

  // The Mac menu bar's poll is passive: it must not count as activity on
  // the home machine (that would keep it from ever idling).
  const stale = "2026-01-01T00:00:00.000Z";
  const homeDb = new Database(dbPath);
  homeDb.prepare(
    "insert into rudder_workspaces (id, account_id, workspace_key, status, source_kind, last_activity_at, created_at, updated_at) values (?,?,?,?,?,?,?,?)",
  ).run("home-bob", "acct-bob", "home", "deleting", "home", stale, now, now);
  const activity = () => homeDb.prepare("select last_activity_at from rudder_workspaces where id = 'home-bob'").get().last_activity_at;
  await ok("bob", "GET", "/api/app/state?passive=1");
  assert.equal(activity(), stale, "a passive poll leaves the machine's activity alone");
  await ok("bob", "GET", "/api/app/state");
  assert.notEqual(activity(), stale, "opening the app counts as activity");
  homeDb.close();

  // Pull with no cloud machine says so instead of hanging.
  const pull = await call("alice", "POST", "/api/rudder/home/pull", { runId: "run-1", repo: "exla-ai/rudder" });
  assert.ok(pull.status >= 400 && pull.status < 500 || pull.status === 503, `pull without a home -> ${pull.status} ${pull.text}`);
  assert.equal((await call("alice", "POST", "/api/rudder/home/pull", { runId: "../etc" })).status, 400);
});
