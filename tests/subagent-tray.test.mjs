// The chat's subagent tray (cloud/worker/subagents.mjs): every Claude Code
// Agent-tool subagent and teammate, and every Codex spawn_agent subagent, an
// agent started, found from their own records. Fixtures are synthetic, shaped
// like Claude Code 2.1.327 and Codex 0.169 logs.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { listSubagents } from "../cloud/worker/subagents.mjs";

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
