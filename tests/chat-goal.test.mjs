// A chat's standing goal (cloud/worker/chat-goal.mjs): Claude Code's goal_status
// entries in the session JSONL, and Codex's thread_goals table. The Claude
// lines are real ones from a home's session (2.1.327), trimmed to the fields
// that matter; the met, failed and cleared shapes are the ones Claude Code
// 2.1.328 writes, which that session never reached.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { activeGoal, newestGoal } from "../cloud/worker/chat-goal.mjs";

const FIRST = "lock in and deploy this";
const SECOND = "monitor this every five minutes make a list and amke sure this all works. if it doesn't launch a subagent fixing it and then fix it";

// /goal <condition>: the attachment, then the command and its output.
const SET = [
  `{"parentUuid":"2ed09ced-9a98-49ad-9dc1-601d21188dfb","isSidechain":false,"type":"attachment","uuid":"a3588778-6007-4157-a598-1b016b4739e0","timestamp":"2026-11-19T05:25:04.956Z","attachment":{"type":"goal_status","met":false,"sentinel":true,"condition":"${SECOND}"}}`,
  `{"parentUuid":"a3588778-6007-4157-a598-1b016b4739e0","isSidechain":false,"type":"user","message":{"role":"user","content":"<command-name>/goal</command-name>\\n            <command-message>goal</command-message>\\n            <command-args>${SECOND}</command-args>"},"uuid":"c6383376-4278-4f01-80d8-764c378989fb","timestamp":"2026-11-19T05:25:04.956Z"}`,
  `{"parentUuid":"c6383376-4278-4f01-80d8-764c378989fb","isSidechain":false,"type":"user","message":{"role":"user","content":"<local-command-stdout>Goal set: ${SECOND}</local-command-stdout>"},"uuid":"5f0f0b7e-0a51-4c1e-9d55-2b3c3a1f0e11","timestamp":"2026-11-19T05:25:04.956Z"}`,
];
// After a compaction Claude Code writes the goal again (its keys in another order).
const RESTATED = `{"parentUuid":"8019cdad-bd1b-4f0d-a12c-71c128ef9084","isSidechain":false,"attachment":{"type":"goal_status","met":false,"sentinel":true,"condition":"${FIRST}"},"type":"attachment","uuid":"17612c2a-dae6-4bcb-b74e-70d6822d3544","timestamp":"2026-11-18T23:06:00.193Z"}`;
// A check-in is a plain prompt; it changes nothing.
const CHECK_IN = `{"parentUuid":"0f13755e-af9c-4afc-9ed0-19905e1a4acf","isSidechain":false,"type":"user","message":{"role":"user","content":"Goal check-in: «${FIRST}» is still active, and evaluation has been deferred for 62 min because background work is still running"},"uuid":"9d1f1c50-61f7-4f5b-8f0e-6f8e0c0b5c2a","timestamp":"2026-11-19T03:48:00.000Z"}`;

const status = (attachment, extra = {}) =>
  JSON.stringify({ isSidechain: false, type: "attachment", uuid: "u", timestamp: "2026-11-19T06:00:00.000Z", attachment: { type: "goal_status", ...attachment }, ...extra });
const NOT_MET = status({ met: false, condition: SECOND, reason: "The list is not checked yet." });
const MET = status({ met: true, condition: SECOND, reason: "Everything on the list works.", iterations: 4, durationMs: 3_600_000, tokens: 210_000 });
const FAILED = status({ met: false, failed: true, condition: SECOND, reason: "The machine is gone.", iterations: 2, durationMs: 60_000, tokens: 9_000 });
const CLEARED = status({ met: true, sentinel: true, condition: SECOND });

const goalOf = (lines, before = null) => newestGoal(Buffer.from(lines.map((line) => `${line}\n`).join("")), before);

test("a goal is active from /goal until it is met, fails or is cleared", () => {
  assert.equal(goalOf([CHECK_IN]), null);
  assert.deepEqual(goalOf(SET), { text: SECOND });
  assert.deepEqual(goalOf([...SET, CHECK_IN, NOT_MET]), { text: SECOND });
  assert.equal(goalOf([...SET, NOT_MET, MET]), null);
  assert.equal(goalOf([...SET, FAILED]), null);
  assert.equal(goalOf([...SET, CLEARED]), null);
  // The goal before these lines holds when none of them changes it.
  assert.deepEqual(goalOf([CHECK_IN], { text: FIRST }), { text: FIRST });
});

test("the newest goal_status decides: a new /goal replaces the old one, a compaction restates it", () => {
  assert.deepEqual(goalOf([RESTATED, CHECK_IN]), { text: FIRST });
  assert.deepEqual(goalOf([RESTATED, ...SET]), { text: SECOND });
  assert.deepEqual(goalOf([...SET, CLEARED, RESTATED]), { text: FIRST });
});

test("a subagent's goal_status and the marker inside another entry are not the chat's goal", () => {
  const sidechain = status({ met: false, sentinel: true, condition: "a subagent's goal" }, { isSidechain: true });
  const nested = JSON.stringify({ type: "progress", data: { message: { type: "attachment", attachment: { type: "goal_status", met: false, condition: "nested" } } } });
  assert.equal(goalOf([sidechain, nested]), null);
  assert.equal(goalOf([...SET, sidechain, nested, MET]), null);
});

test("a goal_status without a condition is an error, not a goal", () => {
  assert.throws(() => goalOf([status({ met: false, sentinel: true })]), /no longer has a condition/);
});

test("Claude: the session file is read once, then only what it gains", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "goal-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const file = path.join(dir, "session.jsonl");
  const record = { backend: "claude" };
  fs.writeFileSync(file, `${CHECK_IN}\n`);
  assert.equal(activeGoal(record, file, dir), null);
  fs.appendFileSync(file, SET.map((line) => `${line}\n`).join(""));
  assert.deepEqual(activeGoal(record, file, dir), { text: SECOND });
  // A line still being written counts once it is whole.
  const half = Math.floor(MET.length / 2);
  fs.appendFileSync(file, MET.slice(0, half));
  assert.deepEqual(activeGoal(record, file, dir), { text: SECOND });
  fs.appendFileSync(file, `${MET.slice(half)}\n`);
  assert.equal(activeGoal(record, file, dir), null);
  fs.appendFileSync(file, `${RESTATED}\n`);
  assert.deepEqual(activeGoal(record, file, dir), { text: FIRST });
  // A shorter file is a new session: read from its start.
  fs.writeFileSync(file, `${CHECK_IN}\n`);
  assert.equal(activeGoal(record, file, dir), null);
});

test("Codex: the thread's row in goals_1.sqlite, active only while its status is active", (t) => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "goal-"));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const record = (threadId) => ({ backend: "codex", session: { nativeSessionId: threadId } });
  // No goal was ever set on the machine.
  assert.equal(activeGoal(record("thread-a"), null, home), null);
  fs.mkdirSync(path.join(home, ".codex"));
  const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite");
  const db = new DatabaseSync(path.join(home, ".codex", "goals_1.sqlite"));
  // Codex 0.170's table.
  db.exec(`CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL, goal_id TEXT NOT NULL, objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('active', 'paused', 'blocked', 'usage_limited', 'budget_limited', 'complete')),
    token_budget INTEGER, tokens_used INTEGER NOT NULL DEFAULT 0, time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL)`);
  const insert = db.prepare("INSERT INTO thread_goals (thread_id, goal_id, objective, status, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, 0, 0)");
  insert.run("thread-a", "g1", "make texting work", "active");
  insert.run("thread-b", "g2", "ship the API", "blocked");
  insert.run("thread-c", "g3", "implement the whole thing", "complete");
  db.close();
  assert.deepEqual(activeGoal(record("thread-a"), null, home), { text: "make texting work" });
  assert.equal(activeGoal(record("thread-b"), null, home), null);
  assert.equal(activeGoal(record("thread-c"), null, home), null);
  assert.equal(activeGoal(record("thread-d"), null, home), null);
});
