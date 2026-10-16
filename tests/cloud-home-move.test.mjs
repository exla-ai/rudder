// Moving laptop agents onto the home machine, and deleting them anywhere,
// against the real control plane with a fake home supervisor on the control
// socket (the same harness shape as cloud-home.test.mjs).
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import { WebSocket } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..");
const cloudDir = path.join(repoRoot, "cloud");
const cloudReady =
  fs.existsSync(path.join(cloudDir, "dist", "server.js")) &&
  fs.existsSync(path.join(cloudDir, "node_modules", "better-sqlite3"));
const serverTest = cloudReady
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

async function bootServer(t) {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-home-move-test-"));
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
  return { port, base, dbPath, log: () => log };
}

function seed(dbPath, { cliToken, workerToken, accountId }) {
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(cliToken), accountId, "me@example.com", now, now);
  const insert = db.prepare(`
    insert into rudder_workspaces (id, account_id, workspace_key, repo_name, status, machine_id, worker_token_hash,
      source_kind, repo_url, home_id, created_at, updated_at, last_activity_at)
    values (@id, @account, @key, @name, 'running', @machine, @hash, @kind, @url, @home, @now, @now, @now)
  `);
  insert.run({ id: "home-mv", account: accountId, key: "home", name: "home", machine: "m-home", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
  insert.run({ id: "libra-mv", account: accountId, key: "home:exla-ai/libra", name: "libra", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/exla-ai/libra.git", home: "home-mv", now });
  db.close();
}

function connectSupervisor(t, port, workerToken, onRpc) {
  const messages = [];
  const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-mv/control`, {
    headers: { authorization: `Bearer ${workerToken}` },
  });
  t.after(() => control.close());
  control.on("message", (data) => {
    const message = JSON.parse(data.toString());
    messages.push(message);
    if (message.type === "rpc") {
      Promise.resolve(onRpc(message))
        .then((result) => control.send(JSON.stringify({ type: "rpc-result", id: message.id, result })))
        .catch((error) => control.send(JSON.stringify({ type: "rpc-result", id: message.id, error: error.message })));
    }
  });
  return {
    messages,
    ready: new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); }),
  };
}

serverTest("a laptop's agents move to the home machine through one bundle the supervisor pulls", async (t) => {
  const { port, base, dbPath } = await bootServer(t);
  const cliToken = "rdr_move_cli";
  const workerToken = "rdrw_move_worker";
  seed(dbPath, { cliToken, workerToken, accountId: "acct-move" });
  const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };
  const bundle = Buffer.from("pretend this is a tgz of jj changes and transcripts");

  let pulled = null;
  const supervisor = connectSupervisor(t, port, workerToken, async (message) => {
    if (message.method !== "adopt") {
      return { ok: true };
    }
    // The supervisor fetches the bytes with its own (worker) token.
    const asUser = await fetch(`${base}${message.params.bundlePath}`, { headers: { authorization: `Bearer ${cliToken}` } });
    assert.notEqual(asUser.status, 200, "a user token cannot read a bundle off the machine's route");
    const res = await fetch(`${base}${message.params.bundlePath}`, { headers: { authorization: `Bearer ${workerToken}` } });
    assert.equal(res.status, 200);
    pulled = Buffer.from(await res.arrayBuffer());
    return { adopted: [{ runId: "run-a", resumable: true }, { runId: "run-b", resumable: false }], failed: [] };
  });
  await supervisor.ready;

  const started = await fetch(`${base}/api/rudder/home/move`, {
    method: "POST",
    headers: auth,
    body: JSON.stringify({ repo: "exla-ai/libra", bundle: bundle.toString("base64") }),
  });
  assert.equal(started.status, 202);
  const { move } = await started.json();
  assert.ok(move.id);

  const done = await waitFor(async () => {
    const res = await (await fetch(`${base}/api/rudder/home/move/${move.id}`, { headers: auth })).json();
    return res.move.status === "done" || res.move.status === "failed" ? res.move : null;
  });
  assert.equal(done.status, "done", JSON.stringify(done));
  assert.deepEqual(done.result.adopted.map((a) => a.runId), ["run-a", "run-b"]);
  assert.deepEqual(pulled, bundle, "the supervisor got the exact bytes the laptop sent");
  const adopt = supervisor.messages.find((m) => m.type === "rpc" && m.method === "adopt");
  assert.equal(adopt.params.workspaceId, "libra-mv", "staged into that repo's project");

  // Served once: after the job, the bundle is gone from the control plane.
  const again = await fetch(`${base}/api/rudder/home/home-mv/moves/${move.id}/bundle`, { headers: { authorization: `Bearer ${workerToken}` } });
  assert.equal(again.status, 404);
  // Another account cannot read the job.
  const db = new Database(dbPath);
  db.prepare("insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)")
    .run(tokenHash("rdr_someone_else"), "acct-other", "x@example.com", new Date().toISOString(), new Date().toISOString());
  db.close();
  const peek = await fetch(`${base}/api/rudder/home/move/${move.id}`, { headers: { authorization: "Bearer rdr_someone_else" } });
  assert.equal(peek.status, 404);
});

serverTest("deletes stay in sync: a laptop's delete reaches a sleeping home when it wakes, and asks are honest", async (t) => {
  const { port, base, dbPath } = await bootServer(t);
  const cliToken = "rdr_sync_cli";
  const workerToken = "rdrw_sync_worker";
  seed(dbPath, { cliToken, workerToken, accountId: "acct-sync" });
  const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };
  const agents = (body) => fetch(`${base}/api/rudder/home/agents`, { method: "POST", headers: auth, body: JSON.stringify(body) }).then((r) => r.json());

  // Asleep: "which still exist?" is unknown (never "all deleted"), and a
  // delete is queued.
  assert.deepEqual(await agents({ repo: "exla-ai/libra", action: "check", runIds: ["run-a"] }), { known: false });
  assert.deepEqual(await agents({ repo: "exla-ai/libra", action: "delete", runIds: ["run-a"] }), { status: "queued" });
  // A repo the home never opened has none of these agents.
  assert.deepEqual(await agents({ repo: "exla-ai/other", action: "check", runIds: ["run-a"] }), { known: true, existing: [] });

  const supervisor = connectSupervisor(t, port, workerToken, (message) => {
    if (message.method === "runs") return { known: true, existing: ["run-b"] };
    if (message.method === "deleteRuns") return { queued: message.params.runIds.length };
    return { ok: true };
  });
  await supervisor.ready;
  const flushed = await waitFor(() => supervisor.messages.find((m) => m.type === "rpc" && m.method === "deleteRuns"));
  assert.deepEqual(flushed.params, { workspaceId: "libra-mv", runIds: ["run-a"] }, "the queued delete is sent on connect");

  assert.deepEqual(await agents({ repo: "exla-ai/libra", action: "check", runIds: ["run-a", "run-b"] }), { known: true, existing: ["run-b"] });
  assert.deepEqual(await agents({ repo: "exla-ai/libra", action: "delete", runIds: ["run-b"] }), { status: "deleted" });

  // The phone's delete is a dashboard action like stop and merge.
  const action = await (await fetch(`${base}/api/app/action`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-mv", run: "run-b", action: "delete" }) })).json();
  assert.deepEqual(action, { ok: true });
  const sent = supervisor.messages.filter((m) => m.type === "rpc" && m.method === "action").pop();
  assert.deepEqual(sent.params, { workspaceId: "libra-mv", runId: "run-b", action: "delete" });
  const bogus = await fetch(`${base}/api/app/action`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-mv", run: "run-b", action: "rm -rf" }) });
  assert.equal(bogus.status, 400);
});
