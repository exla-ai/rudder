// Session lineage: the Claude branch a chat is on, a Codex fork's history
// through history_base, the messages the chat can edit, and reading a chat
// back page by page. Fixtures are synthetic.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { activeClaudeLines, codexLineage, editablePrompts, readSessionLines } from "../cloud/worker/session-lineage.mjs";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const uuids = (lines) => lines.filter(Boolean).map((line) => JSON.parse(line).uuid).filter(Boolean);

test("Claude shows only the branch it is on, with parallel tool results and history before a compaction", () => {
  const user = (uuid, parentUuid, content) => ({ type: "user", uuid, parentUuid, message: { role: "user", content } });
  const assistant = (uuid, parentUuid, content) => ({ type: "assistant", uuid, parentUuid, message: { content } });
  const lines = jsonl([
    user("u1", null, "first ask"),
    assistant("a1", "u1", [{ type: "tool_use", id: "t1" }]),
    // Parallel calls: the first result hangs off the first call.
    assistant("a2", "a1", [{ type: "tool_use", id: "t2" }]),
    user("r1", "a1", [{ type: "tool_result", tool_use_id: "t1" }]),
    user("r2", "a2", [{ type: "tool_result", tool_use_id: "t2" }]),
    assistant("a3", "r2", [{ type: "text", text: "done" }]),
    { type: "system", subtype: "compact_boundary", uuid: "c1", parentUuid: null, logicalParentUuid: "gone" },
    user("u2", "c1", "second ask"),
    assistant("a4", "u2", [{ type: "text", text: "a refusal" }]),
    // Went back to before u2 and asked again.
    user("u3", "c1", "second ask, reworded"),
    assistant("a5", "u3", [{ type: "text", text: "sure" }]),
  ]).split("\n");
  const { keep } = activeClaudeLines(lines);
  const active = lines.filter((_, index) => keep[index]);
  assert.deepEqual(uuids(active), ["u1", "a1", "a2", "r1", "r2", "a3", "c1", "u3", "a5"]);
  assert.deepEqual(editablePrompts(active, "claude"), [
    { messageId: "u1", entry: "u1", text: "first ask" },
    { messageId: "u3", entry: "u3", text: "second ask, reworded" },
  ]);

  // Every turn-starting prompt, oldest first. A text-block prompt is the
  // transcript's `${uuid}:${index}` message; one sent mid-turn (after a tool
  // result) is not editable, and neither is Claude's note that a turn was
  // interrupted.
  const blocks = jsonl([
    user("u1", null, "first"),
    assistant("a1", "u1", [{ type: "text", text: "ok" }]),
    user("u2", "a1", [{ type: "image" }, { type: "text", text: "see this" }]),
    assistant("a2", "u2", [{ type: "tool_use", id: "t1" }]),
    user("r1", "a2", [{ type: "tool_result", tool_use_id: "t1" }]),
    user("q1", "r1", "also this"),
    assistant("a3", "q1", [{ type: "text", text: "done" }]),
    user("u3", "a3", "third"),
    assistant("a4", "u3", [{ type: "text", text: "partly" }]),
    user("i1", "a4", [{ type: "text", text: "[Request interrupted by user]" }]),
  ]).split("\n");
  assert.deepEqual(editablePrompts(blocks, "claude"), [
    { messageId: "u1", entry: "u1", text: "first" },
    { messageId: "u2:1", entry: "u2", text: "see this" },
    { messageId: "u3", entry: "u3", text: "third" },
  ]);
  assert.deepEqual(editablePrompts(blocks.slice(0, 2), "claude"), [{ messageId: "u1", entry: "u1", text: "first" }]);

  // A prompt that sets off an auto-compaction is written before the boundary
  // and again, with the same uuid, after the summary (Claude Code 2.1.3xx).
  // The chat shows it once, so it is one prompt to edit.
  const compacted = jsonl([
    user("u1", null, "first ask"),
    assistant("a1", "u1", [{ type: "text", text: "ok" }]),
    { type: "system", subtype: "turn_duration", uuid: "d1", parentUuid: "a1" },
    { type: "file-history-snapshot", messageId: "u2" },
    user("u2", "d1", "second ask"),
    { type: "system", subtype: "compact_boundary", uuid: "c1", parentUuid: null, logicalParentUuid: "u2" },
    { type: "attachment", uuid: "x1", parentUuid: "c1", attachment: { type: "file" } },
    { type: "user", uuid: "s1", parentUuid: "x1", isCompactSummary: true, message: { role: "user", content: "This session is being continued from a previous conversation." } },
    user("u2", "s1", "second ask"),
    assistant("a2", "u2", [{ type: "text", text: "on it" }]),
    user("u3", "a2", "third ask"),
  ]).split("\n");
  const kept = activeClaudeLines(compacted).keep;
  assert.deepEqual(editablePrompts(compacted.filter((_, index) => kept[index]), "claude"), [
    { messageId: "u1", entry: "u1", text: "first ask" },
    { messageId: "u2", entry: "u2", text: "second ask" },
    { messageId: "u3", entry: "u3", text: "third ask" },
  ]);
});

test("a Codex fork reads its base rollout up to the fork point, then its own lines", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-lineage-"));
  try {
    const day = path.join(home, ".codex", "sessions", "2026", "01", "02");
    fs.mkdirSync(day, { recursive: true });
    const turn = (id) => ({ type: "event_msg", payload: { type: "task_started", turn_id: id } });
    const ask = (id, text) => ({ type: "response_item", payload: { type: "message", role: "user", id, content: [{ type: "input_text", text }] } });
    const said = (text) => ({ type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text }] } });
    const before = jsonl([
      { type: "session_meta", payload: { id: "thread-a" } },
      turn("turn-1"),
      ask("m1", "<environment_context>cwd</environment_context>"),
      ask("m2", "first ask"),
      said("one"),
      turn("turn-2"),
      ask("m3", "second ask"),
      said("two"),
    ]);
    const after = jsonl([turn("turn-3"), ask("m4", "third ask, later dropped"), said("three")]);
    fs.writeFileSync(path.join(day, "rollout-2026-01-02T00-00-00-thread-a.jsonl"), before + after);
    const forked = path.join(day, "rollout-2026-01-02T00-10-00-thread-b.jsonl");
    fs.writeFileSync(forked, jsonl([
      { type: "session_meta", payload: { id: "thread-b", history_base: { thread_id: "thread-a", end_byte_offset: Buffer.byteLength(before) } } },
      turn("turn-4"),
      ask("m5", "third ask, edited"),
    ]));

    const segments = codexLineage(forked, home);
    const total = segments.reduce((sum, segment) => sum + segment.end, 0);
    const { lines } = readSessionLines(segments, 0, total);
    const asks = lines.filter(Boolean).map((line) => JSON.parse(line).payload?.content?.[0]?.text).filter(Boolean);
    assert.deepEqual(asks, ["<environment_context>cwd</environment_context>", "first ask", "one", "second ask", "two", "third ask, edited"]);
    // Every turn-starting prompt, across the fork.
    assert.deepEqual(editablePrompts(lines, "codex"), [
      { messageId: "m2", entry: "m2", text: "first ask" },
      { messageId: "m3", entry: "m3", text: "second ask" },
      { messageId: "m5", entry: "m5", text: "third ask, edited" },
    ]);

    // A read starting mid-line across the fork drops the cut line.
    const tail = readSessionLines(segments, total - 400, total);
    assert.equal(tail.lines[tail.lines.length - 1], JSON.stringify(ask("m5", "third ask, edited")));
    assert.doesNotThrow(() => tail.lines.forEach((line) => JSON.parse(line)));

    // The only prompt is editable too (it starts the conversation over).
    // Without the latest prompt's id the edit request can't name it, so
    // nothing is editable.
    assert.deepEqual(editablePrompts(jsonl([turn("t"), ask("m", "only ask")]).split("\n"), "codex"), [{ messageId: "m", entry: "m", text: "only ask" }]);
    const noId = { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "no id" }] } };
    assert.deepEqual(editablePrompts(jsonl([turn("t1"), ask("m1", "one"), turn("t2"), noId]).split("\n"), "codex"), []);

    fs.rmSync(path.join(day, "rollout-2026-01-02T00-00-00-thread-a.jsonl"));
    assert.throws(() => codexLineage(forked, home), /thread-a, which is missing/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test("a chat reads back page by page to the session's start, past the tail read, without splitting a tool call from its result", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-pages-"));
  try {
    // Claude: 60 replies, each with a tool call, one of them 7 MiB (more
    // than a page's 6 MiB read, which then starts between it and its
    // result), pairs of parallel calls, and a prompt the user went back from.
    const cwd = "/workspace/code/pages";
    const sessionId = "44444444-2222-3333-4444-555555555555";
    const projectDir = path.join(home, ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"));
    fs.mkdirSync(projectDir, { recursive: true });
    const entries = [];
    let parent = null;
    const add = (type, uuid, content, from = parent) => {
      entries.push({ type, uuid, parentUuid: from, message: { role: type, content } });
      parent = uuid;
    };
    add("user", "u0", "first ask");
    for (let i = 0; i < 60; i += 1) {
      add("assistant", `a${i}`, [{ type: "text", text: `reply ${i}` }]);
      add("assistant", `c${i}`, [{ type: "tool_use", id: `t${i}`, name: "Bash", input: { command: i === 30 ? `step ${i} ${"x".repeat(7 << 20)}` : `step ${i}` } }]);
      if (i % 7 === 3) {
        // Parallel calls: the first one's result comes after the second call.
        add("assistant", `p${i}`, [{ type: "tool_use", id: `q${i}`, name: "Bash", input: { command: `side ${i}` } }]);
        add("user", `r${i}`, [{ type: "tool_result", tool_use_id: `t${i}`, content: `out ${i}` }], `c${i}`);
        add("user", `s${i}`, [{ type: "tool_result", tool_use_id: `q${i}`, content: `side out ${i}` }], `p${i}`);
      } else {
        add("user", `r${i}`, [{ type: "tool_result", tool_use_id: `t${i}`, content: `out ${i}` }]);
      }
      if (i === 5) {
        const back = parent;
        add("user", "gone", "an ask the user went back from");
        add("assistant", "gone-reply", [{ type: "text", text: "a reply gone back from" }]);
        add("user", "u1", "the ask, reworded", back);
      }
    }
    fs.writeFileSync(path.join(projectDir, `${sessionId}.jsonl`), jsonl(entries));
    const record = { id: "run-p", backend: "claude", task: "first ask", session: { nativeSessionId: sessionId }, workspace: { path: cwd } };
    const repo = path.join(home, "repo");
    fs.mkdirSync(path.join(repo, ".rudder", "runs", record.id), { recursive: true });
    fs.writeFileSync(path.join(repo, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));

    const pageBack = (run, limit) => {
      const newest = readTranscript(repo, run, { limit, home });
      const pages = [newest];
      while (pages[0].earlier) {
        pages.unshift(readTranscript(repo, run, { limit, home, before: pages[0].earlier }));
        assert.ok(pages.length < 50, "pages reach the session's start");
      }
      return { newest, pages, messages: pages.flatMap((page) => page.messages) };
    };
    // Page sizes that put page starts on different lines, parallel calls' included.
    for (const limit of [17, 18, 19, 20, 21, 22, 23]) {
      const { newest, pages, messages } = pageBack(record.id, limit);
      assert.ok(pages.length > 4);
      assert.ok(newest.messages.length >= limit && newest.stats && newest.edits, "the newest page is the full one");
      assert.deepEqual(Object.keys(pages[0]).sort(), ["conversation", "earlier", "edits", "messages", "runId"], "an earlier page carries only its messages and edits");
      // The first ask is editable from the page that shows it, which the
      // newest page's read does not reach. All pages' edits, oldest first,
      // end with the latest prompt (chat-edit.js). The reworded ask hangs
      // off a tool result: sent mid-turn, not editable.
      const edits = pages.flatMap((page) => page.edits);
      assert.deepEqual(newest.edits, []);
      assert.ok(pages[0].edits.some((edit) => edit.messageId === "u0"), `${limit} a page`);
      assert.equal(edits.at(-1).messageId, "u0");
      const said = messages.filter((m) => m.role !== "tool").map((m) => m.text);
      assert.deepEqual(said, ["first ask", ...Array.from({ length: 60 }, (_, i) => [`reply ${i}`, ...(i === 5 ? ["the ask, reworded"] : [])]).flat()]);
      const tools = messages.filter((m) => m.role === "tool");
      assert.equal(tools.length, 60 + 9);
      assert.deepEqual(tools.filter((m) => m.tool.output === null).map((m) => m.tool.input.slice(0, 20)), [], `every call has its result (${limit} a page)`);
      assert.equal(new Set(messages.map((m) => m.id)).size, messages.length);
    }
    const newest = readTranscript(repo, record.id, { limit: 20, home });
    // From a cursor on: the same messages as the page that gave it.
    const after = readTranscript(repo, record.id, { limit: 20, home, after: newest.earlier });
    assert.deepEqual(after.messages.map((m) => m.id), newest.messages.map((m) => m.id));
    // A cursor of another conversation (an edit starts a new one).
    const stale = newest.earlier.replace(sessionId, "55555555-2222-3333-4444-555555555555");
    assert.throws(() => readTranscript(repo, record.id, { limit: 20, home, before: stale }), /new conversation/);
    assert.deepEqual(readTranscript(repo, record.id, { limit: 20, home, after: stale }).messages.map((m) => m.id), newest.messages.map((m) => m.id));

    // A page read from where the conversation continues: what follows that
    // entry in these lines is a branch the user went back from; a leaf before
    // the lines puts them all on one.
    const lines = jsonl([
      { type: "user", uuid: "v1", parentUuid: "v0", message: { role: "user", content: "ask" } },
      { type: "assistant", uuid: "v2", parentUuid: "v1", message: { content: [{ type: "text", text: "reply" }] } },
      { type: "user", uuid: "w1", parentUuid: "v2", message: { role: "user", content: "gone" } },
      { type: "assistant", uuid: "w2", parentUuid: "w1", message: { content: [{ type: "text", text: "gone reply" }] } },
    ]).split("\n");
    const branch = activeClaudeLines(lines, "v2");
    assert.deepEqual(branch.keep.slice(0, 4), [true, true, false, false]);
    assert.equal(branch.leafBefore(2), "v2");
    assert.equal(branch.leafBefore(0), "v0", "before these lines it goes on at v1's parent");
    assert.deepEqual(activeClaudeLines(lines, "v0").keep.slice(0, 4), [false, false, false, false]);
    assert.deepEqual(activeClaudeLines(lines, "v0", true).keep.slice(0, 4), [true, true, true, true], "at the file's start v0 is in no line");

    // Codex: a fork's pages run back through its base rollout.
    const day = path.join(home, ".codex", "sessions", "2026", "01", "02");
    fs.mkdirSync(day, { recursive: true });
    const said2 = (text) => ({ type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text }] } });
    const baseLines = jsonl([{ type: "session_meta", payload: { id: "thread-c" } }, ...Array.from({ length: 30 }, (_, i) => said2(`base ${i}`))]);
    fs.writeFileSync(path.join(day, "rollout-2026-01-02T00-00-00-thread-c.jsonl"), baseLines + jsonl([said2("dropped at the fork")]));
    fs.writeFileSync(path.join(day, "rollout-2026-01-02T00-10-00-thread-d.jsonl"), jsonl([
      { type: "session_meta", payload: { id: "thread-d", history_base: { thread_id: "thread-c", end_byte_offset: Buffer.byteLength(baseLines) } } },
      ...Array.from({ length: 30 }, (_, i) => said2(`fork ${i}`)),
    ]));
    const codexRecord = { id: "run-q", backend: "codex", task: "x", session: { nativeSessionId: "thread-d" } };
    fs.mkdirSync(path.join(repo, ".rudder", "runs", codexRecord.id), { recursive: true });
    fs.writeFileSync(path.join(repo, ".rudder", "runs", codexRecord.id, "run.json"), JSON.stringify(codexRecord));
    const codex = pageBack(codexRecord.id, 20);
    assert.equal(codex.pages.length, 3);
    assert.deepEqual(codex.messages.map((m) => m.text), [...Array.from({ length: 30 }, (_, i) => `base ${i}`), ...Array.from({ length: 30 }, (_, i) => `fork ${i}`)]);
    assert.equal(new Set(codex.messages.map((m) => m.id)).size, 60);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
