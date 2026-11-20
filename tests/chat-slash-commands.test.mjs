// /goal and /compact sent from the chat (cloud/worker/transcripts.mjs,
// cloud/app/pending.js). The agent's CLI runs a slash command instead of
// recording what was typed, so the sent bubble used to wait out its five
// minutes at the end of the chat, under everything the agent did next, and
// Claude's goal check-ins and Codex's goal turns showed nothing or showed as
// the user. Fixtures are cut from real Claude Code 2.1.328 and Codex 0.170
// sessions, their words replaced.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readTranscript } from "../cloud/worker/transcripts.mjs";
import { sentAfter, settlePending } from "../cloud/app/pending.js";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const at = (seconds) => new Date(Date.UTC(2026, 10, 19, 9, 0, seconds)).toISOString();
const goal = "watch the deploy and fix what breaks";

function setup(t, record) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-slash-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  fs.mkdirSync(path.join(repo, ".rudder", "runs", record.id), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));
  return { home, repo };
}

// Each entry follows the one before it, as Claude Code chains them.
function chain(entries) {
  let parent = null;
  return entries.map((entry, index) => {
    const uuid = `e${index}`;
    const chained = { parentUuid: entry.type === "system" && entry.subtype === "compact_boundary" ? null : parent, isSidechain: false, uuid, ...entry };
    parent = uuid;
    return chained;
  });
}

const user = (seconds, content, extra = {}) => ({ type: "user", timestamp: at(seconds), message: { role: "user", content }, ...extra });
const reply = (seconds, text) => ({ type: "assistant", timestamp: at(seconds), message: { role: "assistant", content: [{ type: "text", text }] } });
const goalStatus = (seconds, status) => ({ type: "attachment", timestamp: at(seconds), attachment: { type: "goal_status", condition: goal, ...status } });
const command = (seconds, name, args) => user(seconds, `<command-name>${name}</command-name>\n            <command-message>${name.slice(1)}</command-message>\n            <command-args>${args}</command-args>`);

const checkIn = `Goal check-in: «${goal}» is still active, and evaluation has been deferred for 30 min because background work is still running:\n- b5590xt6c · monitor · Deploy watcher\nCheck on their progress (e.g. read their output). If they are progressing, say so briefly and keep waiting; if they are stuck or no longer needed, fix or stop them and continue toward the goal.`;
const reason = "The deploy watcher is still running and nothing shows the deploy finished.";

const shown = (messages) => messages.map((m) => ({
  role: m.role,
  ...(m.kind ? { kind: m.kind } : {}),
  text: m.text,
  ...(m.command ? { command: m.command } : {}),
  ...(m.goal?.detail ? { detail: m.goal.detail } : {}),
}));

test("Claude's /goal, its check-ins and /compact show once, in order, as notes, and settle what the chat sent", (t) => {
  const session = "ab1b54a0-0000-4000-8000-000000000001";
  const { home, repo } = setup(t, { id: "run_c", backend: "claude", status: "running", session: { nativeSessionId: session } });
  const project = path.join(home, ".claude", "projects", "-x");
  fs.mkdirSync(project, { recursive: true });
  const entries = chain([
    user(0, "ship the release"),
    reply(1, "Shipping it."),
    // /goal <condition>, as the chat types it into Claude Code.
    goalStatus(10, { met: false, sentinel: true }),
    command(10, "/goal", goal),
    user(10, `<local-command-stdout>Goal set: ${goal}</local-command-stdout>`, { turnOrigin: "human" }),
    user(10, `A session-scoped Stop hook is now active with condition: "${goal}". Briefly acknowledge the goal, then immediately start (or continue) working toward it.`, { isMeta: true }),
    reply(12, "Goal set. Watching the deploy."),
    // A check-in that wakes the idle agent.
    user(40, `<task-notification>\n<summary>Goal check-in: background work still running</summary>\n</task-notification>\n<system-reminder>\n${checkIn}\n</system-reminder>`, {
      origin: { kind: "task-notification" }, turnOrigin: "task_notification", queueOrigin: { kind: "task-notification", source: "goal-checkin" },
    }),
    reply(41, "The watcher is still going."),
    // One at the end of a turn, then the evaluator's verdict.
    user(42, checkIn, { isMeta: true }),
    user(43, `Stop hook feedback:\n[${goal}]: ${reason}`, { isMeta: true }),
    goalStatus(43, { met: false, reason }),
    reply(44, "Still on it."),
    // /compact: Claude Code writes the boundary, the summary and only then the
    // command it ran (stamped when it was typed) and its output.
    { type: "system", subtype: "compact_boundary", content: "Conversation compacted", level: "info", timestamp: at(55), logicalParentUuid: "e12", compactMetadata: { trigger: "manual", preTokens: 212070, postTokens: 24989, durationMs: 4000 } },
    user(55, "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\nThe deploy is being watched.", { isCompactSummary: true, isVisibleInTranscriptOnly: true }),
    user(51, "<local-command-caveat>The command below was run directly in Claude Code, not sent to you as a request.</local-command-caveat>", { isMeta: true }),
    command(51, "/compact", ""),
    user(55, "<local-command-stdout>\u001b[2mCompacted (ctrl+o to see full summary)\u001b[22m\n\u001b[2mPostCompact [command hook 1] completed successfully\u001b[22m</local-command-stdout>"),
    goalStatus(55, { met: false, sentinel: true }),
    // A command that prints nothing shows as typed; /effort shows its output.
    command(60, "/effort", "low"),
    user(60, "<local-command-stdout>Set effort level to low</local-command-stdout>"),
    user(61, "what's left?"),
  ]);
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl(entries));
  const { messages } = readTranscript(repo, "run_c", { home });
  assert.deepEqual(shown(messages), [
    { role: "user", text: "ship the release" },
    { role: "assistant", text: "Shipping it." },
    { role: "system", kind: "command", text: `Goal set: ${goal}`, command: "/goal" },
    { role: "assistant", text: "Goal set. Watching the deploy." },
    { role: "system", kind: "goal", text: "Goal check-in", detail: checkIn },
    { role: "assistant", text: "The watcher is still going." },
    { role: "system", kind: "goal", text: "Goal check-in", detail: checkIn },
    { role: "system", kind: "goal", text: "Goal not met yet", detail: reason },
    { role: "assistant", text: "Still on it." },
    { role: "system", kind: "compact", text: "Context compacted" },
    { role: "system", kind: "command", text: "Set effort level to low", command: "/effort" },
    { role: "user", text: "what's left?" },
  ]);
  assert.equal(messages.find((m) => m.kind === "compact").compact.summary, "The deploy is being watched.");

  // What the chat sent settles on what each command left, never before.
  const before = messages.slice(0, 2);
  const sent = (text, id) => ({ id, text, ts: Date.parse(at(2)), state: "sent", after: sentAfter(before) });
  const goalSent = sent(`/goal ${goal}`, "p1");
  const compactSent = sent("/compact", "p2");
  assert.deepEqual(settlePending([goalSent, compactSent], before, Date.parse(at(3))), [goalSent, compactSent], "nothing ran yet");
  assert.deepEqual(settlePending([goalSent, compactSent], messages.slice(0, 9), Date.parse(at(45))), [compactSent], "/goal ran");
  assert.deepEqual(settlePending([goalSent, compactSent], messages, Date.parse(at(62))), [], "/compact ran");
  // A path is words: it settles on the user message that repeats it.
  const words = sent("/tmp/build.log is empty", "p3");
  assert.deepEqual(settlePending([words], messages, Date.parse(at(62))), [words]);
  assert.deepEqual(settlePending([words], [...messages, { id: "u9", role: "user", ts: Date.parse(at(63)), text: "/tmp/build.log is empty" }], Date.parse(at(64))), []);
});

test("Codex's /goal shows as a note, and each turn it starts for the goal as a marker, not as the user", (t) => {
  const hex = Date.parse(at(0)).toString(16).padStart(12, "0");
  const thread = `${hex.slice(0, 8)}-${hex.slice(8)}-7000-8000-000000000002`;
  const { home, repo } = setup(t, { id: "run_x", backend: "codex", status: "running", session: { nativeSessionId: thread } });
  const day = path.join(home, ".codex", "sessions", "2026", "11", "19");
  fs.mkdirSync(day, { recursive: true });
  const harness = (kind) => ({ turn_id: "t2", content_item_kinds: [kind], content_item_metadata: [{ provenance: { type: "harness", hook: false } }] });
  fs.writeFileSync(path.join(day, `rollout-2026-11-19T09-00-00-${thread}.jsonl`), jsonl([
    { timestamp: at(0), type: "session_meta", payload: { id: thread, cli_version: "0.170.0" } },
    { timestamp: at(1), type: "response_item", payload: { type: "message", role: "user", id: "m1", content: [{ type: "input_text", text: "ship the release" }] } },
    { timestamp: at(2), type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "Shipping it." }] } },
    { timestamp: at(10), type: "response_item", payload: { type: "message", role: "user", id: "m2", content: [{ type: "input_text", text: `<codex_internal_context source="user_goal">\nUser set the goal: "${goal}"\nUser set goal status: "active".\n</codex_internal_context>` }], internal_chat_message_metadata_passthrough: harness("user.goal") } },
    { timestamp: at(10), type: "event_msg", payload: { type: "thread_goal_updated", threadId: thread, goal: { threadId: thread, objective: goal, status: "active" } } },
    { timestamp: at(10), type: "event_msg", payload: { type: "task_started", turn_id: "t2", turn_attribution: { turn_id: "t2", turn_trigger: "goal" } } },
    { timestamp: at(11), type: "response_item", payload: { type: "message", role: "user", id: "m3", content: [{ type: "input_text", text: "<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal.\n\nBudget:\n- Tokens used: 588663\n</codex_internal_context>" }], internal_chat_message_metadata_passthrough: harness("goal.internal_context") } },
    { timestamp: at(12), type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "Watching the deploy." }] } },
  ]));
  const { messages } = readTranscript(repo, "run_x", { home });
  assert.deepEqual(shown(messages), [
    { role: "user", text: "ship the release" },
    { role: "assistant", text: "Shipping it." },
    { role: "system", kind: "command", text: `User set the goal: "${goal}"\n\nUser set goal status: "active".`, command: "/goal" },
    { role: "system", kind: "goal", text: "Continuing toward the goal" },
    { role: "assistant", text: "Watching the deploy." },
  ]);
  const goalSent = { id: "p1", text: `/goal ${goal}`, ts: Date.parse(at(3)), state: "sent", after: sentAfter(messages.slice(0, 2)) };
  assert.deepEqual(settlePending([goalSent], messages, Date.parse(at(13))), []);
});
