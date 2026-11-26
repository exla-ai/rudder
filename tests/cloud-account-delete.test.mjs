// Deleting an account (POST /api/app/account/delete) against the real control
// plane with a fake Fly API: nothing of the account can sign in or hold a
// secret afterwards, the reaper frees its machines and volumes, another
// account is untouched, and the only admin of a shared org is refused.
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
const cloudDir = path.join(path.resolve(here, ".."), "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const test = cloudReady
  ? nodeTest
  : (name, options, fn) =>
      nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn);
const Database = cloudReady ? createRequire(path.join(cloudDir, "package.json"))("better-sqlite3") : null;

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

async function waitFor(fn, { timeout = 8000, interval = 100 } = {}) {
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

test("deleting an account removes all of it and nothing of anyone else", { timeout: 40_000 }, async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-account-delete-test-"));
  const dbPath = path.join(dataDir, "state.sqlite");
  const base = `http://127.0.0.1:${port}`;
  const flyCalls = [];
  let machineDeletesToFail = 1;

  // The first machine destroy fails, so the reaper has to finish on a later sweep.
  const fly = http.createServer((req, res) => {
    flyCalls.push(`${req.method} ${req.url}`);
    if (req.method === "DELETE" && /\/machines\//.test(req.url ?? "") && machineDeletesToFail > 0) {
      machineDeletesToFail -= 1;
      res.writeHead(500, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "fly is having a moment" }));
      return;
    }
    if (req.method === "DELETE") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ ok: true }));
      return;
    }
    res.writeHead(404).end();
  });
  await new Promise((resolve, reject) => {
    fly.once("error", reject);
    fly.listen(0, "127.0.0.1", resolve);
  });

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
      RUDDER_FABRIC_S3_BUCKET: "",
      SLACK_BOT_TOKEN: "",
      RUDDER_SECRETS_KEY: randomBytes(32).toString("base64"),
      FLY_API_TOKEN: "test-fly-token",
      RUDDER_FLY_APP_NAME: "workers",
      FLY_API_HOSTNAME: `http://127.0.0.1:${fly.address().port}`,
      RUDDER_DELETE_SWEEP_MS: "1000",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let log = "";
  child.stdout.on("data", (d) => { log += d.toString(); });
  child.stderr.on("data", (d) => { log += d.toString(); });
  t.after(async () => {
    child.kill("SIGKILL");
    fly.closeAllConnections();
    await new Promise((resolve) => fly.close(resolve));
    fs.rmSync(dataDir, { recursive: true, force: true });
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  const call = (token) => (route, method = "GET", body) => fetch(`${base}${route}`, {
    method,
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const gone = call("rdr_gone_phone");
  const stays = call("rdr_stays");
  const admin = call("rdr_admin");

  const now = new Date().toISOString();
  const ms = Date.now();
  const db = new Database(dbPath);
  t.after(() => db.close());
  const count = (sql, ...args) => db.prepare(sql).get(...args).n;
  const insertToken = db.prepare("insert into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)");
  for (const [token, account] of [["rdr_gone_phone", "acct-gone"], ["rdr_gone_laptop", "acct-gone"], ["rdr_stays", "acct-stays"], ["rdr_admin", "acct-admin"]]) {
    insertToken.run(tokenHash(token), account, `${account}@example.com`, now, now);
  }
  const insertUser = db.prepare(`insert into "user" (id, name, email, emailVerified, createdAt, updatedAt) values (?,?,?,1,?,?)`);
  const insertSession = db.prepare("insert into session (id, expiresAt, token, createdAt, updatedAt, userId) values (?,?,?,?,?,?)");
  const insertLink = db.prepare("insert into account (id, accountId, providerId, userId, createdAt, updatedAt) values (?,?,?,?,?,?)");
  for (const account of ["acct-gone", "acct-stays"]) {
    insertUser.run(account, account, `${account}@example.com`, now, now);
    insertSession.run(`session-${account}`, now, `session-token-${account}`, now, now, account);
    insertLink.run(`link-${account}`, `google-${account}`, "google", account, now, now);
  }
  const insertWorkspace = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, volume_id, worker_token_hash,
      source_kind, home_id, host_kind, created_at, updated_at, last_activity_at)
    values (@id, @account, @key, @id, 'running', @machine, @volume, 'hash', @kind, @home, @host, @now, @now, @now)
  `);
  const workspace = (row) => insertWorkspace.run({ machine: null, volume: null, kind: null, home: null, host: null, now, ...row });
  // A home that moved to an external host still holds its Fly machine and volume.
  workspace({ id: "home-gone", account: "acct-gone", key: "home", machine: "m-gone", volume: "vol-gone", kind: "home", host: "external" });
  workspace({ id: "repo-gone", account: "acct-gone", key: "home:a/b", kind: "home-repo", home: "home-gone" });
  workspace({ id: "ws-gone", account: "acct-gone", key: "an-older-workspace", machine: "m-ws-gone", volume: "vol-ws-gone" });
  workspace({ id: "home-stays", account: "acct-stays", key: "home", machine: "m-stays", volume: "vol-stays", kind: "home" });
  db.prepare("insert into rudder_sails (id, account_id, status, runtime, created_at, updated_at) values (?,?,?,?,?,?)")
    .run("sail-gone", "acct-gone", "running", "byo-vm", now, now);
  const insertDigest = db.prepare("insert into rudder_digests (id, account_id, day, since_ms, until_ms, created_at, digest_json) values (?,?,?,?,?,?,'{\"projects\":[]}')");
  const insertCall = db.prepare("insert into rudder_voice_calls (call_id, account_id, started_at) values (?,?,?)");
  const insertPrefs = db.prepare("insert into rudder_account_prefs (account_id, time_zone, updated_at) values (?,?,?)");
  const insertSlackLink = db.prepare("insert into rudder_slack_links (team_id, user_id, account_id, created_at) values (?,?,?,?)");
  for (const account of ["acct-gone", "acct-stays"]) {
    insertDigest.run(`digest-${account}`, account, "2026-11-25", 0, 1, now);
    // Long over, so a restart would not rejoin them.
    insertCall.run(`call-${account}`, account, 1);
    insertPrefs.run(account, "UTC", now);
    insertSlackLink.run("T1", `U-${account}`, account, now);
  }
  db.prepare("insert into rudder_agent_visibility (workspace_id, run_id, visibility, updated_at) values (?,?,?,?)").run("repo-gone", "run-1", "private", ms);
  const insertOrg = db.prepare("insert into rudder_orgs (id, name, created_by, created_at, updated_at) values (?,?,?,?,?)");
  const insertMember = db.prepare("insert into rudder_org_members (org_id, account_id, role, email, joined_at) values (?,?,?,?,?)");
  const insertInvite = db.prepare("insert into rudder_org_invites (id, org_id, token_hash, role, created_by, created_at, expires_at) values (?,?,?,'member',?,?,?)");
  insertOrg.run("org-solo", "Just Me", "acct-gone", ms, ms);
  insertMember.run("org-solo", "acct-gone", "admin", null, ms);
  insertInvite.run("invite-solo", "org-solo", "hash-solo", "acct-gone", ms, ms + 86_400_000);
  // Created by the leaving account, with a second admin who joined later.
  insertOrg.run("org-shared", "Shared", "acct-gone", ms, ms);
  insertMember.run("org-shared", "acct-gone", "admin", null, ms);
  insertMember.run("org-shared", "acct-stays", "admin", null, ms + 1);
  insertInvite.run("invite-shared", "org-shared", "hash-shared", "acct-gone", ms, ms + 86_400_000);
  insertOrg.run("org-held", "Held Together", "acct-admin", ms, ms);
  insertMember.run("org-held", "acct-admin", "admin", null, ms);
  insertMember.run("org-held", "acct-stays", "member", null, ms + 1);
  // Tokens nobody can open: the rows go, and there is nothing to revoke at Slack.
  const insertInstall = db.prepare(`
    insert into rudder_integrations (id, owner_kind, owner_id, provider, token_ciphertext, created_at, updated_at)
    values (?,?,?,'slack','sealed',?,?)
  `);
  insertInstall.run("int-gone", "account", "acct-gone", now, now);
  insertInstall.run("int-solo", "org", "org-solo", now, now);
  insertInstall.run("int-shared", "org", "org-shared", now, now);
  insertInstall.run("int-stays", "account", "acct-stays", now, now);

  for (const api of [gone, stays]) {
    const saved = await api("/api/rudder/secrets/item", "PUT", { name: "API_KEY", kind: "env", valueBase64: Buffer.from("s3cret").toString("base64") });
    assert.equal(saved.status, 200, log);
  }
  assert.equal(count("select count(*) as n from rudder_account_keys"), 2);
  const pairing = await (await gone("/api/app/pair/start", "POST", {})).json();
  assert.match(pairing.code, /^[A-Z0-9]{4}-[A-Z0-9]{4}$/);

  // The only admin of an org with other people in it: refused, nothing changed.
  const refused = await admin("/api/app/account/delete", "POST");
  assert.equal(refused.status, 409);
  assert.match((await refused.json()).error, /only admin of Held Together.*Make someone else an admin first/);
  assert.equal((await admin("/api/app/state")).status, 200, "the refused account still works");
  assert.equal(count("select count(*) as n from rudder_org_members where org_id = 'org-held'"), 2);

  assert.equal((await gone("/api/app/account/delete")).status, 405);
  const deleted = await gone("/api/app/account/delete", "POST");
  assert.equal(deleted.status, 200, log);
  assert.deepEqual(await deleted.json(), { ok: true });

  // Nothing signs in.
  assert.equal((await gone("/api/app/state")).status, 401);
  assert.equal((await call("rdr_gone_laptop")("/api/app/state")).status, 401);
  const paired = await fetch(`${base}/api/app/pair/finish`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code: pairing.code }),
  });
  assert.equal(paired.status, 400, "a pairing code made before the deletion is dead");

  // Every row that names the account is gone at once.
  const accountTables = {
    rudder_tokens: "account_id", session: "userId", account: "userId", '"user"': "id", rudder_digests: "account_id",
    rudder_voice_calls: "account_id", rudder_account_prefs: "account_id", rudder_account_keys: "account_id",
    rudder_secrets: "account_id", rudder_org_members: "account_id", rudder_slack_links: "account_id",
    rudder_org_invites: "created_by", rudder_orgs: "created_by",
  };
  for (const [table, column] of Object.entries(accountTables)) {
    assert.equal(count(`select count(*) as n from ${table} where ${column} = 'acct-gone'`), 0, `${table} still names the account`);
  }
  assert.equal(count("select count(*) as n from rudder_integrations where owner_id in ('acct-gone', 'org-solo')"), 0);
  assert.equal(count("select count(*) as n from rudder_agent_visibility"), 0);
  assert.equal(count("select count(*) as n from rudder_orgs where id = 'org-solo'"), 0, "an org with nobody else in it goes too");
  assert.equal(db.prepare("select created_by from rudder_orgs where id = 'org-shared'").get().created_by, "acct-stays");
  // What is left of its machines is marked for the reaper and can no longer be reached.
  assert.equal(count("select count(*) as n from rudder_workspaces where account_id = 'acct-gone' and status <> 'deleting'"), 0);
  assert.equal(count("select count(*) as n from rudder_sails where account_id = 'acct-gone'"), 0, "a sail on the user's own VM has nothing to tear down");

  // The reaper retries the machine that failed, then frees every machine and
  // volume, the external home's included, and forgets the account.
  await waitFor(() => count("select count(*) as n from rudder_workspaces where account_id = 'acct-gone'") === 0, { timeout: 20_000, interval: 200 });
  for (const id of ["machines/m-gone", "volumes/vol-gone", "machines/m-ws-gone", "volumes/vol-ws-gone"]) {
    assert.ok(flyCalls.some((line) => line.startsWith("DELETE") && line.includes(id)), `${id} was never destroyed: ${flyCalls.join(", ")}`);
  }
  await waitFor(() => count("select count(*) as n from rudder_account_deletions") === 0, { timeout: 10_000, interval: 200 });

  // The other account has all of its own.
  assert.equal((await stays("/api/app/state")).status, 200, log);
  assert.ok(!flyCalls.some((line) => /m-stays|vol-stays/.test(line)), flyCalls.join(", "));
  assert.equal(db.prepare("select status from rudder_workspaces where id = 'home-stays'").get().status, "running");
  for (const [table, column] of Object.entries({ ...accountTables, rudder_orgs: null, rudder_org_invites: null })) {
    if (column) {
      assert.equal(count(`select count(*) as n from ${table} where ${column} = 'acct-stays'`) > 0, true, `${table} lost the other account`);
    }
  }
  assert.equal(count("select count(*) as n from rudder_integrations where id in ('int-shared', 'int-stays')"), 2);
  const secrets = await (await stays("/api/rudder/secrets")).json();
  assert.deepEqual(secrets.secrets.map((secret) => secret.name), ["API_KEY"]);
  assert.equal(count("select count(*) as n from rudder_org_members where org_id = 'org-shared'"), 1);
});
