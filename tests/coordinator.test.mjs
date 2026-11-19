import test, { after } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { agentEvents, createCoordinatorTools, snapshot, startCoordinatorBroker } from "../cloud/worker/coordinator.mjs";

const dirs = [];
after(() => {
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true });
});
function tmpdir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-all-agents-"));
  dirs.push(dir);
  return dir;
}

const project = (name, agents, extra = {}) => ({ workspaceId: `wsp_${name}`, name, dir: `/x/${name}`, status: "running", descriptor: {}, agents: () => agents, ...extra });
const agent = (id, status, extra = {}) => ({ id, title: `Task ${id}`, status, backend: "claude", model: "opus", updatedAt: 0, asks: [], ...extra });
// A run record in `dir` whose Claude transcript (under `home`) ends with `text`, said at `ts`.
function said(home, dir, id, text, ts = 0) {
  const session = randomUUID();
  fs.mkdirSync(path.join(dir, ".rudder", "runs", id), { recursive: true });
  fs.writeFileSync(path.join(dir, ".rudder", "runs", id, "run.json"), JSON.stringify({ id, backend: "claude", task: "t", session: { nativeSessionId: session }, workspace: { path: dir } }));
  const transcripts = path.join(home, ".claude", "projects", dir.replace(/[^A-Za-z0-9-]/g, "-"));
  fs.mkdirSync(transcripts, { recursive: true });
  fs.writeFileSync(path.join(transcripts, `${session}.jsonl`), `${JSON.stringify({ type: "assistant", uuid: "a1", timestamp: new Date(ts).toISOString(), message: { role: "assistant", content: [{ type: "text", text }] } })}\n`);
}

test("events: started, finished, failed and a new prompt; nothing for what did not change", () => {
  const before = snapshot([project("web", [
    agent("a", "working"),
    agent("b", "working"),
    agent("c", "done"),
    agent("d", "working"),
    agent("g", "waiting"),
  ])]);
  const after = snapshot([project("web", [
    agent("a", "done"),
    agent("b", "failed"),
    agent("c", "done"),
    agent("d", "working", { asks: [{ id: "q1", kind: "question", input: { questions: [{ question: "Which db?", options: [{ label: "pg" }] }] } }] }),
    agent("e", "starting"),
    agent("f", "working", { pending: true }),
    agent("g", "failed"),
  ])]);
  const reply = (entry) => (entry.id === "a" ? { text: "Shipped the fix.", ts: 0 } : null);
  const events = agentEvents(before, after, reply).map((e) => `${e.kind} ${e.id}${e.detail ? `: ${e.detail}` : ""}`);
  assert.deepEqual(events, [
    "finished a: Shipped the fix.",
    "failed b",
    "needs you d: question: Which db? [pg] (prompt q1)",
    "started e",
    "failed g",
  ]);
});

test("wait_for_events returns what changed, or that the user wrote to the chat", async () => {
  const root = tmpdir();
  const caller = "run_all_1";
  const runFile = path.join(root, ".rudder", "runs", caller, "run.json");
  fs.mkdirSync(path.dirname(runFile), { recursive: true });
  fs.writeFileSync(runFile, JSON.stringify({ id: caller, lastUserInputAt: "1" }));
  const web = tmpdir();
  said(root, web, "a", "Shipped the fix.");
  said(root, web, "b", "Done with b.");
  const agents = [agent("a", "working"), agent("b", "working")];
  const projects = [project("web", agents, { dir: web }), project("rudder-all-agents", [], { dir: root, coordinator: true })];
  let clock = 0;
  let onTick = () => {};
  const tools = createCoordinatorTools({
    projects: () => projects,
    rpc: async () => ({ status: "delivered" }),
    home: root,
    now: () => clock,
    sleep: async (ms) => { clock += ms; onTick(clock); },
  });

  // Only the agents asked about count.
  onTick = (t) => {
    if (t > 3000) agents[1].status = "done";
    if (t > 6000) agents[0].status = "done";
  };
  assert.match(await tools.call("wait_for_events", { agents: ["a"], timeout_seconds: 60 }, { caller }), /^finished: web \/ Task a \(a\)/);
  // What that wait left out is still news to this chat.
  assert.match(await tools.call("wait_for_events", { timeout_seconds: 60 }, { caller }), /^finished: web \/ Task b \(b\)/);

  agents[0].status = "working";
  onTick = (t) => { if (t > 3000) fs.writeFileSync(runFile, JSON.stringify({ id: caller, lastUserInputAt: "2" })); };
  assert.equal(await tools.call("wait_for_events", {}, { caller }), "The user sent you a new message. Stop waiting and read it.");

  onTick = () => {};
  assert.equal(await tools.call("wait_for_events", { timeout_seconds: 5 }, { caller }), "Nothing changed in 5s.");
});

// Seen live: a one-word reply went working and done in 500ms, between the
// send and the wait, and the wait sat out its whole timeout.
test("a reply that finishes before the wait starts is reported at once, in its own words", async () => {
  const home = tmpdir();
  const web = tmpdir();
  said(home, web, "a", "Ready.", 150);
  said(home, web, "b", "Ready.", 150);
  // The list's preview trails the transcript.
  const agents = [agent("a", "done", { inputAt: 100, lastMessage: "Ready." }), agent("b", "done", { inputAt: 100, lastMessage: "Ready." })];
  const tools = createCoordinatorTools({
    projects: () => [project("web", agents, { dir: web })],
    rpc: async (method, params) => {
      // The dashboard types the message in: the record shows it, the turn has not begun.
      agents.find((a) => a.id === params.runId).inputAt = 200;
      return { status: "delivered" };
    },
    home,
    now: () => 0,
    sleep: async () => { throw new Error("waited"); },
  });
  const caller = "run_all_2";
  await tools.call("send_message", { agent: "a", text: "Reply with one word: pong." }, { caller });
  await tools.call("send_message", { agent: "b", text: "Reply with one word: pong." }, { caller });
  // a's whole turn is over; b has not started its own.
  said(home, web, "a", "pong", 900);
  assert.equal(await tools.call("wait_for_events", { timeout_seconds: 5 }, { caller }), "finished: web / Task a (a) · done\n  pong");
});

test("the MCP server lists and calls the tools over the supervisor's socket", async () => {
  const root = tmpdir();
  const socketPath = path.join(root, "all.sock");
  const sent = [];
  const tools = createCoordinatorTools({
    projects: () => [project("web", [agent("a", "working")])],
    rpc: async (method, params) => { sent.push({ method, params }); return { status: "delivered" }; },
    home: root,
  });
  const server = startCoordinatorBroker({ socketPath, tools, log: () => {} });
  await new Promise((resolve) => server.once("listening", resolve));
  const child = spawn(process.execPath, [path.resolve("cloud/worker/coordinator-mcp.mjs"), "--socket", socketPath], {
    env: { ...process.env, RUDDER_SCRATCH: path.join(root, "scratch", "run_all_1") },
    stdio: ["pipe", "pipe", "inherit"],
  });
  const replies = new Map();
  const waiting = new Map();
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line);
    (waiting.get(message.id) ?? ((value) => replies.set(message.id, value)))(message);
  });
  const call = (id, method, params) => {
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return replies.has(id) ? Promise.resolve(replies.get(id)) : new Promise((resolve) => waiting.set(id, resolve));
  };
  try {
    assert.equal((await call(1, "initialize", { protocolVersion: "2025-06-18" })).result.serverInfo.name, "rudder-all-agents");
    const names = (await call(2, "tools/list", {})).result.tools.map((t) => t.name);
    assert.deepEqual(names, ["list_agents", "read_agent", "send_message", "interrupt_agent", "start_agent", "answer_prompt", "wait_for_events"]);
    const listed = (await call(3, "tools/call", { name: "list_agents", arguments: {} })).result;
    assert.equal(listed.isError, false);
    assert.match(listed.content[0].text, /^web \[wsp_web\]\n- a · working · claude opus/);
    const sentReply = (await call(4, "tools/call", { name: "send_message", arguments: { agent: "a", text: "add a test" } })).result;
    assert.equal(sentReply.content[0].text, "Sent to web / Task a.");
    assert.deepEqual(sent, [{ method: "send", params: { workspaceId: "wsp_web", runId: "a", text: "add a test" } }]);
    const missing = (await call(5, "tools/call", { name: "read_agent", arguments: { agent: "zz" } })).result;
    assert.equal(missing.isError, true);
    assert.match(missing.content[0].text, /^no agent zz/);
  } finally {
    child.kill();
    server.close();
  }
});
