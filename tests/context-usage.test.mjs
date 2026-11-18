// The chat's context numbers (cloud/worker/context-usage.mjs): context in use
// and window from the session log, and where Claude Code and Codex compact.
// Fixtures are synthetic; the threshold rules are the ones measured on a home.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { contextUsage } from "../cloud/worker/context-usage.mjs";

const jsonl = (entries) => entries.map((entry) => `${JSON.stringify(entry)}\n`).join("");

function tempHome(t) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "context-usage-"));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  return home;
}

function write(file, body) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, typeof body === "string" ? body : JSON.stringify(body));
  return file;
}

function claudeHome(t, { user = {}, project = null } = {}) {
  const home = tempHome(t);
  const cwd = path.join(home, "repo");
  write(path.join(home, ".rudder", "models-dev.json"), {
    anthropic: { models: { "claude-opus-5-5": { limit: { context: 1_000_000, output: 128_000 } } } },
  });
  write(path.join(home, ".claude", "settings.json"), user);
  if (project) {
    write(path.join(cwd, ".claude", "settings.json"), project);
  }
  return { home, record: { backend: "claude", model: "opus", workspace: { path: cwd } } };
}

const response = (id, usage, extra = {}) => ({
  type: "assistant",
  timestamp: "2026-11-18T10:00:00.000Z",
  message: { id, model: "claude-opus-5-5", usage, stop_reason: "end_turn" },
  ...extra,
});

test("Claude: newest response's tokens, its compaction, and the threshold from /autocompact", (t) => {
  const { home, record } = claudeHome(t, {
    user: { autoCompactWindow: 150_000, modelSettings: { "claude-opus-5-5": { autoCompactWindow: 400_000 } } },
  });
  // The newest line is still being written.
  const writing = JSON.stringify(response("m3", { input_tokens: 170_000 }));
  const file = write(path.join(home, "s.jsonl"), jsonl([
    response("m1", { input_tokens: 2, cache_creation_input_tokens: 900, cache_read_input_tokens: 160_000, output_tokens: 500 }),
    // A subagent's response is not this chat's context.
    response("m2", { input_tokens: 9_999_999 }, { isSidechain: true }),
  ]) + writing.slice(0, 60));
  // Measured: 400K sessions compacted just above 367,000.
  assert.deepEqual(contextUsage(record, file, home), {
    used: 161_402,
    // What `/context` counts against: the auto-compact window, not the model's 1M.
    window: 400_000,
    autoCompactAt: 367_000,
    source: "Auto-compact window 400K (autoCompactWindow in user settings) of the model's 1M. Claude Code compacts 33K before it.",
    at: Date.parse("2026-11-18T10:00:00.000Z"),
  });

  // After a compaction, its own post-compaction size until the next response.
  fs.appendFileSync(file, `${writing.slice(60)}\n${JSON.stringify({ type: "system", subtype: "compact_boundary", timestamp: "2026-11-18T10:05:00.000Z", compactMetadata: { trigger: "auto", preTokens: 367_400, postTokens: 48_000 } })}\n`);
  const compacted = contextUsage(record, file, home);
  assert.equal(compacted.used, 48_000);
  assert.equal(compacted.at, Date.parse("2026-11-18T10:05:00.000Z"));
});

test("Claude: a project's top-level window replaces per-model ones, a window past the model's is capped, unset is unknown", (t) => {
  const usage = { input_tokens: 10_000, output_tokens: 1_000 };
  const capped = claudeHome(t, {
    user: { modelSettings: { "claude-opus-5-5": { autoCompactWindow: 400_000 } } },
    project: { autoCompactWindow: 2_000_000 },
  });
  const file = write(path.join(capped.home, "s.jsonl"), jsonl([response("m1", usage)]));
  const result = contextUsage(capped.record, file, capped.home);
  assert.equal(result.window, 1_000_000);
  assert.equal(result.autoCompactAt, 1_000_000 - 33_000);
  assert.equal(result.source, "Auto-compact window 2M (autoCompactWindow in project settings), capped at the model's 1M. Claude Code compacts 33K before it.");

  // No window set: Claude Code uses its own server-side default.
  const unset = claudeHome(t);
  const unsetFile = write(path.join(unset.home, "s.jsonl"), jsonl([response("m1", usage)]));
  assert.deepEqual(
    { ...contextUsage(unset.record, unsetFile, unset.home), at: null },
    { used: 11_000, window: 1_000_000, autoCompactAt: null, source: null, at: null },
  );

  // Off through a settings env block.
  const off = claudeHome(t, { user: { env: { DISABLE_AUTO_COMPACT: "1" }, autoCompactWindow: 400_000 } });
  const offFile = write(path.join(off.home, "s.jsonl"), jsonl([response("m1", usage)]));
  const offResult = contextUsage(off.record, offFile, off.home);
  assert.equal(offResult.autoCompactAt, null);
  assert.equal(offResult.source, "Auto-compact is off (DISABLE_AUTO_COMPACT)");
});

test("Codex: the newest token count, and the auto-compact limit Codex logged for the session", (t) => {
  const home = tempHome(t);
  const thread = "01a1ed00-3171-76f1-993f-37eb4ca79ab2";
  const count = (total, ts) => ({
    timestamp: ts,
    type: "event_msg",
    payload: { type: "token_count", info: { last_token_usage: { input_tokens: total - 100, output_tokens: 100, total_tokens: total }, model_context_window: 475_000 } },
  });
  const file = write(path.join(home, ".codex", "sessions", "2026", "11", "18", `rollout-2026-11-18T04-52-55-${thread}.jsonl`), jsonl([
    count(149_000, "2026-11-18T05:09:00.000Z"),
    { timestamp: "2026-11-18T05:09:39.150Z", type: "compacted", payload: { message: "" } },
    count(30_109, "2026-11-18T05:09:39.152Z"),
  ]));
  const record = { backend: "codex", session: { nativeSessionId: thread } };

  // No Codex log on this machine: the limit is not known.
  assert.deepEqual(contextUsage(record, file, home), { used: 30_109, window: 475_000, autoCompactAt: null, source: null, at: Date.parse("2026-11-18T05:09:39.152Z") });

  const { DatabaseSync } = createRequire(import.meta.url)("node:sqlite");
  const db = new DatabaseSync(path.join(home, ".codex", "logs_2.sqlite"));
  db.exec("CREATE TABLE logs (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, ts_nanos INTEGER NOT NULL, level TEXT NOT NULL, target TEXT NOT NULL, feedback_log_body TEXT, thread_id TEXT)");
  const log = db.prepare("INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body, thread_id) VALUES (?, 0, 'TRACE', 'codex_core::session::turn', ?, ?)");
  const line = (limit) => `run_turn{}: post sampling token usage turn_id=x total_usage_tokens=30109 auto_compact_scope_tokens=30109 auto_compact_scope_limit=${limit} auto_compact_limit_scope=Total full_context_window_limit=Some(475000)`;
  log.run(1, line("Some(244800)"), thread);
  log.run(2, line("Some(150000)"), thread);
  log.run(3, line("Some(99)"), "another-thread");
  db.close();
  fs.appendFileSync(file, jsonl([count(31_000, "2026-11-18T05:10:00.000Z")]));
  const result = contextUsage(record, file, home);
  assert.equal(result.used, 31_000);
  assert.equal(result.autoCompactAt, 150_000);
  assert.equal(result.source, "Codex's auto-compact limit for this session, from its log");
});
