// Reviews and second opinions an agent gets from another harness show in its
// subagent tray: commands found in the process tree under the agent
// (cloud/worker/outside-agents.mjs), and helpers it started with agents_start.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { listSubagents } from "../cloud/worker/subagents.mjs";
import { agentProcesses, watchOutsideAgents } from "../cloud/worker/outside-agents.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");

function setup(t) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "rudder-outside-agents-")));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "code", "app");
  fs.mkdirSync(path.join(home, ".rudder"), { recursive: true });
  const run = (record) => {
    const file = path.join(repo, ".rudder", "runs", record.id, "run.json");
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, JSON.stringify({ repoRoot: path.join(root, "nowhere"), ...record }));
  };
  return { root, home, repo, run };
}

const uuid7 = (ms, tail) => {
  const hex = ms.toString(16).padStart(12, "0");
  return `${hex.slice(0, 8)}-${hex.slice(8)}-7${tail}`;
};

test("a Codex agent's codex exec, agy -p and claude -p are found under its process and listed with their models", (t) => {
  const { root, home, repo, run } = setup(t);
  const now = Date.now();
  const iso = (ms) => new Date(ms).toISOString();
  const hostThread = uuid7(now - 60_000, "000-8000-0000000000a1");
  run({ id: "run_c", backend: "codex", status: "running", process: { pid: 500 }, session: { nativeSessionId: hostThread } });
  const day = new Date(now);
  const sessions = path.join(home, ".codex", "sessions", String(day.getUTCFullYear()), String(day.getUTCMonth() + 1).padStart(2, "0"), String(day.getUTCDate()).padStart(2, "0"));
  fs.mkdirSync(sessions, { recursive: true });
  fs.writeFileSync(path.join(sessions, `rollout-x-${hostThread}.jsonl`), jsonl([{ timestamp: iso(now - 60_000), ordinal: 0, type: "session_meta", payload: { id: hostThread, cwd: repo } }]));

  // codex exec: a thread started then, in the command's directory. Another
  // thread started then somewhere else is not it.
  const work = path.join(root, "work");
  const review = uuid7(now - 1_000, "000-8000-0000000000a2");
  const elsewhere = uuid7(now - 900, "000-8000-0000000000a3");
  fs.writeFileSync(path.join(sessions, `rollout-x-${elsewhere}.jsonl`), jsonl([{ timestamp: iso(now), ordinal: 0, type: "session_meta", payload: { id: elsewhere, cwd: "/somewhere/else" } }]));
  const rollout = path.join(sessions, `rollout-x-${review}.jsonl`);
  fs.writeFileSync(rollout, jsonl([
    { timestamp: iso(now - 1_000), ordinal: 0, type: "session_meta", payload: { id: review, cwd: work } },
    { timestamp: iso(now - 1_000), ordinal: 1, type: "turn_context", payload: { model: "gpt-9" } },
    { timestamp: iso(now - 1_000), ordinal: 2, type: "event_msg", payload: { type: "task_started", turn_id: "t1" } },
    { timestamp: iso(now - 500), ordinal: 3, type: "response_item", payload: { type: "function_call", name: "exec_command", call_id: "c1", arguments: JSON.stringify({ cmd: "git diff" }) } },
  ]));
  // agy -p: the conversation whose first request is the command's prompt.
  const brain = (id, prompt) => {
    const logs = path.join(home, ".gemini", "antigravity-cli", "brain", id, ".system_generated", "logs");
    fs.mkdirSync(logs, { recursive: true });
    fs.writeFileSync(path.join(logs, "transcript_full.jsonl"), jsonl([
      { step_index: 0, source: "USER", type: "USER_INPUT", status: "DONE", created_at: iso(now).replace(/\.\d+Z$/, "Z"), content: `<USER_REQUEST>\n${prompt}\n</USER_REQUEST>` },
      { step_index: 1, source: "MODEL", type: "PLANNER_RESPONSE", status: "DONE", created_at: iso(now).replace(/\.\d+Z$/, "Z"), model: "gemini-9", content: "Looks right.", tool_calls: [] },
    ]));
  };
  brain("agy-review", "Review the payout rounding in lib/payout.ts");
  brain("agy-other", "Summarize the README for a newcomer");
  // claude -p: the session started then in the directory's project folder.
  const project = path.join(home, ".claude", "projects", work.replace(/[^A-Za-z0-9]/g, "-"));
  fs.mkdirSync(project, { recursive: true });
  fs.writeFileSync(path.join(project, "c1a0de00-0000-4000-8000-0000000000a4.jsonl"), jsonl([
    { type: "user", uuid: "u1", timestamp: iso(now), message: { role: "user", content: "Second opinion on the retry loop." } },
    { type: "assistant", uuid: "a1", timestamp: iso(now), message: { role: "assistant", model: "claude-9", content: [{ type: "text", text: "It can spin forever." }] } },
  ]));

  const proc = (pid, ppid, comm, cmdline, cwd = work) => [pid, { pid, ppid, comm, cmdline, cwd, ageMs: 800 }];
  const processes = new Map([
    proc(500, 1, "codex", "codex -c x=y", repo),
    proc(501, 500, "bash", "bash -lc codex exec ..."),
    proc(502, 501, "codex", "codex exec -m gpt-9 Review the diff"),
    proc(503, 501, "agy", "agy -p Review the payout rounding in lib/payout.ts --model gemini-9"),
    proc(504, 501, "claude", "claude -p Second opinion on the retry loop."),
    // Not launched by this agent, and not a one-shot run.
    proc(600, 1, "codex", "codex exec elsewhere"),
    proc(505, 501, "claude", "claude --resume abc"),
  ]);
  assert.deepEqual(agentProcesses(path.join(root, "code")), [{ repoDir: repo, runId: "run_c", pid: 500 }]);
  watchOutsideAgents({ home, agents: agentProcesses(path.join(root, "code")), processes, now });

  const tray = () => Object.fromEntries(listSubagents(repo, "run_c", { home }).subagents.map((s) => [s.kind, s]));
  let found = tray();
  assert.deepEqual(Object.keys(found).sort(), ["Antigravity", "Claude", "Codex"]);
  assert.deepEqual(
    { id: found.Codex.id, backend: found.Codex.backend, model: found.Codex.model, status: found.Codex.status, toolCalls: found.Codex.toolCalls },
    { id: review, backend: "codex", model: "gpt-9", status: "running", toolCalls: 1 },
  );
  assert.deepEqual(
    { id: found.Antigravity.id, model: found.Antigravity.model, status: found.Antigravity.status, description: found.Antigravity.description },
    { id: "agy-review", model: "gemini-9", status: "done", description: "Review the payout rounding in lib/payout.ts" },
  );
  assert.deepEqual({ model: found.Claude.model, status: found.Claude.status }, { model: "claude-9", status: "running" });

  // The commands end: Claude's reply is its result; Codex never finished its turn.
  watchOutsideAgents({ home, agents: agentProcesses(path.join(root, "code")), processes: new Map([proc(500, 1, "codex", "codex -c x=y", repo)]), now: now + 5_000 });
  found = tray();
  assert.deepEqual({ status: found.Claude.status, step: found.Claude.step }, { status: "done", step: "It can spin forever." });
  assert.equal(found.Codex.status, "stopped");
  const opened = listSubagents(repo, "run_c", { home, open: "agy-review" }).open;
  assert.deepEqual(opened.messages.map((m) => m.role), ["user", "assistant"]);
});

test("a helper started with agents_start is listed under the agent that asked, on the model it was started on", (t) => {
  const { home, repo, run } = setup(t);
  run({ id: "run_lead", backend: "codex", status: "running", process: { pid: process.pid }, session: { nativeSessionId: uuid7(Date.now(), "000-8000-0000000000b1") } });
  run({ id: "run_other", backend: "claude", status: "running", process: { pid: process.pid }, parentRunId: "someone-else", session: { nativeSessionId: "none" } });
  run({ id: "run_helper", backend: "antigravity", model: "gemini-4-neon-xhigh", status: "completed", taskSummary: "Review lib and test", createdAt: 1_800_000_000_000, parentRunId: "run_lead", process: { pid: process.pid }, session: { nativeSessionId: "helper-conv" } });
  const logs = path.join(home, ".gemini", "antigravity-cli", "brain", "helper-conv", ".system_generated", "logs");
  fs.mkdirSync(logs, { recursive: true });
  fs.writeFileSync(path.join(logs, "transcript_full.jsonl"), jsonl([
    { step_index: 0, source: "USER", type: "USER_INPUT", status: "DONE", created_at: "2027-01-15T08:00:00Z", content: "<USER_REQUEST>\nReview lib/ and test/.\n</USER_REQUEST>" },
    { step_index: 1, source: "MODEL", type: "PLANNER_RESPONSE", status: "DONE", created_at: "2027-01-15T08:00:09Z", content: "Two bugs in lib/pay.ts.", tool_calls: [] },
  ]));
  const tray = listSubagents(repo, "run_lead", { home, open: "run_helper" });
  assert.deepEqual(
    tray.subagents.map((s) => ({ id: s.id, backend: s.backend, kind: s.kind, model: s.model, description: s.description, status: s.status, step: s.step })),
    [{ id: "run_helper", backend: "antigravity", kind: "Helper", model: "gemini-4-neon-xhigh", description: "Review lib and test", status: "done", step: "Two bugs in lib/pay.ts." }],
  );
  assert.deepEqual(tray.open.messages.map((m) => m.text), ["Review lib/ and test/.", "Two bugs in lib/pay.ts."]);
});
