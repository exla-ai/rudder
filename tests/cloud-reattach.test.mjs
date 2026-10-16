// A laptop that comes back must get its terminal back at once.
//
// A laptop that slept leaves its socket half-open: the relay still counts it as
// the controller until a ping times out, so the terminal reconnecting from the
// same `rudder cloud` run used to sit read-only (keys dropped) for up to a
// minute. The client now names itself (`?client=`), and the relay lets the same
// terminal take its own place back; any other terminal still only watches.
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
const test = cloudReady
  ? nodeTest
  : (name, opts, fn) =>
      nodeTest(name, { skip: "cloud not built (npm --prefix cloud install && npm --prefix cloud run build)" }, fn ?? opts);
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

function connect(url, headers) {
  const ws = new WebSocket(url, { headers });
  ws.binaryType = "nodebuffer";
  const statuses = [];
  const frames = [];
  ws.on("message", (data, isBinary) => {
    if (isBinary) {
      frames.push(Buffer.from(data).toString("utf8"));
    } else {
      try { statuses.push(JSON.parse(Buffer.from(data).toString("utf8"))); } catch { /* ignore */ }
    }
  });
  const ready = new Promise((resolve, reject) => {
    ws.once("open", resolve);
    ws.once("error", reject);
  });
  return { ws, statuses, frames, ready };
}

test("the same terminal reconnecting takes control back at once; another one only watches", { timeout: 30_000 }, async (t) => {
  const port = await freePort();
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-reattach-test-"));
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
  t.after(() => {
    child.kill("SIGKILL");
    try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch { /* ignore */ }
  });
  await waitFor(async () => (await fetch(`${base}/health`).catch(() => null))?.ok);
  await waitFor(() => fs.existsSync(dbPath));
  await sleep(150);

  const cliToken = "rdr_reattach_cli";
  const workerToken = "rdrw_reattach_worker";
  const sailId = "reattach-sail";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at) values (?,?,?,?,?)",
  ).run(tokenHash(cliToken), "acct-reattach", "me@example.com", now, now);
  db.prepare(
    "insert into rudder_sails (id, account_id, status, runtime, worker_token_hash, created_at, updated_at) values (?,?,?,?,?,?,?)",
  ).run(sailId, "acct-reattach", "running", "fly", tokenHash(workerToken), now, now);
  db.close();

  const worker = connect(`ws://127.0.0.1:${port}/api/rudder/sail/${sailId}/worker`, { authorization: `Bearer ${workerToken}` });
  const workerTexts = [];
  const workerBytes = [];
  worker.ws.on("message", (data, isBinary) => {
    (isBinary ? workerBytes : workerTexts).push(Buffer.from(data).toString("utf8"));
  });
  await worker.ready;

  const attachUrl = (client) => `ws://127.0.0.1:${port}/api/rudder/sail/${sailId}/attach${client ? `?client=${client}` : ""}`;
  const auth = { authorization: `Bearer ${cliToken}` };
  const laptop = "laptop-terminal-1234";

  // The laptop's first connection controls the session.
  const first = connect(attachUrl(laptop), auth);
  await first.ready;
  await waitFor(() => first.statuses.some((s) => s.control === "active"));

  // It goes half-open (the laptop slept): from the relay's side the socket is
  // still open. Keys typed into it before the drop was noticed are lost; the
  // same terminal comes back on a new socket.
  first.ws.pause?.();
  const again = connect(attachUrl(laptop), auth);
  await again.ready;
  await waitFor(() => again.statuses.length > 0);
  assert.equal(again.statuses[0].control, "active", `the returning terminal has control at once: ${JSON.stringify(again.statuses)}`);
  await waitFor(() => first.ws.readyState === WebSocket.CLOSED, { timeout: 5000 });

  again.ws.send(Buffer.from("typed-after-coming-back"), { binary: true });
  again.ws.send(JSON.stringify({ type: "repaint" }));
  await waitFor(() => workerBytes.some((text) => text.includes("typed-after-coming-back")));
  await waitFor(() => workerTexts.some((text) => text.includes('"repaint"')));

  // A different terminal (or an old client that names none) still only watches.
  const other = connect(attachUrl("someone-else-5678"), auth);
  await other.ready;
  await waitFor(() => other.statuses.length > 0);
  assert.equal(other.statuses[0].control, "read-only");
  const anonymous = connect(attachUrl(null), auth);
  await anonymous.ready;
  await waitFor(() => anonymous.statuses.length > 0);
  assert.equal(anonymous.statuses[0].control, "read-only");
  other.ws.send(Buffer.from("observer-must-not-type"), { binary: true });
  await sleep(200);
  assert.ok(!workerBytes.some((text) => text.includes("observer-must-not-type")));
  assert.equal(again.ws.readyState, WebSocket.OPEN, "a watcher never displaces the controller");

  for (const socket of [again.ws, other.ws, anonymous.ws, worker.ws]) socket.close();
});
