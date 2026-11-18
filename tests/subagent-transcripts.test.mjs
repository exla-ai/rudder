// Subagents in the app's chat (cloud/worker/transcripts.mjs): a Claude Code
// Agent call or a Codex spawn_agent call carries the subagent it started, read
// from the subagent's own log and followed as that log grows. Fixtures are
// synthetic, shaped like Claude Code 2.1.327 and Codex 0.169 logs.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const line = (entry) => `${JSON.stringify(entry)}\n`;
const jsonl = (entries) => entries.map(line).join("");

function setup(t, record) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-subagents-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const runDir = path.join(repo, ".rudder", "runs", record.id);
  fs.mkdirSync(runDir, { recursive: true });
  fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify({ repoRoot: path.join(root, "nowhere"), ...record }));
  return { home, repo };
}

const at = (seconds) => new Date(Date.UTC(2026, 10, 18, 9, 0, seconds)).toISOString();

test("Claude: each Agent call shows its subagent, done or running, and a running one updates as its log grows", (t) => {
  const session = "5e551011-0000-4000-8000-000000000001";
  const { home, repo } = setup(t, { id: "run_c", backend: "claude", status: "running", session: { nativeSessionId: session } });
  const project = path.join(home, ".claude", "projects", "-x");
  const subagents = path.join(project, session, "subagents");
  fs.mkdirSync(subagents, { recursive: true });
  const user = (uuid, parentUuid, content, ts, extra = {}) => ({ type: "user", uuid, parentUuid, timestamp: at(ts), message: { role: "user", content }, ...extra });
  const assistant = (uuid, parentUuid, content, ts) => ({ type: "assistant", uuid, parentUuid, timestamp: at(ts), message: { model: "claude-opus-5-5", content } });
  const agentCall = (id, description, prompt) => ({ type: "tool_use", id, name: "Agent", input: { description, subagent_type: "Explore", prompt, run_in_background: true } });
  const launched = (id, agentId) => [{ type: "tool_result", tool_use_id: id, content: [{ type: "text", text: `Async agent launched successfully.\nagentId: ${agentId}` }] }];
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl([
    user("u1", null, "settle bets when a market resolves", 0),
    assistant("a1", "u1", [agentCall("toolu_map", "Map resolution flow", "Find where markets resolve.")], 1),
    user("r1", "a1", launched("toolu_map", "amap"), 2, { toolUseResult: { status: "async_launched", agentId: "amap" } }),
    assistant("a2", "r1", [agentCall("toolu_pay", "Review payout rounding", "Check payout rounding.")], 3),
    user("r2", "a2", launched("toolu_pay", "apay"), 4),
    // A teammate is an Agent call too, but its own session: no metadata, no card.
    assistant("a3", "r2", [agentCall("toolu_team", "Teammate", "Work on the PR.")], 5),
    user("r3", "a3", [{ type: "tool_result", tool_use_id: "toolu_team", content: "Spawned successfully." }], 6),
    // Claude reports the background subagent's end in a task notification.
    { type: "attachment", uuid: "n1", parentUuid: "r3", timestamp: at(40), attachment: { type: "queued_command", prompt: "<task-notification>\n<task-id>amap</task-id>\n<tool-use-id>toolu_map</tool-use-id>\n<status>completed</status>\n<result>Nothing settles bets.</result>\n</task-notification>", origin: { kind: "task-notification" } } },
    assistant("a4", "n1", [{ type: "text", text: "Writing the settlement job." }], 41),
  ]));

  const side = (uuid, parentUuid, type, content, ts, agentId) => ({ type, uuid, parentUuid, isSidechain: true, agentId, timestamp: at(ts), message: { role: type, content } });
  fs.writeFileSync(path.join(subagents, "agent-amap.meta.json"), JSON.stringify({ agentType: "Explore", description: "Map resolution flow", toolUseId: "toolu_map", finished: true }));
  fs.writeFileSync(path.join(subagents, "agent-amap.jsonl"), jsonl([
    side("s1", null, "user", "Find where markets resolve.", 2, "amap"),
    { type: "api-request-blob", sessionId: session, agentId: "amap", message: { role: "system", content: "x".repeat(5000) } },
    side("s2", "s1", "assistant", [{ type: "tool_use", id: "g1", name: "Grep", input: { pattern: "resolveMarket" } }], 10, "amap"),
    side("s3", "s2", "user", [{ type: "tool_result", tool_use_id: "g1", content: "src/resolve.ts:12" }], 11, "amap"),
    side("s4", "s3", "assistant", [{ type: "text", text: "Nothing settles bets." }], 30, "amap"),
  ]));
  // Running: no `finished` yet.
  fs.writeFileSync(path.join(subagents, "agent-apay.meta.json"), JSON.stringify({ agentType: "general-purpose", description: "Review payout rounding", toolUseId: "toolu_pay" }));
  const payLog = path.join(subagents, "agent-apay.jsonl");
  fs.writeFileSync(payLog, jsonl([
    side("p1", null, "user", "Check payout rounding.", 4, "apay"),
    side("p2", "p1", "assistant", [{ type: "tool_use", id: "rd", name: "Read", input: { file_path: "src/payout.ts" } }], 12, "apay"),
  ]));

  const read = () => readTranscript(repo, "run_c", { home });
  const cards = (transcript) => transcript.messages.filter((m) => m.subagent).map((m) => m.subagent);
  const [done, running] = cards(read());
  assert.equal(cards(read()).length, 2, "the teammate gets no card");
  assert.deepEqual(
    { id: done.id, kind: done.kind, description: done.description, prompt: done.prompt, status: done.status, toolCalls: done.toolCalls, result: done.result, endedAt: done.endedAt },
    { id: "amap", kind: "Explore", description: "Map resolution flow", prompt: "Find where markets resolve.", status: "done", toolCalls: 1, result: "Nothing settles bets.", endedAt: Date.parse(at(30)) },
  );
  // Its own messages, without the prompt repeated as the first of them.
  assert.deepEqual(done.messages.map((m) => m.role), ["tool", "assistant"]);
  assert.equal(done.messages[0].tool.output, "src/resolve.ts:12");
  assert.equal(done.startedAt, Date.parse(at(1)));
  assert.deepEqual({ status: running.status, toolCalls: running.toolCalls, endedAt: running.endedAt, result: running.result }, { status: "running", toolCalls: 1, endedAt: null, result: null });

  // The log grows while the parent's turn goes on; half a line waits for the rest.
  const next = line(side("p3", "p2", "user", [{ type: "tool_result", tool_use_id: "rd", content: "export const payout = ..." }], 13, "apay"))
    + line(side("p4", "p3", "assistant", [{ type: "tool_use", id: "ed", name: "Edit", input: { file_path: "src/payout.ts" } }], 20, "apay"));
  fs.appendFileSync(payLog, next.slice(0, next.length - 30));
  let pay = cards(read())[1];
  assert.equal(pay.toolCalls, 1);
  assert.equal(pay.messages[0].tool.output, "export const payout = ...");
  fs.appendFileSync(payLog, next.slice(next.length - 30));
  pay = cards(read())[1];
  assert.equal(pay.toolCalls, 2);
  assert.deepEqual(pay.messages.map((m) => m.tool?.name), ["Read", "Edit"]);

  // It ends: Claude marks the metadata finished.
  fs.appendFileSync(payLog, line(side("p5", "p4", "assistant", [{ type: "text", text: "Rounded down to whole cents." }], 50, "apay")));
  fs.writeFileSync(path.join(subagents, "agent-apay.meta.json"), JSON.stringify({ agentType: "general-purpose", description: "Review payout rounding", toolUseId: "toolu_pay", finished: true }));
  pay = cards(read())[1];
  assert.deepEqual({ status: pay.status, result: pay.result, endedAt: pay.endedAt }, { status: "done", result: "Rounded down to whole cents.", endedAt: Date.parse(at(50)) });

  // An unfinished subagent of a run that is no longer alive stopped with it.
  fs.writeFileSync(path.join(subagents, "agent-apay.meta.json"), JSON.stringify({ agentType: "general-purpose", description: "Review payout rounding", toolUseId: "toolu_pay" }));
  const runFile = path.join(repo, ".rudder", "runs", "run_c", "run.json");
  fs.writeFileSync(runFile, JSON.stringify({ ...JSON.parse(fs.readFileSync(runFile, "utf8")), status: "paused" }));
  assert.equal(cards(read())[1].status, "stopped");
});

test("Codex: spawn_agent shows the subagent from its own rollout, past the parent history it inherited", (t) => {
  const uuid7 = (ms, tail) => {
    const hex = ms.toString(16).padStart(12, "0");
    return `${hex.slice(0, 8)}-${hex.slice(8)}-7${tail}`;
  };
  const spawnAt = Date.parse(at(10));
  const parentId = uuid7(Date.parse(at(0)), "000-8000-000000000001");
  const childId = uuid7(spawnAt + 40, "000-8000-000000000002");
  const otherId = uuid7(spawnAt + 60, "000-8000-000000000003");
  const { home, repo } = setup(t, { id: "run_x", backend: "codex", status: "running", session: { nativeSessionId: parentId } });
  const day = path.join(home, ".codex", "sessions", "2026", "11", "18");
  fs.mkdirSync(day, { recursive: true });
  const item = (ordinal, ts, payload) => ({ timestamp: at(ts), ordinal, type: "response_item", payload });
  const event = (ordinal, ts, payload) => ({ timestamp: at(ts), ordinal, type: "event_msg", payload });
  const spawnCall = (ordinal, ts) => item(ordinal, ts, { type: "function_call", name: "spawn_agent", namespace: "collaboration", call_id: "call_spawn", arguments: JSON.stringify({ task_name: "proc_net_parsing", message: "gAAAAAencrypted" }) });
  fs.writeFileSync(path.join(day, `rollout-2026-11-18T09-00-00-${parentId}.jsonl`), jsonl([
    { timestamp: at(0), ordinal: 0, type: "session_meta", payload: { id: parentId, cli_version: "0.169.0" } },
    item(1, 1, { type: "message", role: "user", content: [{ type: "input_text", text: "list ports per project" }] }),
    spawnCall(2, 10),
    item(3, 10, { type: "function_call_output", call_id: "call_spawn", output: JSON.stringify({ task_name: "/root/proc_net_parsing" }) }),
    item(4, 11, { type: "function_call", name: "spawn_agent", call_id: "call_bad", arguments: JSON.stringify({ task_name: "docker_ports", message: "gAAAA" }) }),
    item(5, 11, { type: "function_call_output", call_id: "call_bad", output: "agent limit reached" }),
  ]));
  const childMeta = (id, agentPath, parent) => ({ timestamp: at(10), ordinal: 0, type: "session_meta", payload: {
    id, forked_from_id: parent, subagent_history_start_ordinal: 4, agent_nickname: "Turing", agent_path: agentPath,
    source: { subagent: { thread_spawn: { parent_thread_id: parent, depth: 1, agent_path: agentPath, agent_nickname: "Turing", agent_role: null } } },
  } });
  const childLog = path.join(day, `rollout-2026-11-18T09-00-10-${childId}.jsonl`);
  fs.writeFileSync(childLog, jsonl([
    childMeta(childId, "/root/proc_net_parsing", parentId),
    // The parent's history, inherited: not the subagent's own.
    { timestamp: at(10), ordinal: 1, type: "session_meta", payload: { id: parentId } },
    { timestamp: at(10), ordinal: 2, type: "compacted", payload: { message: "y".repeat(5000) } },
    spawnCall(3, 10),
    event(4, 10, { type: "task_started", turn_id: "t1" }),
    item(5, 12, { type: "function_call", name: "exec_command", call_id: "c1", arguments: JSON.stringify({ cmd: "cat /proc/net/tcp6" }) }),
    item(6, 13, { type: "function_call_output", call_id: "c1", output: "sl local_address" }),
  ]));
  // Another parent's subagent with the same task name, spawned at nearly the same time.
  fs.writeFileSync(path.join(day, `rollout-2026-11-18T09-00-10-${otherId}.jsonl`), jsonl([childMeta(otherId, "/root/proc_net_parsing", "someone-else")]));

  const read = () => readTranscript(repo, "run_x", { home }).messages.filter((m) => m.subagent);
  let [sub, failed] = read().map((m) => m.subagent);
  assert.equal(read()[0].tool.input, "proc_net_parsing", "the spawn shows its task name, not the encrypted message");
  assert.deepEqual(
    { id: sub.id, name: sub.name, description: sub.description, status: sub.status, toolCalls: sub.toolCalls, startedAt: sub.startedAt },
    { id: childId, name: "Turing", description: "proc net parsing", status: "running", toolCalls: 1, startedAt: spawnAt },
  );
  assert.deepEqual(sub.messages.map((m) => [m.role, m.tool?.input]), [["tool", "cat /proc/net/tcp6"]]);
  assert.deepEqual({ status: failed.status, result: failed.result }, { status: "failed", result: "agent limit reached" });

  fs.appendFileSync(childLog, jsonl([
    item(7, 20, { type: "message", role: "assistant", content: [{ type: "output_text", text: "Parsing works." }] }),
    event(8, 20, { type: "task_complete", turn_id: "t1", last_agent_message: "Parsing works." }),
  ]));
  [sub] = read().map((m) => m.subagent);
  assert.deepEqual({ status: sub.status, result: sub.result, endedAt: sub.endedAt, toolCalls: sub.toolCalls }, { status: "done", result: "Parsing works.", endedAt: Date.parse(at(20)), toolCalls: 1 });
});
