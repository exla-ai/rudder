// Search everything's messages (cloud/worker/transcript-search.mjs): what the
// user and the agents said in Claude, Codex and Antigravity chats, never tool
// calls, their output or a harness's own bookkeeping, with the ids the chat
// gives those messages. Fixtures are synthetic.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { searchChats } from "../cloud/worker/transcript-search.mjs";
import { readTranscript } from "../cloud/worker/transcripts.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");
const at = (minute) => `2026-11-20T10:${String(minute).padStart(2, "0")}:00Z`;

function fixtures() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-search-"));
  const home = path.join(root, "home");
  const repo = path.join(root, "repo");
  const run = (record) => {
    fs.mkdirSync(path.join(repo, ".rudder", "runs", record.id), { recursive: true });
    fs.writeFileSync(path.join(repo, ".rudder", "runs", record.id, "run.json"), JSON.stringify(record));
  };

  const cwd = "/workspace/code/shop";
  const claudeId = "aaaaaaaa-1111-2222-3333-444444444444";
  const claudeDir = path.join(home, ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"));
  fs.mkdirSync(claudeDir, { recursive: true });
  const user = (uuid, minute, content, extra = {}) => ({ parentUuid: null, isSidechain: false, type: "user", uuid, timestamp: at(minute), message: { role: "user", content }, ...extra });
  const say = (uuid, minute, content, extra = {}) => ({ parentUuid: null, isSidechain: false, type: "assistant", uuid, timestamp: at(minute), message: { role: "assistant", content }, ...extra });
  fs.writeFileSync(path.join(claudeDir, `${claudeId}.jsonl`), jsonl([
    user("u1", 1, "Objective: ship it\nDone when: live\n\nRudder-specific context injected by Rudder:\n- deploy with care\n\nship the checkout"),
    say("a1", 2, [{ type: "text", text: "I will Deploy the checkout after the tests." }]),
    say("a2", 3, [{ type: "tool_use", id: "t1", name: "Bash", input: { command: "npm run deploy" } }]),
    user("u2", 4, [{ tool_use_id: "t1", type: "tool_result", content: "deploy finished" }]),
    { type: "api-request-blob", sessionId: claudeId, body: "deploy deploy deploy" },
    say("side", 5, [{ type: "text", text: "a subagent says deploy" }], { isSidechain: true }),
    user("u3", 6, 'Say "deploy now" when it is safe.'),
    say("a3", 7, [{ type: "text", text: "Le café est prêt." }]),
  ]));
  run({ id: "run-claude", backend: "claude", task: "Objective: ship it\nDone when: live\n\nship the checkout", session: { nativeSessionId: claudeId }, workspace: { path: cwd } });

  const codexId = "01a1b2c3-d4e5-7f60-8a9b-0c1d2e3f4a5b";
  const day = path.join(home, ".codex", "sessions", "2026", "11", "20");
  fs.mkdirSync(day, { recursive: true });
  const item = (minute, payload) => ({ timestamp: at(minute), type: "response_item", payload });
  fs.writeFileSync(path.join(day, `rollout-2026-11-20T10-00-00-${codexId}.jsonl`), jsonl([
    { timestamp: at(10), type: "session_meta", payload: { id: codexId, instructions: "never deploy on Fridays" } },
    item(11, { type: "message", id: "msg_user1", role: "user", content: [{ type: "input_text", text: "deploy the billing worker" }] }),
    item(12, { type: "function_call", name: "shell", call_id: "c1", arguments: JSON.stringify({ command: ["bash", "-lc", "make deploy"] }) }),
    item(13, { type: "function_call_output", call_id: "c1", output: "deploy ok" }),
    { timestamp: at(14), type: "event_msg", payload: { type: "agent_message", message: "Billing worker deploy is done." } },
    item(14, { type: "message", role: "assistant", content: [{ type: "output_text", text: "Billing worker deploy is done." }] }),
  ]));
  run({ id: "run-codex", backend: "codex", task: "deploy the billing worker", session: { nativeSessionId: codexId } });

  const agyId = "7f0c2a52-1b9e-4c55-9f43-2d0a8e6c1b11";
  const logs = path.join(home, ".gemini", "antigravity-cli", "brain", agyId, ".system_generated", "logs");
  fs.mkdirSync(logs, { recursive: true });
  fs.writeFileSync(path.join(logs, "transcript_full.jsonl"), jsonl([
    { step_index: 0, source: "USER_EXPLICIT", type: "USER_INPUT", status: "DONE", created_at: at(20), content: "can you deploy the docs" },
    { step_index: 1, source: "MODEL", type: "PLANNER_RESPONSE", status: "DONE", created_at: at(21), content: "Deploying the docs.", tool_calls: [{ name: "run_command", args: { CommandLine: '"npm run deploy"' } }] },
    { step_index: 2, source: "MODEL", type: "GENERIC", status: "DONE", created_at: at(22), content: "Created At: x\nCommand output:\ndeploy done" },
  ]));
  run({ id: "run-agy", backend: "antigravity", task: "can you deploy the docs", session: { nativeSessionId: agyId } });
  // OpenCode keeps its chats in a database: passed over.
  run({ id: "run-opencode", backend: "opencode", task: "deploy", session: { nativeSessionId: "ses_1" } });
  return { root, home, projects: [{ workspaceId: "ws-shop", dir: repo }], repo };
}

test("searching chats finds what the user and the agents said, with the chat's own ids", async () => {
  const { root, home, projects, repo } = fixtures();
  try {
    const { results, partial } = await searchChats(projects, { home, query: "deploy" });
    assert.equal(partial, false);
    assert.deepEqual(results.map((r) => [r.runId, r.role, r.snippet]), [
      ["run-agy", "assistant", "Deploying the docs."],
      ["run-agy", "user", "can you deploy the docs"],
      ["run-codex", "assistant", "Billing worker deploy is done."],
      ["run-codex", "user", "deploy the billing worker"],
      ["run-claude", "user", 'Say "deploy now" when it is safe.'],
      ["run-claude", "assistant", "I will Deploy the checkout after the tests."],
    ], "newest first; tool calls, their output, API copies, sidechains and the Rudder framing are not what was said");
    // Each one is a message the chat shows, under the same id (chat-find.js opens it).
    for (const r of results) {
      const shown = readTranscript(repo, r.runId, { limit: 50, home }).messages.find((m) => m.id === r.messageId);
      assert.ok(shown, `${r.runId} shows ${r.messageId}`);
      assert.equal(shown.role, r.role);
      assert.equal(shown.ts, r.ts);
      assert.match(shown.text.toLowerCase(), /deploy/);
    }
    // As a JSON string holds it, and other letters in either case.
    const quoted = await searchChats(projects, { home, query: '"DEPLOY now"' });
    assert.deepEqual(quoted.results.map((r) => r.messageId), ["u3"]);
    const accented = await searchChats(projects, { home, query: "CAFÉ EST" });
    assert.deepEqual(accented.results.map((r) => [r.messageId, r.snippet]), [["a3:0", "Le café est prêt."]]);
    assert.deepEqual((await searchChats(projects, { home, query: "Fridays" })).results, [], "Codex's instructions are not a message");
    await assert.rejects(searchChats(projects, { home, query: " d " }), /at least 2 characters/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a chat gives its five newest matches, and the time budget leaves the rest out", async () => {
  const { root, home, projects } = fixtures();
  try {
    const claudeFile = fs.readdirSync(path.join(home, ".claude", "projects"), { recursive: true }).find((f) => f.endsWith(".jsonl"));
    const file = path.join(home, ".claude", "projects", claudeFile);
    const extra = Array.from({ length: 8 }, (_, i) => ({ parentUuid: null, isSidechain: false, type: "assistant", uuid: `n${i}`, timestamp: at(30 + i), message: { role: "assistant", content: [{ type: "text", text: `note ${i} about kestrels` }] } }));
    fs.appendFileSync(file, jsonl(extra));
    const { results } = await searchChats(projects, { home, query: "kestrels" });
    assert.deepEqual(results.map((r) => r.messageId), ["n7:0", "n6:0", "n5:0", "n4:0", "n3:0"]);
    const late = await searchChats(projects, { home, query: "kestrels", budgetMs: -1 });
    assert.deepEqual(late, { results: [], partial: true });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
