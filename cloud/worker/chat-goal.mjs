// Whether an agent's chat has a standing goal, the one `/goal <condition>`
// sets in Claude Code and in Codex. The chat shows "Goal active" while it
// has one (cloud/app/goal-chip.js):
//
//   { text } while a goal is active, else null
//
// Claude Code (2.1.x) appends each change to the session's JSONL as an
// attachment entry, `{ type: "goal_status", met, condition, ... }`:
//   /goal <condition>                met false, sentinel true
//   after each compaction            the same again, so the goal outlives it
//   a check finds it is not met      met false, with the evaluator's reason
//   the goal is met                  met true, with reason and iterations
//   the goal is impossible           met false, failed true
//   /goal clear, or an unrecoverable met true, sentinel true
//     error clears it
// The newest entry decides, as it does when Claude Code resumes a session:
// the goal is active unless that entry is met or failed. A new /goal writes
// its own entry over the old one, and /clear starts a new session file.
//
// Codex (0.170) keeps one goal per thread in ~/.codex/goals_1.sqlite,
// `thread_goals` (objective, status). Its rollout logs only some changes
// (thread_goal_updated while active, nothing when it turns blocked), so the
// table decides: active while its status is "active". Paused, blocked,
// usage_limited, budget_limited and complete are not.
//
// Either way a goal is only pursued while the agent's process runs; the
// caller checks that (transcripts.mjs).

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

const CHUNK_BYTES = 4 * 1024 * 1024;
const MARKER = Buffer.from('"type":"goal_status"');
const CODEX_GOALS = "goals_1.sqlite";
const MEMO_MAX = 200;

/** The active goal of the run `record`, from its session log `file` (transcripts.mjs transcriptFile). */
export function activeGoal(record, file, home) {
  return record.backend === "codex" ? codexGoal(record, home) : claudeGoal(file);
}

// ---------------------------------------------------------------------------
// Claude Code

// file -> { offset, goal }: the session is read once, then only what it
// gained since. A long session holds no goal entry near its end, so it is
// not read back from the end each poll.
const claudeScans = new Map();

function claudeGoal(file) {
  const fd = fs.openSync(file, "r");
  try {
    const size = fs.fstatSync(fd).size;
    const hit = claudeScans.get(file);
    // Claude Code only appends; a shorter file is a new one.
    const scan = hit && hit.offset <= size ? hit : { offset: 0, goal: null };
    let position = scan.offset;
    let carry = null; // a line whose end is not read yet
    while (position < size) {
      const chunk = Buffer.alloc(Math.min(CHUNK_BYTES, size - position));
      fs.readSync(fd, chunk, 0, chunk.length, position);
      position += chunk.length;
      const data = carry ? Buffer.concat([carry, chunk]) : chunk;
      const end = data.lastIndexOf(10) + 1;
      if (end > 0) {
        scan.goal = newestGoal(data.subarray(0, end), scan.goal, file);
      }
      carry = data.subarray(end);
    }
    // A last line still being written is read with what follows it.
    scan.offset = size - (carry?.length ?? 0);
    remember(claudeScans, file, scan);
    return scan.goal;
  } finally {
    fs.closeSync(fd);
  }
}

/**
 * The goal after the complete lines in `buffer`: `goal` (the one before
 * them) unless one of them is a goal_status entry, then the newest of those.
 */
export function newestGoal(buffer, goal, file = "the session") {
  for (let at = buffer.indexOf(MARKER); at !== -1; ) {
    const start = buffer.lastIndexOf(10, at) + 1;
    const end = buffer.indexOf(10, at);
    const entry = JSON.parse(buffer.toString("utf8", start, end));
    // The marker can sit deeper in another entry: a subagent's progress.
    if (entry.type === "attachment" && entry.attachment?.type === "goal_status" && !entry.isSidechain) {
      goal = claudeStatus(entry.attachment, file);
    }
    at = buffer.indexOf(MARKER, end);
  }
  return goal;
}

function claudeStatus(status, file) {
  if (status.met === true || status.failed === true) {
    return null;
  }
  if (typeof status.condition !== "string" || !status.condition) {
    throw new Error(`Claude Code's goal_status entry no longer has a condition (${file})`);
  }
  return { text: status.condition };
}

// ---------------------------------------------------------------------------
// Codex

let sqlite = null;

function codexGoal(record, home) {
  const threadId = record.session?.nativeSessionId;
  if (typeof threadId !== "string" || !threadId) {
    throw new Error("this Codex chat has no thread id");
  }
  const db = path.join(home, ".codex", CODEX_GOALS);
  if (!fs.existsSync(db)) {
    return null; // no goal was ever set on this machine
  }
  // Loaded on first use: Node 22 warns once that node:sqlite is experimental.
  sqlite ??= createRequire(import.meta.url)("node:sqlite");
  const conn = new sqlite.DatabaseSync(db, { readOnly: true });
  try {
    const row = conn.prepare("SELECT objective, status FROM thread_goals WHERE thread_id = ?").get(threadId);
    return row?.status === "active" ? { text: String(row.objective) } : null;
  } finally {
    conn.close();
  }
}

function remember(memo, key, value) {
  memo.delete(key);
  memo.set(key, value);
  if (memo.size > MEMO_MAX) {
    memo.delete(memo.keys().next().value);
  }
}
