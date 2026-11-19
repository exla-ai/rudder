// Files sent from the app (cloud/app/pending.js withAttachments) come back in
// the chat as a user message's `attachments`, its text only the user's words
// (cloud/worker/transcripts.mjs). Claude Code records a multi-line message as
// a terminal paste, wrapped in <pasted_content id="N"> tags whose closing tag
// carries the id too. Fixtures are shaped like Claude Code 2.1.328 and Codex
// 0.169 logs.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const at = (seconds) => new Date(Date.UTC(2026, 10, 19, 4, 42, seconds)).toISOString();
const shot = "/workspace/code/offstage/.rudder/uploads/20261119-044218-image.png";
const log = "/workspace/code/offstage/.rudder/uploads/20261119-044219-build.log";

function setup(t, record) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-attachments-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  fs.mkdirSync(path.join(repo, ".rudder", "runs", record.id), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));
  return { home, repo };
}

const shown = (transcript) => transcript.messages.filter((m) => m.role === "user").map(({ text, attachments }) => ({ text, attachments }));

test("A message's files show as attachments, apart from the words, whether pasted, queued or from Codex", (t) => {
  const session = "a8caccf0-0000-4000-8000-000000000001";
  const claude = setup(t, { id: "run_c", backend: "claude", status: "running", session: { nativeSessionId: session } });
  const project = path.join(claude.home, ".claude", "projects", "-x");
  fs.mkdirSync(project, { recursive: true });
  const user = (uuid, parentUuid, content, ts) => ({ type: "user", uuid, parentUuid, timestamp: at(ts), message: { role: "user", content } });
  const reply = (uuid, parentUuid, ts) => ({ type: "assistant", uuid, parentUuid, timestamp: at(ts), message: { content: [{ type: "text", text: "Looking." }] } });
  fs.writeFileSync(path.join(project, `${session}.jsonl`), jsonl([
    user("u1", null, "fix the menu", 0),
    reply("a1", "u1", 1),
    user("u2", "a1", `\n\n<pasted_content id="2500">\nfigure out how to use chrome\n\nAttached files:\n- ${shot}\n- ${log}\n</pasted_content id="2500">\n`, 2),
    reply("a2", "u2", 3),
    // Sent while the agent works: files only, handed to the running turn.
    { type: "attachment", uuid: "q1", parentUuid: "a2", timestamp: at(4), attachment: { type: "queued_command", prompt: `Attached files:\n- ${shot}`, commandMode: "prompt", origin: { kind: "human" } } },
    // A list the user pasted themselves, of files saved elsewhere, is their words.
    user("u3", "q1", '<pasted_content id="2501">\nsee the mail:\n\nAttached files:\n- report.pdf\n</pasted_content id="2501">', 5),
  ]));
  assert.deepEqual(shown(readTranscript(claude.repo, "run_c", { home: claude.home })), [
    { text: "fix the menu", attachments: undefined },
    { text: "figure out how to use chrome", attachments: [shot, log] },
    { text: "", attachments: [shot] },
    { text: "see the mail:\n\nAttached files:\n- report.pdf", attachments: undefined },
  ]);

  const hex = Date.parse(at(0)).toString(16).padStart(12, "0");
  const thread = `${hex.slice(0, 8)}-${hex.slice(8)}-7000-8000-000000000002`;
  const codex = setup(t, { id: "run_x", backend: "codex", status: "running", session: { nativeSessionId: thread } });
  const day = path.join(codex.home, ".codex", "sessions", "2026", "11", "19");
  fs.mkdirSync(day, { recursive: true });
  fs.writeFileSync(path.join(day, `rollout-2026-11-19T04-42-00-${thread}.jsonl`), jsonl([
    { timestamp: at(0), ordinal: 0, type: "session_meta", payload: { id: thread, cli_version: "0.169.0" } },
    { timestamp: at(1), ordinal: 1, type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: `what is this\n\nAttached files:\n- ${shot}` }] } },
  ]));
  assert.deepEqual(shown(readTranscript(codex.repo, "run_x", { home: codex.home })), [{ text: "what is this", attachments: [shot] }]);
});
