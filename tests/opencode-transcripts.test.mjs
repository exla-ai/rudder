// An OpenCode agent's chat in the app (cloud/worker/transcripts.mjs), read
// from OpenCode's own database (cloud/worker/opencode.mjs). The fixture is
// synthetic, shaped like OpenCode 1.19.5's tables.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { listSubagents } from "../cloud/worker/subagents.mjs";
import { lastAssistantText, readTranscript, recentConversation } from "../cloud/worker/transcripts.mjs";

const SESSION = "ses_0a1b2c3d4e5fOpenCodeTest";

function setup(t, seed) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-opencode-"));
  const saved = { data: process.env.XDG_DATA_HOME, cache: process.env.XDG_CACHE_HOME };
  delete process.env.XDG_DATA_HOME;
  delete process.env.XDG_CACHE_HOME;
  t.after(() => {
    fs.rmSync(root, { recursive: true, force: true });
    for (const [key, value] of [["XDG_DATA_HOME", saved.data], ["XDG_CACHE_HOME", saved.cache]]) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const runDir = path.join(repo, ".rudder", "runs", "run_o");
  fs.mkdirSync(runDir, { recursive: true });
  fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify({
    id: "run_o", backend: "opencode", model: "opencode/big-pickle", status: "done", task: "fix the bug",
    repoRoot: path.join(root, "nowhere"), session: { nativeSessionId: SESSION },
    ...(seed ? { process: { pid: process.pid } } : {}),
  }));
  fs.mkdirSync(path.join(home, ".cache", "opencode"), { recursive: true });
  fs.writeFileSync(path.join(home, ".cache", "opencode", "models.json"), JSON.stringify({
    opencode: { models: { "big-pickle": { limit: { context: 200_000 } } } },
  }));
  const dir = path.join(home, ".local", "share", "opencode");
  fs.mkdirSync(dir, { recursive: true });
  const db = new DatabaseSync(path.join(dir, "opencode.db"));
  db.exec(`
    CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, agent TEXT, time_created INTEGER, time_updated INTEGER);
    CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
    CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
  `);
  db.prepare("INSERT INTO session VALUES (?, NULL, 'Fix the bug', 'build', 1000, 9000)").run(SESSION);
  let parts = 0;
  const addMessage = (session, id, created, data, ...pieces) => {
    db.prepare("INSERT INTO message VALUES (?, ?, ?, ?)").run(id, session, created, JSON.stringify({ time: { created }, ...data }));
    for (const piece of pieces) {
      parts += 1;
      db.prepare("INSERT INTO part VALUES (?, ?, ?, ?, ?, ?)").run(`prt_${String(parts).padStart(3, "0")}`, id, session, created, created, JSON.stringify(piece));
    }
  };
  const message = (...args) => addMessage(SESSION, ...args);
  const tokens = (input, output, read) => ({ input, output, reasoning: 0, cache: { read, write: 0 } });
  const reply = (created, completed, finish, t) => ({ role: "assistant", time: { created, completed }, finish, providerID: "opencode", modelID: "big-pickle", tokens: t });
  message("msg_01", 1000, { role: "user" }, { type: "text", text: "fix the bug" }, { type: "text", text: "<system-reminder>", synthetic: true });
  message("msg_02", 1100, reply(1100, 3100, "tool-calls", tokens(100, 50, 2000)),
    { type: "step-start" },
    { type: "reasoning", text: "thinking" },
    { type: "text", text: "Looking at it." },
    { type: "tool", tool: "edit", callID: "call_edit", state: { status: "completed", input: { filePath: "/repo/src/a.ts", oldString: "x", newString: "y" }, output: "Edit applied successfully.", time: { start: 1500, end: 1600 } } },
    { type: "step-finish", reason: "tool-calls" });
  message("msg_03", 3200, reply(3200, 5200, "stop", tokens(50, 60, 2100)),
    { type: "tool", tool: "bash", callID: "call_bash", state: { status: "error", input: { command: "npm test", description: "Run tests" }, error: "exit 1", time: { start: 3300, end: 3400 } } },
    { type: "text", text: "Done: the test passes now." });
  message("msg_04", 6000, { role: "user" }, { type: "compaction", auto: true });
  message("msg_05", 6100, { role: "assistant", summary: true, time: { created: 6100, completed: 6500 }, tokens: tokens(0, 0, 0) }, { type: "text", text: "## Goal\nFix the bug" });
  seed?.(db, addMessage);
  db.close();
  return { home, repo };
}

test("OpenCode: the chat shows the user's words, the agent's text and tool calls, the turn's time and a compaction", (t) => {
  const { home, repo } = setup(t);
  const page = readTranscript(repo, "run_o", { home, limit: 300 });
  assert.equal(page.conversation, SESSION);
  assert.equal(page.backend, "opencode");
  assert.equal(page.earlier, null);
  const shown = page.messages.map((m) => [m.role, m.kind ?? m.tool?.name ?? m.text]);
  assert.deepEqual(shown, [
    ["user", "fix the bug"],
    ["assistant", "Looking at it."],
    ["tool", "edit"],
    ["tool", "bash"],
    ["assistant", "Done: the test passes now."],
    ["system", "turn"],
    ["system", "compact"],
  ], "reasoning, step markers and injected text are not the chat's");
  const [, , edit, bash] = page.messages;
  assert.deepEqual(edit.tool, { name: "edit", input: "/repo/src/a.ts", output: "Edit applied successfully.", isError: false, files: [{ path: "/repo/src/a.ts", line: 0, end: 0 }] });
  assert.deepEqual(bash.tool, { name: "bash", input: "npm test", output: "exit 1", isError: true });
  assert.equal(page.messages[5].durationMs, 4200, "from the prompt to the reply that ended the turn");
  assert.equal(page.messages[6].compact.trigger, "auto");
  assert.equal(page.messages[6].compact.summary, "## Goal\nFix the bug");
  assert.equal(page.stats.model, "opencode/big-pickle");
  assert.equal(page.stats.speed.tokensPerSecond, 27.5);
  assert.deepEqual(page.context, { used: 2210, window: 200_000, autoCompactAt: null, source: null, at: 5200 });
  assert.deepEqual(page.edits, []);

  // An earlier page is the messages before the cursor.
  const newest = readTranscript(repo, "run_o", { home, limit: 2 });
  assert.equal(newest.earlier, `${SESSION}:3:`);
  const earlier = readTranscript(repo, "run_o", { home, limit: 300, before: newest.earlier });
  assert.deepEqual(earlier.messages.map((m) => m.kind ?? (m.text || m.tool?.name)), ["fix the bug", "Looking at it.", "edit", "bash", "Done: the test passes now.", "turn"]);
});

test("OpenCode: the preview is the agent's newest words, and a handoff reads its tool calls", (t) => {
  const { home, repo } = setup(t);
  const record = JSON.parse(fs.readFileSync(path.join(repo, ".rudder", "runs", "run_o", "run.json"), "utf8"));
  const preview = lastAssistantText(record, { home, cache: null, now: 10_000 });
  assert.equal(preview.text, "Done: the test passes now.", "not the compaction's summary");
  const recent = recentConversation(repo, "run_o", { home, limit: 120 });
  assert.equal(recent.backend, "opencode");
  assert.equal(recent.lines.length, 2);
  assert.equal(JSON.parse(recent.lines[0]).state.input.filePath, "/repo/src/a.ts");
});

test("OpenCode: background Tasks show their own status, activity and nested sessions", (t) => {
  const running = "ses_Running";
  const done = "ses_Done";
  const nested = "ses_Nested";
  const unrelated = "ses_Unrelated";
  const task = (id, description) => ({
    type: "tool", tool: "task", callID: `call_${id}`,
    state: { status: "completed", input: { description, subagent_type: "explore" }, metadata: { sessionId: id, background: true } },
  });
  const { home, repo } = setup(t, (db, addMessage) => {
    const session = db.prepare("INSERT INTO session VALUES (?, ?, ?, 'explore', 7000, 9000)");
    session.run(running, SESSION, "Find the caller (@explore subagent)");
    session.run(done, SESSION, "Review the fix (@explore subagent)");
    session.run(nested, running, "Read the call site (@explore subagent)");
    session.run(unrelated, null, "Unrelated run");
    addMessage(SESSION, "msg_tasks", 7000, { role: "assistant" }, task(running, "Find the caller"), task(done, "Review the fix"), task(unrelated, "Wrong session"));
    addMessage(running, "msg_find", 7000, { role: "user" }, { type: "text", text: "Where does it get called?" });
    addMessage(running, "msg_search", 7100, { role: "assistant", time: { created: 7100, completed: 7200 }, finish: "stop" }, task(nested, "Read the call site"), {
      type: "tool", tool: "read",
      state: { status: "completed", input: { filePath: "/repo/src/caller.ts" }, metadata: { display: { type: "file" } }, output: "x".repeat(3000) },
    });
    addMessage(done, "msg_review", 7000, { role: "user" }, { type: "text", text: "Check the fix" });
    addMessage(done, "msg_verified", 7100, { role: "assistant", time: { created: 7100, completed: 7500 }, finish: "stop" }, { type: "text", text: "Looks good." });
    addMessage(done, "msg_compact_request", 7600, { role: "user" }, { type: "compaction", auto: true });
    addMessage(done, "msg_compact_result", 7700, { role: "assistant", summary: true }, { type: "text", text: "Compaction summary" });
    addMessage(nested, "msg_scan", 7300, { role: "assistant", time: { created: 7300, completed: 8000 }, error: { name: "APIError", data: { message: "Provider unavailable" } } });
  });

  const tray = listSubagents(repo, "run_o", { home, open: running });
  assert.deepEqual(tray.subagents.map(({ id, kind, description, status, endedAt, toolCalls, step }) => ({ id, kind, description, status, endedAt, toolCalls, step })), [
    { id: running, kind: "explore", description: "Find the caller", status: "running", endedAt: null, toolCalls: 2, step: "read /repo/src/caller.ts" },
    { id: done, kind: "explore", description: "Review the fix", status: "done", endedAt: 7500, toolCalls: 0, step: "Looks good." },
  ]);
  assert.equal(tray.open.prompt, "Where does it get called?");
  const nextTask = tray.open.messages.find((message) => message.tool?.name === "task");
  assert.equal(nextTask.subagent.id, nested);
  assert.equal(nextTask.subagent.status, "failed");
  assert.equal(nextTask.subagent.result, "Provider unavailable");
  const read = tray.open.messages.find((message) => message.tool?.name === "read");
  assert.deepEqual(read.tool.files, [{ path: "/repo/src/caller.ts", line: 0, end: 0 }]);
  assert.match(read.tool.output, /1000 more characters/);
  for (const id of [nested, unrelated]) {
    assert.equal(listSubagents(repo, "run_o", { home, open: id }).open, null);
  }

  const chat = readTranscript(repo, "run_o", { home, limit: 300 });
  const cards = Object.fromEntries(chat.messages.filter((message) => message.tool?.name === "task").map((message) => [message.id, message.subagent]));
  assert.deepEqual(cards[`call_${running}`], tray.open);
  assert.equal(cards[`call_${done}`].result, "Looks good.");
  assert.equal(cards[`call_${unrelated}`], undefined);
});
