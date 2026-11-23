// The chat's subagent tray (cloud/worker/subagents.mjs): every Claude Code
// Agent-tool subagent and teammate, every Codex spawn_agent subagent and every
// Antigravity invoke_subagent subagent an agent started, found from their own
// records. Fixtures are synthetic, shaped like Claude Code 2.1.327, Codex 0.169
// and agy 1.5.14 logs.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { listSubagents } from "../cloud/worker/subagents.mjs";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const line = (entry) => `${JSON.stringify(entry)}\n`;
const jsonl = (entries) => entries.map(line).join("");
const at = (seconds) => new Date(Date.UTC(2026, 10, 18, 9, 0, seconds)).toISOString();
// A pid no process has: the agent's process is gone.
const DEAD_PID = 2 ** 22 + 17;

function setup(t, record) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-subagent-tray-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const runFile = path.join(repo, ".rudder", "runs", record.id, "run.json");
  fs.mkdirSync(path.dirname(runFile), { recursive: true });
  const write = (fields) => fs.writeFileSync(runFile, JSON.stringify({ repoRoot: path.join(root, "nowhere"), ...record, ...fields }));
  write({});
  return { home, repo, write };
}

const byName = (list) => Object.fromEntries(list.map((s) => [s.name || s.description, s]));

test("Claude: the tray lists every Agent subagent and every teammate of the session's team, running, done or stopped", (t) => {
  const session = "5e551011-0000-4000-8000-0000000000c1";
  // The run says completed (the lead's turn ended) while its process, and so its teammates, live on.
  const { home, repo, write } = setup(t, { id: "run_t", backend: "claude", status: "completed", process: { pid: process.pid }, session: { nativeSessionId: session } });
  const project = path.join(home, ".claude", "projects", "-x");
  const subagents = path.join(project, session, "subagents");
  fs.mkdirSync(subagents, { recursive: true });
  const side = (uuid, type, content, ts) => ({ type, uuid, isSidechain: true, timestamp: at(ts), message: { role: type, content } });
  const tool = (id, name, input) => ({ type: "tool_use", id, name, input });
  // The lead's own log: its Agent calls scrolled away long ago; the tray never reads it.
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl([{ type: "user", uuid: "u1", timestamp: at(0), message: { role: "user", content: "ship it" } }]));

  const agent = (id, meta, entries) => {
    fs.writeFileSync(path.join(subagents, `agent-${id}.meta.json`), JSON.stringify(meta));
    fs.writeFileSync(path.join(subagents, `agent-${id}.jsonl`), jsonl(entries));
  };
  agent("adone", { agentType: "Explore", description: "Map the flow", toolUseId: "toolu_1", finished: true }, [
    side("d1", "user", "Find where markets resolve.", 1),
    side("d2", "assistant", [tool("g1", "Grep", { pattern: "resolve" })], 2),
    side("d3", "assistant", [{ type: "text", text: "**Nothing** settles bets." }], 9),
  ]);
  agent("arun", { agentType: "general-purpose", description: "Fix rounding", toolUseId: "toolu_2" }, [
    side("r1", "user", "Fix payout rounding.", 3),
    side("r2", "assistant", [tool("b1", "Bash", { command: "npm test" })], 20),
  ]);
  // Stopped from outside: never marked finished, ends on the interrupt note.
  agent("akill", { agentType: "general-purpose", description: "Build the Mac app", toolUseId: "toolu_3" }, [
    side("k1", "user", "Build it.", 4),
    side("k2", "assistant", [tool("x1", "Bash", { command: "xcodebuild" })], 5),
    side("k3", "user", "[Request interrupted by user for tool use]", 6),
  ]);
  // A subagent of a subagent shows inside its parent, not in the tray.
  agent("anest", { agentType: "Explore", description: "Nested look", toolUseId: "toolu_n", parentAgentId: "arun" }, [side("n1", "user", "Look.", 21)]);

  // The team this session leads, and one another session leads.
  const team = `session-${session.slice(0, 8)}`;
  const writeTeam = (name, lead, members) => {
    fs.mkdirSync(path.join(home, ".claude", "teams", name), { recursive: true });
    fs.writeFileSync(path.join(home, ".claude", "teams", name, "config.json"), JSON.stringify({ name, leadSessionId: lead, members }));
  };
  const member = (name, prompt) => ({ name, prompt, agentType: "general-purpose", backendType: "tmux" });
  writeTeam(team, session, [member("team-lead"), member("pr", "Open the PR."), member("docs", "Write the docs.")]);
  writeTeam("session-0ther000", "0ther000-0000-4000-8000-000000000000", [member("pr", "Open the PR.")]);
  const mate = (file, teamName, name, prompt, entries) => fs.writeFileSync(path.join(project, `${file}.jsonl`), jsonl([
    { type: "agent-setting", agentSetting: "general-purpose" },
    { type: "user", uuid: `${file}-0`, teamName, agentName: name, timestamp: at(10), message: { role: "user", content: `<teammate-message teammate_id="team-lead">\n${prompt}\n</teammate-message>` } },
    ...entries,
  ]));
  const say = (uuid, text, ts) => ({ type: "assistant", uuid, timestamp: at(ts), message: { content: [{ type: "text", text }] } });
  const call = (uuid, name, input, ts) => ({ type: "assistant", uuid, timestamp: at(ts), message: { content: [tool(`${uuid}-t`, name, input)] } });
  mate("7ea77a7e-0000-4000-8000-0000000000c1", team, "pr", "Open the PR.", [call("p1", "Bash", { command: "gh pr create" }, 30)]);
  mate("7ea77a7e-0000-4000-8000-0000000000c2", team, "docs", "Write the docs.", [say("w1", "Docs written.", 25), { type: "system", subtype: "turn_duration", durationMs: 9000, timestamp: at(26) }]);
  // One that finished and agreed to shut down: its process exited, it left the team.
  mate("7ea77a7e-0000-4000-8000-0000000000c5", team, "review", "Review the diff.", [
    call("v1", "SendMessage", { to: "team-lead", message: { type: "shutdown_response", request_id: "s1", approve: true } }, 27),
    { type: "cost-state" },
  ]);
  // An earlier "pr" of the team, shut down mid-turn: no longer a member, though its log never ended.
  mate("7ea77a7e-0000-4000-8000-0000000000c3", team, "pr", "Open the old PR.", [call("o1", "Bash", { command: "git push" }, 12)]);
  // Another lead's teammate.
  mate("7ea77a7e-0000-4000-8000-0000000000c4", "session-0ther000", "pr", "Open the PR.", [call("q1", "Read", { file_path: "x" }, 13)]);

  const list = () => listSubagents(repo, "run_t", { home });
  let tray = list();
  assert.deepEqual(tray.subagents.map((s) => [s.type, s.name || s.description, s.status]), [
    ["teammate", "pr", "running"],
    ["subagent", "Fix rounding", "running"],
    ["teammate", "review", "done"],
    ["teammate", "docs", "done"],
    ["teammate", "pr", "stopped"],
    ["subagent", "Map the flow", "done"],
    ["subagent", "Build the Mac app", "stopped"],
  ], "running first, then the rest by when they ended");
  assert.equal(tray.open, null);
  const s = byName(tray.subagents);
  assert.deepEqual(
    { kind: s["Map the flow"].kind, startedAt: s["Map the flow"].startedAt, endedAt: s["Map the flow"].endedAt, toolCalls: s["Map the flow"].toolCalls, step: s["Map the flow"].step },
    { kind: "Explore", startedAt: Date.parse(at(1)), endedAt: Date.parse(at(9)), toolCalls: 1, step: "**Nothing** settles bets." },
  );
  assert.deepEqual([s["Fix rounding"].step, s["Fix rounding"].endedAt], ["Bash npm test", null]);
  assert.equal(s["Build the Mac app"].step, "Bash xcodebuild", "an interrupted one says what it was doing");
  assert.equal(tray.subagents[0].id, "7ea77a7e-0000-4000-8000-0000000000c1");
  assert.deepEqual([tray.subagents[0].startedAt, tray.subagents[0].step], [Date.parse(at(10)), "Bash gh pr create"]);

  // Opened: its activity as the chat's cards carry it, prompt included.
  const open = listSubagents(repo, "run_t", { home, open: "adone" }).open;
  assert.deepEqual({ prompt: open.prompt, result: open.result, roles: open.messages.map((m) => m.role), earlier: open.earlier },
    { prompt: "Find where markets resolve.", result: "**Nothing** settles bets.", roles: ["tool", "assistant"], earlier: 0 });
  assert.equal(listSubagents(repo, "run_t", { home, open: "7ea77a7e-0000-4000-8000-0000000000c2" }).open.prompt, "Write the docs.");
  assert.equal(listSubagents(repo, "run_t", { home, open: "gone" }).open, null);

  // A running one gains a step; a member leaves the team (shut down) and stops.
  fs.appendFileSync(path.join(subagents, "agent-arun.jsonl"), line(side("r3", "assistant", [tool("e1", "Edit", { file_path: "src/payout.ts" })], 40)));
  writeTeam(team, session, [member("team-lead"), member("docs", "Write the docs.")]);
  tray = list();
  assert.equal(byName(tray.subagents)["Fix rounding"].step, "Edit src/payout.ts");
  assert.deepEqual(tray.subagents.filter((x) => x.name === "pr").map((x) => x.status), ["stopped", "stopped"]);

  // The agent's process is gone: nothing of it runs any more.
  write({ status: "paused", process: { pid: DEAD_PID } });
  assert.deepEqual(list().subagents.filter((x) => x.status === "running"), []);
});

test("Codex: the tray lists every thread that names the agent's thread as its parent", (t) => {
  const uuid7 = (ms, tail) => {
    const hex = ms.toString(16).padStart(12, "0");
    return `${hex.slice(0, 8)}-${hex.slice(8)}-7${tail}`;
  };
  const parentId = uuid7(Date.parse(at(0)), "000-8000-0000000000d1");
  const childId = uuid7(Date.parse(at(10)), "000-8000-0000000000d2");
  const otherId = uuid7(Date.parse(at(11)), "000-8000-0000000000d3");
  const { home, repo } = setup(t, { id: "run_x", backend: "codex", status: "running", process: { pid: process.pid }, session: { nativeSessionId: parentId } });
  const day = path.join(home, ".codex", "sessions", "2026", "11", "18");
  fs.mkdirSync(day, { recursive: true });
  const item = (ordinal, ts, payload) => ({ timestamp: at(ts), ordinal, type: "response_item", payload });
  const event = (ordinal, ts, payload) => ({ timestamp: at(ts), ordinal, type: "event_msg", payload });
  fs.writeFileSync(path.join(day, `rollout-2026-11-18T09-00-00-${parentId}.jsonl`), jsonl([{ timestamp: at(0), ordinal: 0, type: "session_meta", payload: { id: parentId } }]));
  const childMeta = (id, parent) => ({ timestamp: at(10), ordinal: 0, type: "session_meta", payload: {
    id, subagent_history_start_ordinal: 2, agent_nickname: "Turing", agent_path: "/root/proc_net_parsing",
    source: { subagent: { thread_spawn: { parent_thread_id: parent, depth: 1 } } },
  } });
  const childLog = path.join(day, `rollout-2026-11-18T09-00-10-${childId}.jsonl`);
  fs.writeFileSync(childLog, jsonl([
    childMeta(childId, parentId),
    { timestamp: at(10), ordinal: 1, type: "session_meta", payload: { id: parentId } },
    event(2, 10, { type: "task_started", turn_id: "t1" }),
    item(3, 12, { type: "function_call", name: "exec_command", call_id: "c1", arguments: JSON.stringify({ cmd: "cat /proc/net/tcp6" }) }),
  ]));
  fs.writeFileSync(path.join(day, `rollout-2026-11-18T09-00-11-${otherId}.jsonl`), jsonl([childMeta(otherId, "someone-else")]));

  let [sub, ...rest] = listSubagents(repo, "run_x", { home }).subagents;
  assert.deepEqual(rest, []);
  assert.deepEqual(
    { id: sub.id, type: sub.type, name: sub.name, description: sub.description, status: sub.status, startedAt: sub.startedAt, step: sub.step },
    { id: childId, type: "subagent", name: "Turing", description: "proc net parsing", status: "running", startedAt: Date.parse(at(10)), step: "exec_command cat /proc/net/tcp6" },
  );
  fs.appendFileSync(childLog, jsonl([
    item(4, 20, { type: "message", role: "assistant", content: [{ type: "output_text", text: "Parsing works." }] }),
    event(5, 20, { type: "task_complete", turn_id: "t1", last_agent_message: "Parsing works." }),
  ]));
  const tray = listSubagents(repo, "run_x", { home, open: childId });
  [sub] = tray.subagents;
  assert.deepEqual({ status: sub.status, endedAt: sub.endedAt, step: sub.step }, { status: "done", endedAt: Date.parse(at(20)), step: "Parsing works." });
  assert.deepEqual(tray.open.messages.map((m) => m.role), ["tool", "assistant"]);
});

test("Antigravity: the tray lists the subagents agy filed under the conversation, done once a reply ends their turn", (t) => {
  const parentId = "eaff9cce-8c16-45b6-b055-da500727ba34";
  const { home, repo, write } = setup(t, { id: "run_g", backend: "antigravity", status: "running", process: { pid: process.pid }, session: { nativeSessionId: parentId } });
  const brain = path.join(home, ".gemini", "antigravity-cli", "brain");
  const transcript = (id, steps) => {
    const logs = path.join(brain, id, ".system_generated", "logs");
    fs.mkdirSync(logs, { recursive: true });
    fs.writeFileSync(path.join(logs, "transcript_full.jsonl"), jsonl(steps));
    return path.join(logs, "transcript_full.jsonl");
  };
  transcript(parentId, [{ step_index: 0, source: "USER_EXPLICIT", type: "USER_INPUT", status: "DONE", created_at: at(0), content: "<USER_REQUEST>\naudit it\n</USER_REQUEST>" }]);
  // Shaped like agy 1.5.14's listing: `state` stays ALIVE after a subagent finishes.
  const listed = path.join(brain, parentId, ".system_generated", "subagents");
  fs.mkdirSync(listed, { recursive: true });
  const list = (id, typeName, role) => fs.writeFileSync(path.join(listed, `${id}.json`), JSON.stringify({
    conversationId: id, subagentDescriptor: { typeName, role }, state: "SUBAGENT_STATE_ALIVE", spawnStepIndex: 3, workspaceUris: ["file:///w"],
  }));
  const asked = (ts, prompt) => ({ step_index: 0, source: "SYSTEM", type: "SYSTEM_MESSAGE", status: "DONE", created_at: at(ts),
    content: `The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=${at(ts)} sender=${parentId} priority=MESSAGE_PRIORITY_HIGH content=${prompt}\n</SYSTEM_MESSAGE>` });
  const reply = (index, ts, content, calls = []) => ({ step_index: index, source: "MODEL", type: "PLANNER_RESPONSE", status: "DONE", created_at: at(ts), content, tool_calls: calls });
  const result = (index, ts, content) => ({ step_index: index, source: "MODEL", type: "GENERIC", status: "DONE", created_at: at(ts), content: `Created At: ${at(ts)}\nCompleted At: ${at(ts)}\n${content}` });

  const doneId = "0d6b9b6e-f9f3-4b3d-8a0c-3cbed5e475f7";
  const runningId = "6dbb782b-252e-421b-9e06-b30e03bb8740";
  const startingId = "61445077-562e-40cf-a703-6b468d8b156d";
  transcript(parentId, [
    { step_index: 0, source: "USER_EXPLICIT", type: "USER_INPUT", status: "DONE", created_at: at(0), content: "<USER_REQUEST>\naudit it\n</USER_REQUEST>" },
    reply(1, 1, "", [
      { name: "invoke_subagent", args: { TypeName: '"research"', Role: '"Examples Auditor"', Prompt: '"Read every file in examples/."' } },
      { name: "invoke_subagent", args: { TypeName: '"research"', Role: '"Test Suite Auditor"', Prompt: '"Read the tests."' } },
    ]),
    result(2, 2, `Created the following subagent:\n{\n  "conversationId": "${doneId}"\n}`),
    result(3, 2, `Created the following subagent:\n{\n  "conversationId": "${runningId}"\n}`),
  ]);
  list(doneId, "research", "Examples Auditor");
  transcript(doneId, [
    asked(1, "Read every file in `examples/`.\nReport what is broken."),
    reply(1, 2, "", [{ name: "read_file", args: { Path: '"/w/examples/README.md"' } }]),
    result(2, 3, "1: # Examples"),
    reply(3, 4, "", [{ name: "send_message", args: { Message: "**Two** examples are broken: session/redis.js and auth." } }]),
    result(4, 5, `Message sent to "${parentId}".`),
    reply(5, 6, "Report sent to the parent agent."),
    // A trailing system note after the final reply must not revert done -> running.
    { step_index: 6, source: "SYSTEM", type: "SYSTEM_MESSAGE", status: "DONE", created_at: at(7), content: "<SYSTEM_MESSAGE>Reminder: idle</SYSTEM_MESSAGE>" },
  ]);
  list(runningId, "research", "Test Suite Auditor");
  const runningLog = transcript(runningId, [
    asked(1, "Read the tests."),
    reply(1, 7, "", [{ name: "run_command", args: { CommandLine: '"npm test"' } }]),
  ]);
  // Listed, its conversation not written yet: shown immediately as running so
  // the tray button appears left of the message bar without waiting. A torn
  // JSON file agy is still writing is skipped.
  list(startingId, "research", "Lib Code Auditor");
  fs.writeFileSync(path.join(listed, "2f6a2ad6-38d3-4918-b1b3-94c43ed1165b.json"), '{"conversationId":"2f6a');

  const tray = listSubagents(repo, "run_g", { home, open: doneId });
  const subs = byName(tray.subagents);
  assert.deepEqual(Object.keys(subs).sort(), ["Examples Auditor", "Lib Code Auditor", "Test Suite Auditor"]);
  assert.equal(subs["Lib Code Auditor"].status, "running");
  assert.equal(subs["Test Suite Auditor"].status, "running");
  assert.equal(subs["Examples Auditor"].status, "done");
  assert.deepEqual(
    { backend: subs["Examples Auditor"].backend, kind: subs["Examples Auditor"].kind, status: subs["Examples Auditor"].status, startedAt: subs["Examples Auditor"].startedAt, toolCalls: subs["Examples Auditor"].toolCalls, step: subs["Examples Auditor"].step },
    { backend: "antigravity", kind: "research", status: "done", startedAt: Date.parse(at(1)), toolCalls: 2, step: "**Two** examples are broken: session/redis.js and auth." },
  );
  // Inline subagent cards appear on invoke_subagent calls in the chat too.
  const chat = readTranscript(repo, "run_g", { home });
  const cards = chat.messages.filter((m) => m.subagent).map((m) => [m.subagent.name, m.subagent.status, m.subagent.result]);
  assert.deepEqual(cards, [
    ["Examples Auditor", "done", "**Two** examples are broken: session/redis.js and auth."],
    ["Test Suite Auditor", "running", null],
  ]);
  assert.deepEqual(
    { status: subs["Test Suite Auditor"].status, endedAt: subs["Test Suite Auditor"].endedAt, step: subs["Test Suite Auditor"].step },
    { status: "running", endedAt: null, step: "run_command npm test" },
  );
  assert.equal(tray.open.prompt, "Read every file in `examples/`.\nReport what is broken.");
  assert.equal(tray.open.result, "**Two** examples are broken: session/redis.js and auth.");
  assert.deepEqual(tray.open.messages.map((m) => m.role), ["tool", "tool", "assistant"]);
  assert.equal(tray.open.earlier, 0);

  // Its reply with no tool call ends its turn.
  fs.appendFileSync(runningLog, jsonl([result(2, 8, "Command exited with code 0."), reply(3, 9, "All tests pass.")]));
  assert.deepEqual(
    (({ status, endedAt, step }) => ({ status, endedAt, step }))(byName(listSubagents(repo, "run_g", { home }).subagents)["Test Suite Auditor"]),
    { status: "done", endedAt: Date.parse(at(9)), step: "All tests pass." },
  );
  // A subagent mid-turn when the agent's process is gone was stopped.
  fs.appendFileSync(runningLog, jsonl([asked(10, "Also read the benchmarks."), reply(4, 11, "", [{ name: "list_dir", args: { DirectoryPath: '"/w/benchmarks"' } }])]));
  write({ process: { pid: DEAD_PID } });
  assert.equal(byName(listSubagents(repo, "run_g", { home }).subagents)["Test Suite Auditor"].status, "stopped");
});

test("a restarted home serves an unchanged subagent's line without reading its log", async (t) => {
  // Following every log from the start blocked the home for 451 ms on the
  // first tray poll of libra hi-lsup5 (29 subagents, 270 MB of logs).
  const session = "5e551011-0000-4000-8000-0000000000e1";
  const { home, repo } = setup(t, { id: "run_r", backend: "claude", status: "completed", process: { pid: DEAD_PID }, session: { nativeSessionId: session } });
  fs.mkdirSync(path.join(home, ".rudder"), { recursive: true });
  const project = path.join(home, ".claude", "projects", "-x");
  const subagents = path.join(project, session, "subagents");
  fs.mkdirSync(subagents, { recursive: true });
  fs.writeFileSync(path.join(project, `${session}.jsonl`), "");
  const side = (uuid, type, content, ts) => ({ type, uuid, isSidechain: true, timestamp: at(ts), message: { role: type, content } });
  const log = path.join(subagents, "agent-afin.jsonl");
  fs.writeFileSync(path.join(subagents, "agent-afin.meta.json"), JSON.stringify({ agentType: "Explore", description: "Map it", toolUseId: "toolu_f", finished: true }));
  fs.writeFileSync(log, jsonl([side("f1", "user", "Map the flow.", 1), side("f2", "assistant", [{ type: "text", text: "Mapped." }], 5)]));
  const first = listSubagents(repo, "run_r", { home }).subagents;
  assert.equal(first[0].step, "Mapped.");
  const saved = path.join(home, ".rudder", "subagent-tray.json");
  await waitFor(() => fs.existsSync(saved));

  // Unreadable, same size and mtime: a home that read it would fail.
  fs.chmodSync(log, 0);
  const restarted = () => {
    const run = spawnSync(process.execPath, ["--input-type=module", "-e", `
      const { listSubagents } = await import(${JSON.stringify(new URL("../cloud/worker/subagents.mjs", import.meta.url).href)});
      process.stdout.write(JSON.stringify(listSubagents(${JSON.stringify(repo)}, "run_r", { home: ${JSON.stringify(home)} }).subagents));
    `], { encoding: "utf8" });
    assert.equal(run.status, 0, run.stderr);
    return JSON.parse(run.stdout);
  };
  assert.deepEqual(restarted(), first);

  // A log that grew is read again.
  fs.chmodSync(log, 0o644);
  fs.appendFileSync(log, line(side("f3", "assistant", [{ type: "text", text: "Mapped twice." }], 9)));
  assert.equal(restarted()[0].step, "Mapped twice.");
});

async function waitFor(check, timeout = 5000) {
  const start = Date.now();
  while (!check()) {
    if (Date.now() - start > timeout) throw new Error("timed out");
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}
