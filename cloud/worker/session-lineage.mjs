// Which part of an agent's session file is the conversation it is on now,
// and which message in it can be edited.
//
// Claude Code gives every entry a parentUuid. Going back to an earlier
// message (Claude's own rewind, or a prompt sent twice) leaves the old branch
// in the same file, so reading the file top to bottom shows both branches.
//
// Codex forks a thread (thread/fork) by writing a new rollout that refers
// back to the old one (session_meta.history_base: the old thread and how many
// bytes of its rollout came before the fork) instead of copying it. The new
// file alone holds only what came after the fork.
//
// Editing a sent message (native/src/chat_edit.rs) creates both: a Claude
// fork of a cut copy, a Codex fork through the turn before the edited prompt.

import fs from "node:fs";
import path from "node:path";

// ---------------------------------------------------------------------------
// Claude: the active branch.
// ---------------------------------------------------------------------------

const HARNESS_TEXT = /^<(local-command|command-|system-reminder|bash-|user-memory|task-notification)/;

function typedText(text) {
  const value = String(text || "").trim();
  return value !== "" && !HARNESS_TEXT.test(value);
}

// A prompt a person typed: what the chat shows as their message.
function claudeTypedPrompt(entry) {
  if (entry?.type !== "user" || entry.isSidechain || entry.isMeta || entry.isCompactSummary) {
    return false;
  }
  const content = entry.message?.content;
  if (typeof content === "string") {
    return typedText(content);
  }
  return Array.isArray(content)
    && !content.some((block) => block?.type === "tool_result")
    && content.some((block) => block?.type === "text" && typedText(block.text));
}

// A message sent while a turn runs follows a tool call or its result.
function claudeMidTurn(parent) {
  const inside = parent?.type === "user" ? "tool_result" : parent?.type === "assistant" ? "tool_use" : null;
  const content = parent?.message?.content;
  return !!inside && Array.isArray(content) && content.some((block) => block?.type === inside);
}

function parseLines(lines) {
  return lines.map((line) => {
    if (!line.trim()) {
      return null;
    }
    try {
      return JSON.parse(line);
    } catch {
      return null; // a partial line at either end of a tail read
    }
  });
}

/**
 * The lines of a Claude session file without the branches it went back from.
 *
 * The active path runs from the newest entry back along parentUuid. A compact
 * boundary starts a new chain (no parent), so the path continues from the
 * entry written just before it. Entries off the path stay unless their branch
 * starts with a typed prompt: parallel tool calls branch too (each result
 * hangs off its own call) and belong to the conversation, while a branch
 * starting with a prompt is one the user went back from.
 */
export function activeClaudeLines(lines) {
  const entries = parseLines(lines);
  const byUuid = new Map();
  let leaf = -1;
  entries.forEach((entry, index) => {
    if (entry?.uuid && !entry.isSidechain) {
      byUuid.set(entry.uuid, index);
      leaf = index;
    }
  });
  if (leaf < 0) {
    return lines;
  }
  const onPath = new Set();
  for (let index = leaf; index >= 0 && !onPath.has(index);) {
    onPath.add(index);
    const entry = entries[index];
    if (entry.parentUuid) {
      index = byUuid.get(entry.parentUuid) ?? -1;
    } else if (entry.type === "system" && entry.subtype === "compact_boundary") {
      index -= 1;
      while (index >= 0 && !(entries[index]?.uuid && !entries[index].isSidechain)) {
        index -= 1;
      }
    } else {
      index = -1;
    }
  }
  const abandoned = new Map(); // index -> whether its branch was gone back from
  const isAbandoned = (start) => {
    const walked = [];
    let index = start;
    let verdict = false;
    while (true) {
      if (abandoned.has(index)) {
        verdict = abandoned.get(index);
        break;
      }
      walked.push(index);
      const parent = byUuid.get(entries[index].parentUuid);
      if (parent === undefined || walked.includes(parent)) {
        break; // the start of the file (or of a tail read)
      }
      if (onPath.has(parent)) {
        verdict = claudeTypedPrompt(entries[index]);
        break;
      }
      index = parent;
    }
    for (const seen of walked) {
      abandoned.set(seen, verdict);
    }
    return verdict;
  };
  return lines.filter((_, index) => {
    const entry = entries[index];
    if (!entry?.uuid || entry.isSidechain || onPath.has(index)) {
      return true;
    }
    return !isAbandoned(index);
  });
}

// ---------------------------------------------------------------------------
// Codex: follow history_base back through the forks.
// ---------------------------------------------------------------------------

const rolloutPaths = new Map(); // thread id -> rollout file (rollouts never move)

function codexRolloutFile(home, threadId) {
  const known = rolloutPaths.get(threadId);
  if (known && fs.existsSync(known)) {
    return known;
  }
  const suffix = `-${threadId}.jsonl`;
  const walk = (dir, depth) => {
    let entries = [];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return null;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (depth < 3 && entry.isDirectory()) {
        const found = walk(full, depth + 1);
        if (found) {
          return found;
        }
      } else if (depth === 3 && entry.isFile() && entry.name.startsWith("rollout-") && entry.name.endsWith(suffix)) {
        return full;
      }
    }
    return null;
  };
  const found = walk(path.join(home, ".codex", "sessions"), 0);
  if (found) {
    rolloutPaths.set(threadId, found);
  }
  return found;
}

// The first line of a rollout is its session_meta.
function historyBase(file) {
  const fd = fs.openSync(file, "r");
  try {
    const chunks = [];
    const chunk = Buffer.alloc(64 * 1024);
    let position = 0;
    while (true) {
      const read = fs.readSync(fd, chunk, 0, chunk.length, position);
      if (read === 0) {
        break;
      }
      const piece = chunk.subarray(0, read);
      const newline = piece.indexOf(0x0a);
      chunks.push(Buffer.from(newline >= 0 ? piece.subarray(0, newline) : piece));
      if (newline >= 0) {
        break;
      }
      position += read;
    }
    const meta = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    const base = meta?.type === "session_meta" ? meta.payload?.history_base : null;
    if (!base) {
      return null;
    }
    if (typeof base.thread_id !== "string" || !/^[A-Za-z0-9_-]+$/.test(base.thread_id) || !Number.isInteger(base.end_byte_offset)) {
      throw new Error(`${file} has an unreadable history_base`);
    }
    return { threadId: base.thread_id, end: base.end_byte_offset };
  } finally {
    fs.closeSync(fd);
  }
}

/**
 * The byte ranges that make up a Codex thread's conversation, oldest first:
 * each base rollout up to its fork point, then the thread's own rollout.
 */
export function codexLineage(file, home) {
  const segments = [{ file, end: fs.statSync(file).size }];
  const seen = new Set();
  for (let base = historyBase(file); base; base = historyBase(segments[0].file)) {
    if (seen.has(base.threadId)) {
      throw new Error(`Codex conversation ${base.threadId} refers back to itself`);
    }
    seen.add(base.threadId);
    const baseFile = codexRolloutFile(home, base.threadId);
    if (!baseFile) {
      throw new Error(`This chat continues Codex conversation ${base.threadId}, which is missing from ${path.join(home, ".codex", "sessions")}`);
    }
    if (fs.statSync(baseFile).size < base.end) {
      throw new Error(`${baseFile} is shorter than the ${base.end} bytes this chat builds on`);
    }
    segments.unshift({ file: baseFile, end: base.end });
  }
  return segments;
}

/**
 * The last `maxBytes` of a Codex thread's whole conversation (readTail's
 * shape: a cut first line is dropped and `truncated` says so).
 */
export function readCodexLineage(file, home, maxBytes) {
  const segments = codexLineage(file, home);
  const total = segments.reduce((sum, segment) => sum + segment.end, 0);
  const parts = [];
  let left = maxBytes;
  for (let index = segments.length - 1; index >= 0 && left > 0; index -= 1) {
    const { file: segmentFile, end } = segments[index];
    const length = Math.min(left, end);
    const buffer = Buffer.alloc(length);
    const fd = fs.openSync(segmentFile, "r");
    try {
      fs.readSync(fd, buffer, 0, length, end - length);
    } finally {
      fs.closeSync(fd);
    }
    let text = buffer.toString("utf8");
    if (index < segments.length - 1 && text && !text.endsWith("\n")) {
      text += "\n";
    }
    parts.unshift(text);
    left -= length;
  }
  let text = parts.join("");
  const truncated = total > maxBytes;
  if (truncated) {
    text = text.slice(text.indexOf("\n") + 1);
  }
  return { text, truncated };
}

// ---------------------------------------------------------------------------
// The message the chat can edit.
// ---------------------------------------------------------------------------

/**
 * The chat's latest turn-starting prompt, when it can be edited:
 * `{ messageId, entry, text }`. `messageId` is the transcript message the
 * Edit action goes on, `entry` is what the edit request names (Claude's entry
 * uuid, Codex's message id; native chat_edit.rs checks it is still the latest
 * prompt). Null when there is none or it is the first message in `lines`.
 * Pass Claude lines through activeClaudeLines first.
 */
export function editablePrompt(lines, backend) {
  return backend === "codex" ? codexEditable(lines) : claudeEditable(lines);
}

function claudeEditable(lines) {
  const entries = parseLines(lines);
  const byUuid = new Map();
  for (const entry of entries) {
    if (entry?.uuid) {
      byUuid.set(entry.uuid, entry);
    }
  }
  const prompts = entries.filter((entry) => entry && claudeTypedPrompt(entry) && !claudeMidTurn(byUuid.get(entry.parentUuid)));
  if (prompts.length < 2) {
    return null;
  }
  const latest = prompts[prompts.length - 1];
  const content = latest.message.content;
  if (typeof content === "string") {
    return { messageId: latest.uuid, entry: latest.uuid, text: content.trim() };
  }
  // The transcript makes one message per text block: `${uuid}:${index}`.
  const index = content.findIndex((block) => block?.type === "text" && typedText(block.text));
  return { messageId: `${latest.uuid}:${index}`, entry: latest.uuid, text: String(content[index].text).trim() };
}

const CODEX_HARNESS_TEXT = /^<(environment_context|user_instructions|permissions|turn_aborted)/;

function codexEditable(lines) {
  let turns = 0;
  let prompted = false;
  let latest = null;
  for (const entry of parseLines(lines)) {
    const payload = entry?.payload;
    if (entry?.type === "event_msg" && payload?.type === "task_started") {
      turns += 1;
      prompted = false;
      continue;
    }
    if (entry?.type !== "response_item" || payload?.type !== "message" || payload.role !== "user") {
      continue;
    }
    const text = (payload.content || []).map((part) => part?.text || "").join("\n").trim();
    if (!text || CODEX_HARNESS_TEXT.test(text) || text.startsWith("# AGENTS.md instructions")) {
      continue;
    }
    if (!prompted) {
      latest = { turn: turns, id: payload.id, text };
    }
    prompted = true;
  }
  // The edit forks through the turn before this one, so it needs one.
  if (!latest || latest.turn < 2 || typeof latest.id !== "string" || !latest.id) {
    return null;
  }
  return { messageId: latest.id, entry: latest.id, text: latest.text };
}
