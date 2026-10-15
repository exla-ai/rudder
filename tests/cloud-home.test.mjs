// The home machine + phone app, end to end against the real control plane:
// a fake home supervisor holds the control socket, and the phone's routes
// (/api/app/*, /app, /app/native-done) are driven over HTTP. Plus the
// transcript reader the supervisor uses, on Claude and Codex fixtures.
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

import { readTranscript, lastAssistantText } from "../cloud/worker/transcripts.mjs";

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
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-home-test-"));
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

serverTest("a home machine's repos, state and phone requests flow over its control socket", async (t) => {
  const { port, base, dbPath, log } = await bootServer(t);
  const cliToken = "rdr_home_test_cli";
  const workerToken = "rdrw_home_test_worker";
  const accountId = "acct-home";
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
  insert.run({ id: "home-test", account: accountId, key: "home", name: "home", machine: "m-home", hash: tokenHash(workerToken), kind: "home", url: null, home: null, now });
  insert.run({ id: "libra-test", account: accountId, key: "home:exla-ai/libra", name: "libra", machine: null, hash: tokenHash(workerToken), kind: "home-repo", url: "https://github.com/exla-ai/libra.git", home: "home-test", now });
  // Someone else's repo must never show up.
  insert.run({ id: "other-test", account: "acct-other", key: "home:x/y", name: "y", machine: null, hash: tokenHash("rdrw_other"), kind: "home-repo", url: "https://github.com/x/y.git", home: "home-other", now });
  db.close();

  const auth = { authorization: `Bearer ${cliToken}`, "content-type": "application/json" };

  // Signed out: the app API refuses, the shell still serves.
  assert.equal((await fetch(`${base}/api/app/state`)).status, 401);
  const shell = await fetch(`${base}/app/p/libra-test/a/some-run`);
  assert.equal(shell.status, 200, "client routes fall back to the app shell");
  assert.match(await shell.text(), /<html/i);
  const sw = await fetch(`${base}/app/sw.js`);
  assert.equal(sw.headers.get("service-worker-allowed"), "/app");
  const traversal = await fetch(`${base}/app/..%2f..%2fpackage.json`);
  assert.doesNotMatch(await traversal.text(), /"better-auth"/, "no reads outside the app dir");

  // The iOS shell's sign-in ends here; with no browser session it reports an error on its scheme.
  const nativeDone = await fetch(`${base}/app/native-done`, { redirect: "manual" });
  assert.equal(nativeDone.status, 302);
  assert.equal(nativeDone.headers.get("location"), "rudder://auth?error=signin");

  // A worker token is not a user token.
  assert.equal((await fetch(`${base}/api/app/state`, { headers: { authorization: `Bearer ${workerToken}` } })).status, 401);

  // Before the supervisor connects: the project is listed, phone requests explain why they can't run.
  let state = await (await fetch(`${base}/api/app/state`, { headers: auth })).json();
  assert.equal(state.home.id, "home-test");
  assert.equal(state.home.connected, false);
  assert.deepEqual(state.projects.map((p) => p.workspaceId), ["libra-test"]);
  const early = await fetch(`${base}/api/app/send`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", run: "r1", text: "hi" }) });
  assert.equal(early.status, 503);
  assert.match((await early.json()).error, /not connected/);

  // A bad worker token can't take the control socket.
  const rejected = await new Promise((resolve) => {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-test/control`, { headers: { authorization: "Bearer rdrw_wrong" } });
    ws.on("unexpected-response", (_req, res) => resolve(res.statusCode));
    ws.on("open", () => resolve("open"));
    ws.on("error", () => undefined);
  });
  assert.equal(rejected, 401);

  // The fake supervisor connects, gets its repo list, publishes state, answers RPCs.
  const messages = [];
  const control = new WebSocket(`ws://127.0.0.1:${port}/api/rudder/home/home-test/control`, {
    headers: { authorization: `Bearer ${workerToken}` },
  });
  t.after(() => control.close());
  control.on("message", (data) => {
    const message = JSON.parse(data.toString());
    messages.push(message);
    if (message.type === "rpc") {
      const reply = message.method === "send" && message.params.text === "boom"
        ? { type: "rpc-result", id: message.id, error: "the libra dashboard is starting" }
        : { type: "rpc-result", id: message.id, result: { ok: true, echo: message.method, params: message.params } };
      control.send(JSON.stringify(reply));
    }
  });
  await new Promise((resolve, reject) => { control.on("open", resolve); control.on("error", reject); });
  const repos = await waitFor(() => messages.find((m) => m.type === "repos"));
  assert.deepEqual(repos.repos, [{ workspaceId: "libra-test", name: "libra", repo: "exla-ai/libra", remote: "https://github.com/exla-ai/libra.git", ref: null }]);

  control.send(JSON.stringify({
    type: "state",
    tailnet: { hostname: "rudder-home", dnsName: "rudder-home.tail1.ts.net", online: true, https: true },
    projects: [{
      workspaceId: "libra-test",
      name: "libra",
      dashboard: "running",
      error: null,
      agents: [{ id: "r1", title: "Fix the webhook", status: "waiting", attention: "permission" }],
      ports: [{ port: 3000, label: "next dev", url: "https://rudder-home.tail1.ts.net:3000" }],
    }],
  }));
  state = await waitFor(async () => {
    const next = await (await fetch(`${base}/api/app/state`, { headers: auth })).json();
    return next.projects[0]?.agents?.length ? next : null;
  });
  assert.equal(state.email, "me@example.com");
  assert.equal(state.home.connected, true);
  assert.equal(state.home.tailnet.dnsName, "rudder-home.tail1.ts.net");
  assert.equal(state.projects[0].dashboard, "running");
  assert.equal(state.projects[0].agents[0].attention, "permission");
  assert.equal(state.projects[0].ports[0].url, "https://rudder-home.tail1.ts.net:3000");

  // Phone requests become RPCs to the supervisor, scoped to that project.
  const sent = await (await fetch(`${base}/api/app/send`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", run: "r1", text: "yes, allow it" }) })).json();
  assert.deepEqual(sent, { ok: true, echo: "send", params: { workspaceId: "libra-test", runId: "r1", text: "yes, allow it" } });
  const task = await (await fetch(`${base}/api/app/task`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", text: "add a health check" }) })).json();
  assert.equal(task.echo, "task");
  const action = await (await fetch(`${base}/api/app/action`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", run: "r1", action: "merge" }) })).json();
  assert.deepEqual(action.params, { workspaceId: "libra-test", runId: "r1", action: "merge" });
  const transcript = await (await fetch(`${base}/api/app/transcript?workspace=libra-test&run=r1&limit=5`, { headers: auth })).json();
  assert.deepEqual(transcript.params, { workspaceId: "libra-test", runId: "r1", limit: 20 });

  // A supervisor error is the phone's error message.
  const failed = await fetch(`${base}/api/app/send`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", run: "r1", text: "boom" }) });
  assert.equal(failed.status, 400);
  assert.equal((await failed.json()).error, "the libra dashboard is starting");

  // Another account's project is not reachable, even by id.
  const foreign = await fetch(`${base}/api/app/send`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "other-test", run: "r1", text: "hi" }) });
  assert.equal(foreign.status, 404, log());
  const badAction = await fetch(`${base}/api/app/action`, { method: "POST", headers: auth, body: JSON.stringify({ workspace: "libra-test", run: "r1", action: "delete" }) });
  assert.equal(badAction.status, 400);

  // The CLI view of the same machine.
  const home = await (await fetch(`${base}/api/rudder/home`, { headers: auth })).json();
  assert.equal(home.home.id, "home-test");
  assert.equal(home.projects[0].ports[0].port, 3000);

  // A project error reported by the supervisor is persisted as its failure reason.
  control.send(JSON.stringify({ type: "state", projects: [{ workspaceId: "libra-test", name: "libra", dashboard: "failed", error: "git clone failed: Repository not found", agents: [], ports: [] }] }));
  await waitFor(async () => {
    const next = await (await fetch(`${base}/api/app/state`, { headers: auth })).json();
    return next.projects[0].failureReason === "git clone failed: Repository not found";
  });
});

serverTest("a failed worker heartbeat records why", async (t) => {
  const { base, dbPath } = await bootServer(t);
  const workerToken = "rdrw_fail_test";
  const now = new Date().toISOString();
  const db = new Database(dbPath);
  db.pragma("journal_mode = WAL");
  db.prepare(
    "insert into rudder_workspaces (id, account_id, workspace_key, status, worker_token_hash, created_at, updated_at) values (?,?,?,?,?,?,?)",
  ).run("ws-fail", "acct", "k", "running", tokenHash(workerToken), now, now);
  db.close();
  const res = await fetch(`${base}/api/rudder/workspace/ws-fail/heartbeat`, {
    method: "POST",
    headers: { authorization: `Bearer ${workerToken}`, "content-type": "application/json" },
    body: JSON.stringify({ state: "failed", exitCode: 2, reason: "git clone failed: Repository not found" }),
  });
  assert.equal(res.status, 200);
  const check = new Database(dbPath, { readonly: true });
  const row = check.prepare("select status, failure_reason from rudder_workspaces where id = 'ws-fail'").get();
  check.close();
  assert.deepEqual(row, { status: "failed", failure_reason: "git clone failed: Repository not found" });
});

// ---------------------------------------------------------------------------
// Transcripts.
// ---------------------------------------------------------------------------

function fixtureRepo(record) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-transcript-"));
  fs.mkdirSync(path.join(dir, ".rudder", "runs", record.id), { recursive: true });
  fs.writeFileSync(path.join(dir, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));
  return dir;
}

nodeTest("a Claude transcript becomes chat messages with tool calls matched to their results", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-home-"));
  const cwd = "/workspace/code/libra";
  const sessionId = "11111111-2222-3333-4444-555555555555";
  const projectDir = path.join(home, ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"));
  fs.mkdirSync(projectDir, { recursive: true });
  const lines = [
    { type: "user", uuid: "u1", timestamp: "2026-10-15T10:00:00Z", message: { role: "user", content: "fix the webhook" } },
    { type: "user", uuid: "u0", message: { role: "user", content: "<system-reminder>internal</system-reminder>" } },
    { type: "assistant", uuid: "a1", timestamp: "2026-10-15T10:00:05Z", message: { role: "assistant", content: [
      { type: "thinking", thinking: "hidden" },
      { type: "text", text: "Looking at **the handler**." },
      { type: "tool_use", id: "t1", name: "Bash", input: { command: "npm test" } },
    ] } },
    { type: "assistant", uuid: "side", isSidechain: true, message: { role: "assistant", content: [{ type: "text", text: "subagent chatter" }] } },
    { type: "user", uuid: "u2", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "t1", is_error: true, content: [{ type: "text", text: "1 failing" }] }] } },
    { type: "assistant", uuid: "a2", message: { role: "assistant", content: [{ type: "text", text: "Fixed; tests pass." }] } },
  ];
  fs.writeFileSync(path.join(projectDir, `${sessionId}.jsonl`), `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`);
  const record = { id: "run-1", backend: "claude", taskSummary: "Fix the webhook", session: { nativeSessionId: sessionId }, workspace: { path: cwd } };
  const repo = fixtureRepo(record);

  const transcript = readTranscript(repo, "run-1", { limit: 50, home });
  assert.equal(transcript.title, "Fix the webhook");
  assert.deepEqual(
    transcript.messages.map((m) => [m.role, m.text || m.tool?.name]),
    [["user", "fix the webhook"], ["assistant", "Looking at **the handler**."], ["tool", "Bash"], ["assistant", "Fixed; tests pass."]],
  );
  const tool = transcript.messages[2].tool;
  assert.deepEqual(tool, { name: "Bash", input: "npm test", output: "1 failing", isError: true });
  assert.equal(transcript.messages[0].ts, Date.parse("2026-10-15T10:00:00Z"));
  assert.equal(lastAssistantText(record, { home }).text, "Fixed; tests pass.");
  assert.throws(() => readTranscript(repo, "../../etc", { home }), /agent not found/);
});

nodeTest("a Codex rollout becomes chat messages, including code-mode tool calls", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-home-"));
  const sessionId = "01a13ce1-6861-7893-b4a0-9fb8e889dd57";
  const day = path.join(home, ".codex", "sessions", "2026", "10", "14");
  fs.mkdirSync(day, { recursive: true });
  const item = (payload) => ({ type: "response_item", timestamp: "2026-10-14T17:06:07Z", payload });
  const lines = [
    { type: "session_meta", payload: { id: sessionId } },
    item({ type: "message", role: "user", content: [{ type: "input_text", text: "<environment_context>cwd</environment_context>" }] }),
    item({ type: "message", role: "user", content: [{ type: "input_text", text: "Rudder-specific context injected by Rudder: be the owner" }] }),
    item({ type: "message", role: "user", content: [{ type: "input_text", text: "add billing" }] }),
    item({ type: "custom_tool_call", name: "exec", call_id: "c1", input: 'text(await tools.exec_command({cmd:"rg billing src"}))' }),
    item({ type: "custom_tool_call_output", call_id: "c1", output: [{ type: "input_text", text: "src/billing.ts" }] }),
    item({ type: "function_call", name: "shell", call_id: "c2", arguments: JSON.stringify({ command: ["bash", "-lc", "npm test"] }) }),
    item({ type: "function_call_output", call_id: "c2", output: JSON.stringify({ output: "boom", metadata: { exit_code: 1 } }) }),
    item({ type: "message", role: "assistant", content: [{ type: "output_text", text: "Billing is in." }] }),
  ];
  fs.writeFileSync(path.join(day, `rollout-2026-10-14T17-06-07-${sessionId}.jsonl`), `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`);
  const repo = fixtureRepo({ id: "run-2", backend: "codex", task: "add billing", session: { nativeSessionId: sessionId } });

  const transcript = readTranscript(repo, "run-2", { limit: 50, home });
  assert.deepEqual(
    transcript.messages.map((m) => [m.role, m.text || m.tool?.input]),
    [["user", "add billing"], ["tool", "rg billing src"], ["tool", "npm test"], ["assistant", "Billing is in."]],
  );
  assert.equal(transcript.messages[1].tool.output, "src/billing.ts");
  assert.deepEqual([transcript.messages[2].tool.output, transcript.messages[2].tool.isError], ["boom", true]);
});

nodeTest("an agent with no session yet shows the prompts Rudder recorded", () => {
  const repo = fixtureRepo({ id: "run-3", backend: "claude", task: "t", turns: [{ ts: "1789425365440", prompt: "start here", source: "user" }] });
  const transcript = readTranscript(repo, "run-3", { home: os.tmpdir() });
  assert.deepEqual(transcript.messages, [{ id: "turn-0", role: "user", ts: 1789425365440, text: "start here" }]);
});
