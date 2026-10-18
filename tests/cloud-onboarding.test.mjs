// A new user moving between the phone and a laptop, against the real control
// plane: `rudder login` approved from a signed-in phone with a short code, and
// GitHub connected from the phone through the device flow (a fake GitHub
// stands in), landing in the vault the home machine reads.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { createRequire } from "node:module";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const cloudDir = path.join(here, "..", "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = cloudReady
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
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

async function waitFor(fn, { timeout = 8000, interval = 50 } = {}) {
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

async function bootServer(t, extraEnv = {}) {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-onboard-test-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const child = spawn(process.execPath, [path.join(cloudDir, "dist", "server.js")], {
    cwd: cloudDir,
    env: {
      ...process.env,
      PORT: String(port),
      BETTER_AUTH_URL: base,
      BETTER_AUTH_SECRET: "test-secret-test-secret",
      RUDDER_CLOUD_DATA_DIR: dataDir,
      RUDDER_CLOUD_DB: dbPath,
      RUDDER_CLOUD_PERSIST_STATE: "0",
      RUDDER_S3_BUCKET: "",
      SLACK_BOT_TOKEN: "",
      FLY_API_TOKEN: "",
      ...extraEnv,
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => { log += d.toString(); });
  child.stderr.on("data", (d) => { log += d.toString(); });
  t.after(() => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);
  return { base, dbPath, log: () => log };
}

function addToken(dbPath, token, accountId, email) {
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(token), accountId, email, now, now);
  db.close();
}

const client = (base) => (pathname, body, headers = {}) => fetch(`${base}${pathname}`, {
  method: "POST",
  headers: { "content-type": "application/json", ...headers },
  body: JSON.stringify(body ?? {}),
});

test("a signed-in phone approves `rudder login` with a short code", async (t) => {
  const { base, dbPath } = await bootServer(t);
  const phoneToken = "rdr_phone_signed_in";
  addToken(dbPath, phoneToken, "acct-phone", "me@example.com");
  const post = client(base);
  const phone = { authorization: `Bearer ${phoneToken}` };

  const started = await (await post("/api/cli/login", { deviceName: "Viraat's MacBook\u0007", client: "rudder" })).json();
  assert.match(started.userCode, /^[A-HJ-NP-Z2-9]{3}-[A-HJ-NP-Z2-9]{3}$/);
  assert.equal(started.approveUrl, `${base}/app/approve?code=${started.userCode}`);
  assert.equal(started.expiresIn, 300);
  const poll = async () => (await post("/api/cli/login/poll", { deviceCode: started.deviceCode })).json();
  assert.deepEqual(await poll(), { pending: true });

  // Only a signed-in user can even look a code up.
  assert.equal((await post("/api/app/computer/lookup", { code: started.userCode })).status, 401);
  assert.equal((await post("/api/app/computer/approve", { code: started.userCode, approve: true })).status, 401);

  // Lowercase, no dash: the same code. The phone sees which computer asks.
  const loose = started.userCode.replace("-", "").toLowerCase();
  const looked = await (await post("/api/app/computer/lookup", { code: loose }, phone)).json();
  assert.equal(looked.deviceName, "Viraat's MacBook");
  assert.equal(looked.code, started.userCode);
  assert.deepEqual(await poll(), { pending: true }, "a lookup does not approve");

  const approved = await (await post("/api/app/computer/approve", { code: started.userCode, approve: true }, phone)).json();
  assert.deepEqual(approved, { ok: true, approved: true, deviceName: "Viraat's MacBook" });

  // The CLI gets a token on the phone's account.
  const done = await poll();
  assert.equal(done.pending, false);
  assert.match(done.token, /^rdr_/);
  assert.equal(done.accountId, "acct-phone");
  assert.equal(done.email, "me@example.com");
  const state = await (await fetch(`${base}/api/app/state`, { headers: { authorization: `Bearer ${done.token}` } })).json();
  assert.equal(state.email, "me@example.com");
  // Picked up once.
  assert.equal((await post("/api/cli/login/poll", { deviceCode: started.deviceCode })).status, 404);

  // Single use: the code is gone once answered.
  const again = await post("/api/app/computer/approve", { code: started.userCode, approve: true }, phone);
  assert.equal(again.status, 400);
  assert.match((await again.json()).error, /wrong or expired/);
  assert.equal((await post("/api/app/computer/lookup", { code: started.userCode }, phone)).status, 400);
});

test("denying on the phone stops the CLI waiting, and the code dies with it", async (t) => {
  const { base, dbPath } = await bootServer(t);
  addToken(dbPath, "rdr_phone_deny", "acct-deny", "me@example.com");
  const post = client(base);
  const phone = { authorization: "Bearer rdr_phone_deny" };
  const started = await (await post("/api/cli/login", { deviceName: "build-box" })).json();
  const denied = await (await post("/api/app/computer/approve", { code: started.userCode, approve: false }, phone)).json();
  assert.deepEqual(denied, { ok: true, approved: false });
  const poll = await (await post("/api/cli/login/poll", { deviceCode: started.deviceCode })).json();
  assert.equal(poll.pending, false);
  assert.equal(poll.denied, true);
  assert.equal(poll.token, undefined);
  assert.equal((await post("/api/app/computer/approve", { code: started.userCode, approve: true }, phone)).status, 400);
});

test("guessing codes is rate limited per account", async (t) => {
  const { base, dbPath } = await bootServer(t);
  addToken(dbPath, "rdr_phone_guess", "acct-guess", "me@example.com");
  const post = client(base);
  const phone = { authorization: "Bearer rdr_phone_guess" };
  const started = await (await post("/api/cli/login", {})).json();
  const statuses = [];
  for (let attempt = 0; attempt < 11; attempt += 1) {
    statuses.push((await post("/api/app/computer/lookup", { code: "AAA-AAA" }, phone)).status);
  }
  assert.deepEqual(statuses.slice(0, 10), new Array(10).fill(400));
  assert.equal(statuses[10], 429);
  // Even the right code is refused while locked out.
  assert.equal((await post("/api/app/computer/approve", { code: started.userCode, approve: true }, phone)).status, 429);
});

test("an approve code expires with its login", async (t) => {
  const { base, dbPath } = await bootServer(t, { RUDDER_CLI_LOGIN_TTL_MS: "800" });
  addToken(dbPath, "rdr_phone_late", "acct-late", "me@example.com");
  const post = client(base);
  const started = await (await post("/api/cli/login", { deviceName: "slow" })).json();
  await sleep(1000);
  const late = await post("/api/app/computer/approve", { code: started.userCode, approve: true }, { authorization: "Bearer rdr_phone_late" });
  assert.equal(late.status, 400);
  assert.equal((await post("/api/cli/login/poll", { deviceCode: started.deviceCode })).status, 404);
});

test("GitHub connected from the phone lands in the vault the home machine reads", async (t) => {
  // A fake GitHub: one authorization_pending, then a token.
  let polls = 0;
  const seen = [];
  const github = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => { body += c; });
    req.on("end", () => {
      seen.push({ url: req.url, body: body ? JSON.parse(body) : null, auth: req.headers.authorization });
      res.setHeader("content-type", "application/json");
      if (req.url === "/login/device/code") {
        return res.end(JSON.stringify({ device_code: "dev-123", user_code: "WDJB-MJHT", verification_uri: "https://github.com/login/device", expires_in: 900, interval: 1 }));
      }
      if (req.url === "/login/oauth/access_token") {
        polls += 1;
        return res.end(JSON.stringify(polls === 1 ? { error: "authorization_pending" } : { access_token: "gho_phone_token", token_type: "bearer" }));
      }
      if (req.url === "/user") {
        return res.end(JSON.stringify(req.headers.authorization === "Bearer gho_phone_token" ? { id: 7, login: "octo" } : { message: "Bad credentials" }));
      }
      res.statusCode = 404;
      res.end("{}");
    });
  });
  await new Promise((resolve) => github.listen(0, "127.0.0.1", resolve));
  t.after(() => github.close());
  const ghBase = `http://127.0.0.1:${github.address().port}`;
  const { base, dbPath } = await bootServer(t, {
    RUDDER_GITHUB_WEB_URL: ghBase,
    RUDDER_GITHUB_API_URL: ghBase,
    RUDDER_SECRETS_KEY: randomBytes(32).toString("base64"),
  });
  const phoneToken = "rdr_phone_github";
  addToken(dbPath, phoneToken, "acct-gh", "me@example.com");
  const workerToken = "rdrw_home_gh";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.prepare(`insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, worker_token_hash, source_kind, created_at, updated_at)
    values ('home-gh', 'acct-gh', 'home', 'home', 'stopped', ?, 'home', ?, ?)`).run(tokenHash(workerToken), now, now);
  db.close();
  const post = client(base);
  const phone = { authorization: `Bearer ${phoneToken}` };
  const state = async () => (await fetch(`${base}/api/app/state`, { headers: phone })).json();

  assert.deepEqual((await state()).github, { connected: false, source: null, updatedAt: null });
  const started = await (await post("/api/app/github/start", {}, phone)).json();
  assert.equal(started.status, "waiting");
  assert.equal(started.userCode, "WDJB-MJHT");
  assert.equal(started.verificationUri, "https://github.com/login/device");
  assert.equal(seen[0].body.scope, "repo read:org workflow");
  // Asking again within the code's life hands back the same code.
  assert.equal((await (await post("/api/app/github/start", {}, phone)).json()).userCode, "WDJB-MJHT");

  // Polled faster than GitHub's interval: answered without asking GitHub.
  assert.equal((await (await post("/api/app/github/poll", {}, phone)).json()).status, "waiting");
  assert.equal(polls, 0);
  const result = await waitFor(async () => {
    const next = await (await post("/api/app/github/poll", {}, phone)).json();
    return next.status === "waiting" ? null : next;
  }, { timeout: 6000, interval: 300 });
  assert.deepEqual(result, { status: "done", account: "octo" });
  assert.equal(polls, 2);

  const after = (await state()).github;
  assert.equal(after.connected, true);
  assert.equal(after.source, "phone");
  // What the home supervisor fetches at boot (and on every repo push).
  const vault = await (await fetch(`${base}/api/rudder/workspace/home-gh/secrets`, { headers: { authorization: `Bearer ${workerToken}` } })).json();
  assert.equal(vault.env.GH_TOKEN, "gho_phone_token");
  // Someone else's account sees nothing of it.
  addToken(dbPath, "rdr_other", "acct-other", "x@example.com");
  const other = await (await fetch(`${base}/api/app/state`, { headers: { authorization: "Bearer rdr_other" } })).json();
  assert.equal(other.github.connected, false);
  assert.equal((await (await post("/api/app/github/poll", {}, { authorization: "Bearer rdr_other" })).json()).status, "failed");
});

test("connecting GitHub says why when the control plane has no vault", async (t) => {
  const { base, dbPath } = await bootServer(t, { RUDDER_SECRETS_KEY: "" });
  addToken(dbPath, "rdr_novault", "acct-nv", "me@example.com");
  const res = await client(base)("/api/app/github/start", {}, { authorization: "Bearer rdr_novault" });
  assert.equal(res.status, 503);
  assert.match((await res.json()).error, /RUDDER_SECRETS_KEY/);
  const state = await (await fetch(`${base}/api/app/state`, { headers: { authorization: "Bearer rdr_novault" } })).json();
  assert.equal(state.github, null);
});
