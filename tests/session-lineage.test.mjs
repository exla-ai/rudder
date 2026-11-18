// Session lineage: the Claude branch a chat is on, a Codex fork's history
// through history_base, and the messages the chat can edit. Fixtures are
// synthetic.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { activeClaudeLines, editablePrompts, readCodexLineage } from "../cloud/worker/session-lineage.mjs";

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
  const active = activeClaudeLines(lines);
  assert.deepEqual(uuids(active), ["u1", "a1", "a2", "r1", "r2", "a3", "c1", "u3", "a5"]);
  assert.deepEqual(editablePrompts(active, "claude"), [{ messageId: "u3", entry: "u3", text: "second ask, reworded" }]);

  // Every turn-starting prompt but the first, oldest first. A text-block
  // prompt is the transcript's `${uuid}:${index}` message; one sent mid-turn
  // (after a tool result) is not editable.
  const blocks = jsonl([
    user("u1", null, "first"),
    assistant("a1", "u1", [{ type: "text", text: "ok" }]),
    user("u2", "a1", [{ type: "image" }, { type: "text", text: "see this" }]),
    assistant("a2", "u2", [{ type: "tool_use", id: "t1" }]),
    user("r1", "a2", [{ type: "tool_result", tool_use_id: "t1" }]),
    user("q1", "r1", "also this"),
    assistant("a3", "q1", [{ type: "text", text: "done" }]),
    user("u3", "a3", "third"),
  ]).split("\n");
  assert.deepEqual(editablePrompts(blocks, "claude"), [
    { messageId: "u2:1", entry: "u2", text: "see this" },
    { messageId: "u3", entry: "u3", text: "third" },
  ]);
  assert.deepEqual(editablePrompts(blocks.slice(0, 2), "claude"), []);
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

    const { text, truncated } = readCodexLineage(forked, home, 1 << 20);
    assert.equal(truncated, false);
    const asks = text.split("\n").filter(Boolean).map((line) => JSON.parse(line).payload?.content?.[0]?.text).filter(Boolean);
    assert.deepEqual(asks, ["<environment_context>cwd</environment_context>", "first ask", "one", "second ask", "two", "third ask, edited"]);
    // Every turn-starting prompt after the first, across the fork.
    assert.deepEqual(editablePrompts(text.split("\n"), "codex"), [
      { messageId: "m3", entry: "m3", text: "second ask" },
      { messageId: "m5", entry: "m5", text: "third ask, edited" },
    ]);

    // A tail read across the fork drops its cut first line and says so.
    const tail = readCodexLineage(forked, home, 400);
    assert.equal(tail.truncated, true);
    assert.ok(tail.text.endsWith(`${JSON.stringify(ask("m5", "third ask, edited"))}\n`));
    assert.doesNotThrow(() => tail.text.split("\n").filter(Boolean).forEach((line) => JSON.parse(line)));

    // The first turn has no earlier turn to fork through; without the latest
    // prompt's id the edit request can't name it, so nothing is editable.
    assert.deepEqual(editablePrompts(jsonl([turn("t"), ask("m", "only ask")]).split("\n"), "codex"), []);
    const noId = { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "no id" }] } };
    assert.deepEqual(editablePrompts(jsonl([turn("t1"), ask("m1", "one"), turn("t2"), noId]).split("\n"), "codex"), []);

    fs.rmSync(path.join(day, "rollout-2026-01-02T00-00-00-thread-a.jsonl"));
    assert.throws(() => readCodexLineage(forked, home, 1 << 20), /thread-a, which is missing/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});
