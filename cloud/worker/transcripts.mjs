// Read an agent's conversation off disk for the phone app.
//
// Claude Code keeps one JSONL per session at
//   ~/.claude/projects/<cwd with every non-alphanumeric char as "->/<id>.jsonl
// Codex keeps rollouts at ~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl,
// and Antigravity (agy) keeps one per conversation at
//   ~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript.jsonl.
// All are normalized to { id, role, ts, text, tool } messages; a user message
// with files sent from the app also has `attachments`.

import fs from "node:fs";
import path from "node:path";
import { pidAlive } from "./agent-status.mjs";
import { contextUsage } from "./context-usage.mjs";
import { activeGoal } from "./chat-goal.mjs";
import { promptSuggestion } from "./suggestion.mjs";
import { handoffShown } from "./handoff.mjs";
import { claudeCallFiles, claudeResultFiles, codexChangedFiles, opencodeFiles, setToolFiles, withFiles } from "./tool-files.mjs";
import { isOpencodeSessionId, opencodeChildSessions, opencodeContextWindow, opencodeDb, opencodeFirstPrompt, opencodeLastText, opencodeMessages, opencodeSessionExists } from "./opencode.mjs";
import { CLAUDE_HARNESS_TEXT, activeClaudeLines, codexLineage, codexRolloutFile, codexTypedText, editablePrompts, readSessionLines, unwrapPastedContent } from "./session-lineage.mjs";

const TAIL_BYTES = 6 * 1024 * 1024;
const PREVIEW_TAIL_BYTES = 256 * 1024;
const TOOL_OUTPUT_LIMIT = 12_000;
// A subagent's card carries its newest messages, with shorter tool output: the
// chat is polled every few seconds and a turn can start dozens of subagents.
const SUBAGENT_KEEP = 40;
const SUBAGENT_OUTPUT_LIMIT = 2_000;
const SUBAGENT_PROMPT_LIMIT = 6_000;
// Subagents of subagents are shown this many levels deep.
const SUBAGENT_DEPTH = 3;
// The agent's process is up, so its unfinished subagents and teammates are
// still running: they go on after the agent's own turn ends. Otherwise they
// stopped with it.
function subagentsLive(record) {
  const pid = record.process?.pid;
  return Number.isInteger(pid) && pidAlive(pid);
}

/**
 * One page of an agent's chat: its newest messages, or with `before` (a
 * cursor from a page's `earlier`) the messages before that page, or with
 * `after` (one too) every message from that point on. A page reads back at
 * most TAIL_BYTES of the session and keeps about `limit` messages; `earlier`
 * is the cursor for the page before it, null at the start of the session.
 * Pages read before a cursor carry only their messages.
 */
export function readTranscript(repoDir, runId, { limit = 300, home, before = null, after = null }) {
  const record = readRunRecord(repoDir, runId);
  if (!record) {
    throw new Error("agent not found");
  }
  if (before !== null && after !== null) {
    throw new Error("a chat page is read before a cursor or after one, not both");
  }
  if (record.backend === "opencode") {
    return opencodeTranscript(record, { home, limit, before, after });
  }
  const file = transcriptFile(record, home);
  const conversation = file ? record.session.nativeSessionId : null;
  const end = before === null ? null : readCursor(before);
  if (end && end.conversation !== conversation) {
    throw new Error("This chat started a new conversation since it opened. Reopen the chat to see it.");
  }
  // A cursor from the run's previous conversation (an edit starts a new one)
  // does not apply: the page is the new conversation's newest messages, and
  // its `conversation` tells the app to drop the pages it loaded before.
  const from = after === null ? null : readCursor(after);
  const start = from && from.conversation === conversation ? from.offset : null;
  const base = {
    runId: record.id,
    conversation,
    earlier: null,
    messages: [],
  };
  if (!end) {
    base.title = (record.taskSummary || record.task || record.id || "").toString();
    base.backend = record.backend || "claude";
    base.status = record.status || null;
  }
  if (!file) {
    // No session on disk yet (just launched, or a backend without transcripts):
    // show the prompts Rudder itself recorded.
    base.messages = showAttachments(recordedTurns(record).map(showHandoff));
    return base;
  }
  const backend = record.backend || "claude";
  const page = readPage(sessionSegments(record, file, home), {
    backend,
    end: end?.offset ?? null,
    start,
    leaf: end?.leaf ?? "",
    limit,
    stats: end ? null : STATS[backend],
  });
  base.messages = showAttachments(showUserTask(page.messages, record));
  base.earlier = page.start > 0 ? `${conversation}:${page.start}:${page.leaf}` : null;
  // Subagents the page started, each on its spawn call.
  const shown = new Set(base.messages);
  const live = subagentsLive(record);
  if (backend === "codex") {
    attachCodexSubagents(page.reader, { home, live, shown, depth: 0 });
  } else if (backend === "claude") {
    attachClaudeSubagents(page.reader, { dir: claudeSubagentsDir(file), projectDir: path.dirname(file), live, shown, depth: 0 });
  }
  if (end) {
    return base;
  }
  // The prompts the app may offer to edit (chat-edit.js). agy cannot branch a
  // conversation from an earlier turn, so an Antigravity chat offers none.
  base.edits = backend === "antigravity" ? [] : editablePrompts(page.lines, backend);
  // Claude Code's suggested next prompt, saved by Rudder's Claude plugin.
  base.suggestion = promptSuggestion(home, record.id);
  // A compaction under way. Its session gets nothing until it ends, so this
  // is the dashboard's reading of the pane (run.json `compactingSince`, Unix
  // ms). It is over once the process is gone or its marker has landed.
  const since = record.compactingSince;
  const landed = Number.isFinite(since) && base.messages.some((m) => m.kind === "compact" && m.ts >= since);
  base.compacting = live && Number.isFinite(since) && !landed ? { since } : null;
  // The numbers are extra: a model catalog or settings file that cannot be
  // read says so in `stats.error` and the conversation still shows.
  try {
    base.stats = page.stats.result(home);
  } catch (error) {
    base.stats = { error: error instanceof Error ? error.message : String(error) };
  }
  // Context left and where the backend compacts (context-usage.mjs), the
  // same way: a failed read says so and the conversation still shows.
  try {
    base.context = contextUsage(record, file, home);
  } catch (error) {
    base.context = { error: error instanceof Error ? error.message : String(error) };
  }
  // The standing goal `/goal` set (chat-goal.mjs), while the agent runs to pursue
  // it. A failed read says so and the conversation still shows.
  try {
    base.goal = live ? activeGoal(record, file, home) : null;
  } catch (error) {
    base.goal = { error: error instanceof Error ? error.message : String(error) };
  }
  return base;
}

// A page cursor: `<conversation>:<offset>:<leaf>`. The conversation is the
// session the page came from, the offset is the byte (across a Codex fork's
// history, session-lineage.mjs codexLineage) a page starts at, and the leaf
// is the Claude entry the conversation continues from before it
// (activeClaudeLines). The app passes it back as it got it.
const CURSOR = /^([A-Za-z0-9_-]+):(\d+):([A-Za-z0-9-]*)$/;

function readCursor(value) {
  const match = CURSOR.exec(String(value));
  if (!match) {
    throw new Error(`${JSON.stringify(String(value))} is not a chat page cursor`);
  }
  return { conversation: match[1], offset: Number(match[2]), leaf: match[3] };
}

// A session's conversation as byte ranges, oldest first: a Codex fork's
// history, then its own rollout (session-lineage.mjs); a Claude session is
// its one file.
function sessionSegments(record, file, home) {
  return record.backend === "codex" ? codexLineage(file, home) : [{ file, end: fs.statSync(file).size }];
}

// The messages from byte `start` (when given) or about `limit` messages back
// to byte `end` (the end of the session when null). Without `start`, it reads
// back TAIL_BYTES, and twice as far each time that holds no message, until
// one does or it reaches the session's start.
function readPage(segments, { backend, end, start, leaf, limit, stats }) {
  const total = segments.reduce((sum, segment) => sum + segment.end, 0);
  const stop = end ?? total;
  if (stop > total || (start ?? 0) > stop) {
    throw new Error("This chat's session file is shorter than the page asked for. Reopen the chat.");
  }
  for (let bytes = TAIL_BYTES; ; bytes *= 2) {
    const from = start ?? Math.max(0, stop - bytes);
    const window = readSessionLines(segments, from, stop);
    const branch = backend === "claude" ? activeClaudeLines(window.lines, leaf, from === 0) : null;
    const lines = branch ? window.lines.filter((_, index) => branch.keep[index]) : window.lines;
    const offsets = branch ? window.offsets.filter((_, index) => branch.keep[index]) : window.offsets;
    const page = parsePage(lines, offsets, { backend, stats: stats?.() ?? null });
    if (start === null && from > 0 && !page.reader.messages.length) {
      continue;
    }
    const cut = start ?? pageStart(page, { first: from === 0 ? 0 : window.offsets[0], limit });
    const kept = page.born.findIndex((offset) => offset >= cut);
    const firstLine = window.offsets.findIndex((offset) => offset >= cut);
    return {
      messages: kept < 0 ? [] : page.reader.messages.slice(kept),
      reader: page.reader,
      lines,
      stats: page.stats,
      start: cut,
      leaf: branch ? branch.leafBefore(firstLine < 0 ? window.offsets.length : firstLine) : "",
    };
  }
}

// A session's lines read into chat messages: the reader, each message's
// line (`born`, by offset) and each tool result's span (`results`: the
// offset of its call's line, -1 when that came before these lines, and its
// own).
function parsePage(lines, offsets, { backend, stats }) {
  const reader = READERS[backend]({ stats });
  const born = [];
  const bornAt = new Map(); // message -> offset of its line
  const results = [];
  let offset = 0;
  reader.onResult = (call) => results.push([call ? bornAt.get(call) : -1, offset]);
  lines.forEach((line, index) => {
    offset = offsets[index];
    const count = reader.messages.length;
    reader.addLine(line, offset);
    for (let at = count; at < reader.messages.length; at += 1) {
      born.push(offset);
      bornAt.set(reader.messages[at], offset);
    }
  });
  return { reader, born, results, stats };
}

// Where a page starts: the newest line that still keeps `limit` messages
// (all of them when there are fewer), moved to where no tool call is split
// from its result, so the page before holds each of its calls' results. The
// candidates are the read's first line (`first`, 0 at the session's start)
// and each line a message starts on.
function pageStart({ born, results }, { first, limit }) {
  const candidates = [first];
  const keeps = [0]; // the first message each candidate keeps
  born.forEach((offset, index) => {
    if (offset > candidates[candidates.length - 1]) {
      candidates.push(offset);
      keeps.push(index);
    }
  });
  // A candidate splits a call from its result when the call's line is
  // before it and the result's at or after it.
  const split = new Array(candidates.length + 1).fill(0);
  for (const [call, result] of results) {
    const low = firstAbove(candidates, call);
    const high = firstAbove(candidates, result) - 1;
    if (low <= high) {
      split[low] += 1;
      split[high + 1] -= 1;
    }
  }
  let depth = 0;
  const whole = candidates.map((candidate, index) => {
    depth += split[index];
    return depth === 0 || candidate === 0;
  });
  const most = Math.max(0, born.length - limit);
  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    if (keeps[index] <= most && whole[index]) {
      // Of the read's first line and its first message's, which keep the
      // same messages, the first line leaves no empty page before it.
      return index === 1 && keeps[0] === keeps[1] && whole[0] ? candidates[0] : candidates[index];
    }
  }
  const later = whole.findIndex((ok, index) => ok && keeps[index] > most);
  return candidates[later < 0 ? 0 : later];
}

// The index of the first sorted value above `value`.
function firstAbove(sorted, value) {
  let low = 0;
  let high = sorted.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (sorted[middle] > value) {
      high = middle;
    } else {
      low = middle + 1;
    }
  }
  return low;
}

// The newest assistant text, for the agent list's preview line. Re-reads only
// when the transcript file changed.
// A working agent's transcript changes every second; its preview line only
// needs to keep up with a person glancing at the phone.
const PREVIEW_REFRESH_MS = 5_000;

export function lastAssistantText(record, { home, cache, now = Date.now() }) {
  if (record.backend === "opencode") {
    return opencodePreview(record, { home, cache, now });
  }
  const file = transcriptFile(record, home, now);
  if (!file) {
    return null;
  }
  if (cache && cache.file === file && now - (cache.readAt ?? 0) < PREVIEW_REFRESH_MS) {
    return { text: cache.text, remoteUrl: cache.remoteUrl ?? null, cacheEntry: cache };
  }
  const stat = fs.statSync(file);
  if (cache && cache.file === file && cache.mtimeMs === stat.mtimeMs && cache.size === stat.size) {
    return { text: cache.text, remoteUrl: cache.remoteUrl ?? null, cacheEntry: cache };
  }
  const backend = record.backend || "claude";
  const segments = sessionSegments(record, file, home);
  const total = segments.reduce((sum, segment) => sum + segment.end, 0);
  const window = readSessionLines(segments, Math.max(0, total - PREVIEW_TAIL_BYTES), total);
  const branch = backend === "claude" ? activeClaudeLines(window.lines) : null;
  const lines = branch ? window.lines.filter((_, index) => branch.keep[index]) : window.lines;
  const offsets = branch ? window.offsets.filter((_, index) => branch.keep[index]) : window.offsets;
  const { messages } = parsePage(lines, offsets, { backend, stats: null }).reader;
  const last = [...messages].reverse().find((message) => message.role === "assistant" && message.text?.trim());
  const preview = last ? last.text.replace(/\s+/g, " ").trim().slice(0, 280) : null;
  const remoteUrl = backend === "claude" ? claudeSessionUrl(lines) ?? cache?.remoteUrl ?? null : null;
  const entry = { file, mtimeMs: stat.mtimeMs, size: stat.size, readAt: now, text: preview, remoteUrl };
  return { text: preview, remoteUrl, cacheEntry: entry };
}

// Claude records the session's claude.ai URL (where Remote Control shows it)
// as an "attachment" entry. The newest one wins.
function claudeSessionUrl(lines) {
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const line = lines[index];
    if (!line.includes('"attachment"') || !line.includes("claude.ai/code/session_")) {
      continue;
    }
    try {
      const url = JSON.parse(line)?.attachment?.url;
      if (typeof url === "string" && /^https:\/\/claude\.ai\/code\/session_[A-Za-z0-9_-]+$/.test(url)) {
        return url;
      }
    } catch {
      // partial line
    }
  }
  return null;
}

// Rudder launches an agent with its own framing around the request
// ("Objective: ...", "Rudder-specific context injected by Rudder: ...", a
// discussion's scope, a main agent's or the All agents chat's context). In
// the chat that turn is what the user asked for, so it shows as the task
// itself, which is also the text Edit offers to change (chat-edit.js).
const RUDDER_FRAMING = /Rudder-specific context injected by Rudder|^Objective: [\s\S]*\nDone when:|^Rudder discussion scope:|\n\nRudder: you own this request in the user's checkout\.|\n\nRudder: you are the All agents chat\./;

function showUserTask(messages, record) {
  const task = userTask(record.task);
  return messages.map((message) => {
    if (message.role !== "user") {
      return message;
    }
    const moved = showHandoff(message);
    if (moved !== message || !RUDDER_FRAMING.test(message.text)) {
      return moved;
    }
    return task ? { ...message, text: task } : message;
  });
}

// A chat moved here from another harness opens with the handoff its new
// harness was given (handoff.mjs). The chat shows it as one marker, "Moved
// from Claude · Opus 5.5", with what was handed over behind it.
function showHandoff(message) {
  const moved = message.role === "user" ? handoffShown(message.text) : null;
  return moved
    ? { id: message.id, role: "system", kind: "handoff", ts: message.ts, text: `Moved from ${moved.from}`, handoff: moved }
    : message;
}

// The prompts Rudder recorded for an agent, as chat messages.
function recordedTurns(record) {
  return (record.turns || []).map((turn, index) => ({
    id: `turn-${index}`,
    role: "user",
    ts: toMillis(turn.ts),
    text: String(turn.prompt || ""),
  }));
}

/**
 * An agent's newest `limit` messages as the chat shows them and the session
 * lines they came from, for a handoff to another harness (handoff.mjs). It
 * reads no stats, subagents or edits, so the switch does not wait on them.
 */
export function recentConversation(repoDir, runId, { home, limit }) {
  const record = readRunRecord(repoDir, runId);
  if (!record) {
    throw new Error("agent not found");
  }
  const backend = record.backend || "claude";
  if (backend === "opencode") {
    const page = opencodeSessionPage(record, home, { end: null, start: null, limit, stats: false });
    return page
      ? { record, backend, lines: page.lines, messages: showAttachments(showUserTask(page.messages, record)) }
      : { record, backend, lines: [], messages: showUserTask(recordedTurns(record), record) };
  }
  const file = transcriptFile(record, home);
  if (!file) {
    return { record, backend, lines: [], messages: showUserTask(recordedTurns(record), record) };
  }
  const page = readPage(sessionSegments(record, file, home), { backend, end: null, start: null, leaf: "", limit, stats: null });
  return { record, backend, lines: page.lines, messages: showAttachments(showUserTask(page.messages, record)) };
}

// Photos and files sent from the app reach the agent as saved paths listed
// after the message (cloud/app/pending.js withAttachments; home.mjs
// receiveUpload saves them in the repo's .rudder/uploads). The chat shows the
// words as the text and the files as `attachments`. A list the user typed or
// pasted, of files saved anywhere else, stays part of their words.
const ATTACHED_FILES = /(?:^|\n\n)Attached files:\n((?:- \/[^\n]*\/\.rudder\/uploads\/[^\n]+(?:\n|$))+)\s*$/;

export function showAttachments(messages) {
  return messages.map((message) => {
    const match = message.role === "user" ? ATTACHED_FILES.exec(message.text) : null;
    if (!match) {
      return message;
    }
    const attachments = match[1].trim().split("\n").map((line) => line.slice(2).trim());
    return { ...message, text: message.text.slice(0, match.index).trim(), attachments };
  });
}

// What the user asked, without the framing Rudder wraps it in. A task started
// from the task bar is stored framed: "Objective: ...\nDone when: ...\n\n<ask>".
export function userTask(raw) {
  let text = String(raw || "").trim();
  text = text.replace(/^Objective: [^\n]*\nDone when: [^\n]*\n+/, "");
  const context = text.indexOf("Rudder-specific context injected by Rudder");
  if (context >= 0) {
    // The ask follows the context block (its last paragraph).
    const after = text.slice(context).split(/\n\s*\n/).slice(1).join("\n\n").trim();
    text = after || text.slice(0, context).trim();
  }
  return text.trim();
}

function readRunRecord(repoDir, runId) {
  if (!/^[A-Za-z0-9._-]+$/.test(runId)) {
    return null;
  }
  let body;
  try {
    body = fs.readFileSync(path.join(repoDir, ".rudder", "runs", runId, "run.json"), "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null;
    }
    throw error;
  }
  let record;
  try {
    record = JSON.parse(body);
  } catch (error) {
    // Caught mid-write: the next poll reads it whole.
    throw new Error(`could not read agent ${runId}: ${error.message}`);
  }
  return record?.id === runId ? record : null;
}

// Where each session's transcript lives, found once. A miss means searching
// every Claude project folder (hundreds on a long-lived home) or every Codex
// day folder, so it is remembered too and searched again at most every
// MISS_RECHECK_MS. Where the session would appear now is checked every time
// (Claude: its own folder, one stat; Codex: today's folders), so a session
// that starts writing after a miss (a new agent, a branch) shows at once.
const MISS_RECHECK_MS = 30_000;
const transcriptPaths = new Map(); // sessionId -> { file, checkedAt }

export function transcriptFile(record, home, now = Date.now()) {
  const sessionId = record?.session?.nativeSessionId;
  if (!sessionId || !/^[A-Za-z0-9-]+$/.test(sessionId)) {
    return null;
  }
  if (record.backend === "antigravity") {
    // agy files a conversation under its id alone: one stat, nothing to search.
    const file = path.join(home, ".gemini", "antigravity-cli", "brain", sessionId, ".system_generated", "logs", "transcript.jsonl");
    return fs.existsSync(file) ? file : null;
  }
  const known = transcriptPaths.get(sessionId);
  if (known?.file && fs.existsSync(known.file)) {
    return known.file;
  }
  if (known && !known.file && now - known.checkedAt < MISS_RECHECK_MS) {
    const file = record.backend === "codex" ? codexNewRollout(home, sessionId, now) : claudeCwdTranscript(record, home, sessionId);
    if (file) {
      transcriptPaths.set(sessionId, { file, checkedAt: now });
    }
    return file;
  }
  const file = findTranscript(record, home, sessionId);
  transcriptPaths.set(sessionId, { file, checkedAt: now });
  return file;
}

function findTranscript(record, home, sessionId) {
  if (record.backend === "codex") {
    return codexRolloutFile(home, sessionId);
  }
  const direct = claudeCwdTranscript(record, home, sessionId);
  if (direct) {
    return direct;
  }
  const projects = path.join(home, ".claude", "projects");
  let dirs;
  try {
    dirs = fs.readdirSync(projects);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null; // no Claude sessions on this machine yet
    }
    throw error;
  }
  for (const dir of dirs) {
    const candidate = path.join(projects, dir, `${sessionId}.jsonl`);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return null;
}

// Claude files a session under its cwd with every other character as "-",
// except that a long cwd is cut and hashed: those are found by name.
function claudeCwdTranscript(record, home, sessionId) {
  const cwd = record.workspace?.path;
  if (record.backend === "codex" || typeof cwd !== "string" || !cwd) {
    return null;
  }
  const direct = path.join(home, ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"), `${sessionId}.jsonl`);
  return fs.existsSync(direct) ? direct : null;
}

// Codex writes a thread's rollout at its first turn, so a run can name its
// thread before the rollout exists (a managed launch learns it when the
// thread starts). The rollout then lands in the folder of the day it is
// written: today. Codex names that folder in local time, so the UTC day and
// the days either side cover every zone.
const DAY_MS = 86_400_000;

function codexNewRollout(home, threadId, now) {
  const suffix = `-${threadId}.jsonl`;
  for (const offset of [0, -1, 1]) {
    const day = new Date(now + offset * DAY_MS);
    const dir = path.join(home, ".codex", "sessions", String(day.getUTCFullYear()), String(day.getUTCMonth() + 1).padStart(2, "0"), String(day.getUTCDate()).padStart(2, "0"));
    let names;
    try {
      names = fs.readdirSync(dir);
    } catch (error) {
      if (error?.code === "ENOENT") {
        continue; // no rollout that day
      }
      throw error;
    }
    const name = names.find((entry) => entry.startsWith("rollout-") && entry.endsWith(suffix));
    if (name) {
      return path.join(dir, name);
    }
  }
  return null;
}

// Each harness's session reader and its stats.
const READERS = { claude: claudeReader, codex: codexReader, antigravity: antigravityReader };
const STATS = { claude: claudeStats, codex: codexStats, antigravity: antigravityStats };

// ---------------------------------------------------------------------------
// Antigravity (agy) transcript.jsonl: one step per line,
//   {"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE",
//    "model":"…","input_tokens":…,"cache_read_tokens":…,"output_tokens":…,"content":"…"}
// USER_INPUT steps are prompts, PLANNER_RESPONSE steps the model's replies, and
// every other MODEL step a tool call (RUN_COMMAND, …).
// ---------------------------------------------------------------------------

function antigravityReader({ stats = null } = {}) {
  const reader = {
    messages: [],
    // As claudeReader's; a tool step carries its own result.
    onResult: null,
    addLine(line) {
      if (!line.trim()) {
        return;
      }
      let step;
      try {
        step = JSON.parse(line);
      } catch {
        return; // the line agy is still writing
      }
      stats?.add(step);
      const id = `s${step.step_index}`;
      const ts = toMillis(step.created_at);
      const content = typeof step.content === "string" ? step.content.trim() : "";
      if (step.type === "USER_INPUT") {
        if (content) {
          reader.messages.push({ id, role: "user", ts, text: content });
        }
      } else if (step.type === "PLANNER_RESPONSE") {
        if (content) {
          reader.messages.push({ id, role: "assistant", ts, text: content });
        }
      } else if (step.source === "MODEL" && typeof step.type === "string") {
        reader.messages.push({
          id,
          role: "tool",
          ts,
          tool: { name: step.type.toLowerCase(), input: oneLine(content), output: null, isError: step.status === "ERROR" },
        });
      }
    },
  };
  return reader;
}

// The model agy answered with last. agy names no context window and its steps
// carry no timing, so there is no speed or context to show.
export function antigravityStats() {
  let model = null;
  return {
    add(step) {
      if (step.type === "PLANNER_RESPONSE" && typeof step.model === "string" && step.model) {
        model = step.model;
      }
    },
    result() {
      return { model, speed: null, context: null };
    },
  };
}

// ---------------------------------------------------------------------------
// OpenCode: its database (opencode.mjs). A page is a run of the session's
// messages, and a cursor's offset is the index of the page's first message.
// ---------------------------------------------------------------------------

// The session the run is on, when OpenCode has it.
function opencodeConversation(record, home) {
  const id = record.session?.nativeSessionId;
  return isOpencodeSessionId(id) && opencodeSessionExists(home, id) ? id : null;
}

function opencodeTranscript(record, { home, limit, before, after }) {
  const conversation = opencodeConversation(record, home);
  const end = before === null ? null : readCursor(before);
  if (end && end.conversation !== conversation) {
    throw new Error("This chat started a new conversation since it opened. Reopen the chat to see it.");
  }
  const from = after === null ? null : readCursor(after);
  const start = from && from.conversation === conversation ? from.offset : null;
  const base = { runId: record.id, conversation, earlier: null, messages: [] };
  if (!end) {
    base.title = (record.taskSummary || record.task || record.id || "").toString();
    base.backend = "opencode";
    base.status = record.status || null;
  }
  const page = conversation ? opencodeSessionPage(record, home, { end: end?.offset ?? null, start, limit, stats: !end }) : null;
  if (!page) {
    base.messages = showAttachments(recordedTurns(record).map(showHandoff));
    return base;
  }
  attachOpencodeSubagents(page.spawns, { home, parentId: conversation, live: subagentsLive(record), depth: 1 });
  base.messages = showAttachments(showUserTask(page.messages, record));
  base.earlier = page.start > 0 ? `${conversation}:${page.start}:` : null;
  if (end) {
    return base;
  }
  // An OpenCode chat is not edited from an earlier turn (the dashboard's
  // chat edit forks Claude and Codex conversations only).
  base.edits = [];
  base.suggestion = promptSuggestion(home, record.id);
  base.stats = page.stats;
  base.context = page.context;
  return base;
}

/**
 * The chat messages of a page of the run's OpenCode session, the parts they
 * came from as JSON lines (`lines`, for a handoff's changed files) and, with
 * `stats`, the speed and context numbers. Null when OpenCode lacks the session.
 */
function opencodeSessionPage(record, home, { end, start, limit, stats }) {
  const id = opencodeConversation(record, home);
  const rows = id ? opencodeMessages(home, id, { start, end, limit }) : null;
  if (!rows) {
    return null;
  }
  const { messages, lines, spawns } = opencodeChat(rows.messages);
  const page = { messages, lines, spawns, start: rows.start, stats: null, context: null };
  if (stats) {
    // The numbers are extra: a model list that cannot be read says so and
    // the conversation still shows.
    try {
      Object.assign(page, opencodeNumbers(rows.messages, home));
    } catch (error) {
      const message = { error: error instanceof Error ? error.message : String(error) };
      Object.assign(page, { stats: message, context: message });
    }
  }
  return page;
}

// OpenCode's rows as chat messages: a user's text, the agent's text and
// tool calls (each with its result), a compaction's marker with its summary,
// and a finished turn's duration.
function opencodeChat(rows, outputLimit = TOOL_OUTPUT_LIMIT) {
  const messages = [];
  const lines = [];
  const spawns = [];
  let turn = null; // { at: the prompt's time, last: the turn's newest reply }
  const endTurn = () => {
    const done = turn?.last?.time?.completed;
    if (done && turn.last.finish && turn.last.finish !== "tool-calls" && !turn.last.error) {
      messages.push(turnMessage(`${turn.id}:turn`, done, done - turn.at));
    }
    turn = null;
  };
  for (const row of rows) {
    const data = row.data ?? {};
    const ts = toMillis(data.time?.created);
    if (data.role === "user") {
      endTurn();
      const compaction = row.parts.find((part) => part.type === "compaction");
      if (compaction) {
        messages.push(compactMessage(row.id, ts, { trigger: compaction.auto ? "auto" : "manual" }));
        continue;
      }
      const text = row.parts
        .filter((part) => part.type === "text" && !part.synthetic)
        .map((part) => String(part.text || ""))
        .join("\n")
        .trim();
      const command = row.parts.find((part) => part.metadata?.rudderSlashCommand)?.metadata.rudderSlashCommand
        ?? row.parts.find((part) => part.type === "subtask")?.command;
      const slash = typeof command === "string" && /^[^\s/]+$/.test(command) ? `/${command}` : null;
      if (text || slash) {
        messages.push({ id: row.id, role: "user", ts, text: text || slash, ...(slash ? { command: slash } : {}) });
      }
      turn = { id: row.id, at: ts };
      continue;
    }
    if (data.role !== "assistant") {
      continue;
    }
    if (data.summary === true) {
      // The summary OpenCode carries on from belongs to its marker.
      const marker = [...messages].reverse().find((message) => message.kind === "compact");
      if (marker) {
        marker.compact.summary = row.parts.filter((part) => part.type === "text").map((part) => part.text).join("\n").trim();
      }
      continue;
    }
    if (turn) {
      turn.last = data;
    }
    row.parts.forEach((part, index) => {
      const id = `${row.id}:${index}`;
      const at = toMillis(part.time?.start) ?? ts;
      if (part.type === "text" && !part.synthetic) {
        const text = String(part.text || "").trim();
        if (text) {
          messages.push({ id, role: "assistant", ts: at, text });
        }
      } else if (part.type === "tool") {
        const state = part.state ?? {};
        lines.push(JSON.stringify(part));
        const tool = {
          name: String(part.tool || "tool"),
          input: summarizeOpencodeInput(state.input),
          output: state.status === "completed" ? clip(state.output, outputLimit) : state.status === "error" ? clip(state.error, outputLimit) : null,
          isError: state.status === "error",
        };
        setToolFiles(tool, opencodeFiles(part, { text: state.output, limit: outputLimit }));
        const message = { id: part.callID || id, role: "tool", ts: toMillis(state.time?.start) ?? ts, text: "", tool };
        messages.push(message);
        const childId = state.metadata?.sessionId;
        if (part.tool === "task" && isOpencodeSessionId(childId)) {
          spawns.push({ message, childId });
        }
      }
    });
  }
  if (turn?.last) {
    endTurn();
  }
  return { messages, lines, spawns };
}

function summarizeOpencodeInput(input) {
  if (!input || typeof input !== "object") {
    return "";
  }
  const pick = input.command ?? input.filePath ?? input.path ?? input.pattern ?? input.url ?? input.query ?? input.description ?? input.prompt ?? null;
  if (typeof pick === "string") {
    return oneLine(pick);
  }
  if (Array.isArray(input.todos)) {
    return `${input.todos.length} todo(s)`;
  }
  return oneLine(JSON.stringify(input));
}

export function opencodeSubagents(home, parentId, live) {
  return opencodeChildSessions(home, parentId).map(({ id, title, agent, time_created, time_updated, turn, tool_calls, has_tool_calls, step }) => {
    // Background tasks outlive the parent call. The child's own loop also stays
    // active after unknown finishes and after providers say stop with tool calls.
    const endedAt = turn?.role === "assistant" ? toMillis(turn.time?.completed) : null;
    const finished = turn?.finish && !["tool-calls", "unknown"].includes(turn.finish) && !has_tool_calls;
    const terminal = endedAt !== null && (turn.error || finished);
    const status = terminal
      ? turn.error?.name === "MessageAbortedError" ? "stopped" : turn.error ? "failed" : "done"
      : live ? "running" : "stopped";
    const detail = step?.type === "tool" ? `${step.tool} ${summarizeOpencodeInput(step.state?.input)}` : step?.text;
    return {
      id,
      backend: "opencode",
      type: "subagent",
      kind: agent ?? null,
      name: null,
      description: title.replace(/ \(@[^)]+ subagent\)$/, ""),
      status,
      startedAt: time_created,
      endedAt: status === "running" ? null : terminal ? endedAt : Math.max(time_updated, toMillis(turn?.time?.created) ?? 0, endedAt ?? 0),
      toolCalls: tool_calls,
      step: oneLine(turn?.error?.data?.message || detail) || null,
    };
  });
}

export function opencodeSubagentActivity(entry, { home, live, depth = 1 }) {
  const page = opencodeMessages(home, entry.id, { start: null, end: null, limit: SUBAGENT_KEEP });
  if (!page) {
    return { ...entry, prompt: null, result: null, ...shownMessages(null) };
  }
  const { messages, spawns } = opencodeChat(page.messages, SUBAGENT_OUTPUT_LIMIT);
  if (depth < SUBAGENT_DEPTH) {
    attachOpencodeSubagents(spawns, { home, parentId: entry.id, live, depth: depth + 1 });
  }
  const prompt = page.start ? opencodeFirstPrompt(home, entry.id) : messages.find((message) => message.role === "user")?.text;
  const reply = page.messages.findLast((message) => message.data.role === "assistant" && message.data.summary !== true);
  const result = entry.status === "done"
    ? reply?.parts.filter((part) => part.type === "text" && !part.synthetic).map((part) => part.text).join("\n").trim()
    : entry.status === "failed" ? reply?.data.error?.data?.message : null;
  return {
    ...entry,
    prompt: prompt ? clip(prompt, SUBAGENT_PROMPT_LIMIT) : null,
    result: result ? clip(result, SUBAGENT_OUTPUT_LIMIT) : null,
    ...shownMessages({ messages, dropped: page.start }),
  };
}

function attachOpencodeSubagents(spawns, { home, parentId, live, depth }) {
  if (spawns.length === 0) {
    return;
  }
  const children = new Map(opencodeSubagents(home, parentId, live).map((entry) => [entry.id, entry]));
  const cards = new Map();
  for (const { message, childId } of spawns) {
    const entry = children.get(childId);
    if (entry) {
      if (!cards.has(childId)) {
        cards.set(childId, opencodeSubagentActivity(entry, { home, live, depth }));
      }
      message.subagent = cards.get(childId);
    }
  }
}

// The speed of the page's replies and how full the context is after the
// newest: its input, cache and output tokens against the model's window.
function opencodeNumbers(rows, home) {
  const responses = [];
  let last = null;
  for (const row of rows) {
    const data = row.data ?? {};
    if (data.role !== "assistant" || data.summary === true) {
      continue;
    }
    const start = toMillis(data.time?.created);
    const end = toMillis(data.time?.completed);
    const tokens = Number(data.tokens?.output) + Number(data.tokens?.reasoning || 0);
    if (start !== null && end !== null) {
      keepResponse(responses, { start, end, tokens: Number.isFinite(tokens) ? tokens : 0, done: true });
    }
    if (data.tokens && end !== null) {
      last = data;
    }
  }
  const model = last?.providerID && last?.modelID ? `${last.providerID}/${last.modelID}` : null;
  if (!last) {
    return { stats: { model, speed: speedOf(responses), context: null }, context: null };
  }
  const t = last.tokens;
  const used = [t.input, t.output, t.reasoning, t.cache?.read, t.cache?.write].reduce((sum, value) => sum + (Number(value) || 0), 0);
  const window = opencodeContextWindow(home, last.providerID, last.modelID);
  const at = toMillis(last.time.completed);
  return {
    stats: { model, speed: speedOf(responses), context: { used, window, at, modelWindow: window, autoCompactWindow: null, compactedAt: null } },
    context: { used, window, autoCompactAt: null, source: null, at },
  };
}

// The chat list's preview line: the agent's newest text, read again at most
// every PREVIEW_REFRESH_MS.
function opencodePreview(record, { home, cache, now }) {
  const id = record.session?.nativeSessionId;
  if (!isOpencodeSessionId(id)) {
    return null;
  }
  const key = `${opencodeDb(home)}#${id}`;
  if (cache && cache.file === key && now - (cache.readAt ?? 0) < PREVIEW_REFRESH_MS) {
    return { text: cache.text, remoteUrl: null, cacheEntry: cache };
  }
  const last = opencodeLastText(home, id);
  if (last === null) {
    return null;
  }
  const text = last.replace(/\s+/g, " ").trim().slice(0, 280) || null;
  const entry = { file: key, readAt: now, text, remoteUrl: null };
  return { text, remoteUrl: null, cacheEntry: entry };
}

// ---------------------------------------------------------------------------
// Claude Code JSONL.
// ---------------------------------------------------------------------------

// The tool Claude Code starts a subagent with.
const CLAUDE_SUBAGENT_TOOL = "Agent";

// A Claude Code session read entry by entry. The parent's own file skips
// sidechain entries; a subagent's file is nothing but those, and a teammate's
// file is its own session. Either one (`sub`) is shown from its first user
// turn on, which repeats the prompt from the parent's Agent call, so it does
// not show again; neither does the copy of that call a fork's file starts
// with (`self`).
function claudeReader({ stats = null, sub = false, self = null } = {}) {
  const tools = new Map(); // tool_use id -> message
  const inputs = new Map(); // tool_use id -> the call's input, for the files its result names
  const outputLimit = sub ? SUBAGENT_OUTPUT_LIMIT : TOOL_OUTPUT_LIMIT;
  const reader = {
    messages: [],
    // Agent tool_use id -> { message, ts, prompt, description, isError, teammate }
    spawns: new Map(),
    // Agent tool_use id -> the status of its newest <task-notification>
    notifications: new Map(),
    toolCalls: 0,
    // A subagent's or teammate's task (its first user turn) and when it came.
    prompt: null,
    firstAt: null,
    lastAt: null,
    // A teammate's turn ended and it waits for its next message, or its
    // process exited.
    idle: false,
    exited: false,
    // A teammate agreed to shut down: its work is over, its process exits next.
    shutDown: false,
    dropped: 0,
    // Told of each tool result with its call's message (null when the call
    // is not in what was read), for where a chat page may start.
    onResult: null,
    // `offset`: the line's byte offset in the session, which names messages
    // that have no id of their own.
    addLine(line, offset) {
      if (!line.trim()) {
        return;
      }
      let entry;
      try {
        entry = JSON.parse(line);
      } catch {
        return;
      }
      add(entry, offset);
      if (sub && reader.messages.length > SUBAGENT_KEEP * 2) {
        const extra = reader.messages.length - SUBAGENT_KEEP;
        reader.messages.splice(0, extra);
        reader.dropped += extra;
      }
    },
    // Lines a subagent's file has that the chat never shows (raw API requests,
    // most of its bytes), passed over without parsing.
    skip(prefix) {
      return prefix.startsWith('{"type":"api-request');
    },
  };
  const messages = reader.messages;
  const push = (message) => messages.push(message);
  let promptSeen = false;
  function add(entry, offset) {
    stats?.add(entry);
    // Claude Code writes a compaction once it ends (2.1.328): this boundary
    // with what set it off and the context's size before and after, then the
    // summary it carries on from.
    if (entry.type === "system" && entry.subtype === "compact_boundary") {
      const meta = entry.compactMetadata || {};
      push(compactMessage(entry.uuid || `compact-${offset}`, toMillis(entry.timestamp), {
        trigger: typeof meta.trigger === "string" ? meta.trigger : null,
        preTokens: Number(meta.preTokens) || null,
        postTokens: Number(meta.postTokens) || null,
        durationMs: Number(meta.durationMs) || null,
      }));
      return;
    }
    const own = sub || !entry.isSidechain;
    if (sub && entry.type === "system" && entry.subtype === "turn_duration") {
      reader.idle = true;
      reader.lastAt = toMillis(entry.timestamp) ?? reader.lastAt;
      return;
    }
    // Claude Code writes how long each turn took once it ends.
    if (entry.type === "system" && entry.subtype === "turn_duration") {
      if (own && Number(entry.durationMs) > 0) {
        push(turnMessage(entry.uuid || `turn-${offset}`, toMillis(entry.timestamp), Number(entry.durationMs)));
      }
      return;
    }
    // A scheduled task (a cron or /loop prompt) fired. Claude Code runs its
    // prompt as a hidden (isMeta) user turn: the chat marks the turn, with
    // the prompt behind the marker, instead of showing it as the user's words.
    if (entry.type === "system" && entry.subtype === "scheduled_task_fire") {
      if (own) {
        push({ id: entry.uuid || `scheduled-${offset}`, role: "system", kind: "scheduled", ts: toMillis(entry.timestamp), text: "Scheduled check", scheduled: { prompt: String(entry.prompt || "") } });
      }
      return;
    }
    // Claude Code saves a session's cost-state as its process exits.
    if (sub && entry.type === "cost-state") {
      reader.exited = true;
      return;
    }
    // A message typed while the agent works: Claude Code hands it to the running
    // turn as a queued_command attachment, never as a user entry.
    const queued = entry.type === "attachment" && own ? entry.attachment : null;
    if (queued?.type === "queued_command") {
      const prompt = Array.isArray(queued.prompt)
        ? queued.prompt.filter((part) => part?.type === "text").map((part) => part.text).join("\n")
        : queued.prompt;
      noteTaskNotification(prompt);
      const peers = peerMessages(prompt, queued.origin);
      if (peers) {
        pushPeers(entry.uuid, toMillis(entry.timestamp), peers);
      } else if (queued.commandMode === "prompt" && queued.origin?.kind === "human") {
        const text = cleanUserText(prompt);
        if (text) {
          push({ id: entry.uuid, role: "user", ts: toMillis(entry.timestamp), text });
        }
      }
      return;
    }
    // A slash command, typed in the chat or the terminal. Claude Code records
    // the command line (<command-name>, <command-args>) and then what it
    // printed (<local-command-stdout>, its parent) as user entries, or as
    // local_command system entries for some. The model does not answer it, so
    // the chat shows a note reading what it printed ("Goal set: ..."), or the
    // command line when it printed nothing. The note names the command, which
    // settles the message the chat sent (cloud/app/pending.js). /compact is
    // recorded after the compaction it ran, whose marker is its note.
    const local = own && !entry.isMeta && (entry.type === "user" || entry.subtype === "local_command")
      ? localCommand(entry.type === "user" ? entry.message?.content : entry.content)
      : null;
    if (local) {
      const last = messages[messages.length - 1];
      if (local.output !== undefined) {
        if (local.output && last?.kind === "command" && last.id === entry.parentUuid) {
          last.text = local.output;
        }
      } else if (!(local.name === "/compact" && last?.kind === "compact")) {
        push({ id: entry.uuid || `command-${offset}`, role: "system", kind: "command", ts: toMillis(entry.timestamp), text: local.line, command: local.name });
      }
      return;
    }
    // The goal /goal set (chat-goal.mjs). While background work holds off its
    // evaluation, Claude Code checks in on it: a task notification that wakes
    // the agent, or a hidden turn at the end of one. Each evaluation is a
    // goal_status attachment with the evaluator's reason. Each is a note where
    // the agent read it, so a turn it started has a reason in the chat. A goal
    // set, cleared or carried over a compaction (sentinel) has no note of its
    // own: the /goal command's note is that.
    const goal = own ? claudeGoalNote(entry, offset) : null;
    if (goal) {
      push(goal);
      return;
    }
    // Claude Code's own (isMeta) turns stay out of the chat, but for what
    // other agents send (peerMessages).
    if ((entry.type !== "user" && entry.type !== "assistant") || !own || (entry.isMeta && !PEER_ORIGINS.has(entry.origin?.kind))) {
      return;
    }
    const ts = toMillis(entry.timestamp);
    reader.idle = false;
    reader.exited = false;
    if (sub && ts !== null) {
      reader.firstAt ??= ts;
      reader.lastAt = ts;
    }
    if (entry.isCompactSummary) {
      // The summary Claude starts over from is not something the user said:
      // it belongs to the compaction marker just before it.
      const summary = compactSummaryText(entry.message?.content);
      const last = messages[messages.length - 1];
      if (last?.kind === "compact" && !last.compact.summary) {
        last.compact.summary = summary;
      } else {
        push(compactMessage(entry.uuid || `compact-${offset}`, ts, { summary }));
      }
      return;
    }
    const content = entry.message?.content;
    const uuid = entry.uuid || `${offset}`;
    const userText = (text) => {
      noteTaskNotification(text);
      const clean = cleanUserText(text);
      if (clean && sub && !promptSeen) {
        promptSeen = true;
        reader.prompt = peerMessages(clean, entry.origin)?.[0].body ?? clean;
        return "";
      }
      return clean;
    };
    // A user turn is the user's words, or what other agents sent.
    const say = (id, text) => {
      const peers = entry.type === "user" && text ? peerMessages(text, entry.origin) : null;
      if (peers) {
        pushPeers(id, ts, peers);
      } else if (text) {
        push({ id, role: entry.type === "user" ? "user" : "assistant", ts, text });
      }
    };
    if (typeof content === "string") {
      say(uuid, entry.type === "user" ? userText(content) : cleanUserText(content));
      return;
    }
    if (!Array.isArray(content)) {
      return;
    }
    content.forEach((block, index) => {
      const id = `${uuid}:${index}`;
      if (block?.type === "text") {
        say(id, entry.type === "user" ? userText(block.text) : String(block.text || "").trim());
      } else if (block?.type === "thinking" && entry.type === "assistant" && entry.thinkingDisplay === "updates") {
        // Claude Code's "updates" display stores the notes the agent writes
        // to the user between tool calls as thinking; its terminal shows them.
        const text = String(block.thinking || "").trim();
        if (text) {
          push({ id, role: "assistant", ts, text });
        }
      } else if (block?.type === "tool_use" && block.id !== self) {
        const message = {
          id: block.id || id,
          role: "tool",
          ts,
          text: "",
          tool: { name: block.name || "tool", input: summarizeClaudeInput(block.name, block.input), output: null, isError: false },
        };
        setToolFiles(message.tool, { files: claudeCallFiles(block.name, block.input) });
        tools.set(block.id, message);
        inputs.set(block.id, block.input);
        reader.toolCalls += 1;
        const reply = block.name === "SendMessage" ? block.input?.message : null;
        if (reply?.type === "shutdown_response" && reply.approve === true) {
          reader.shutDown = true;
        }
        if (block.name === CLAUDE_SUBAGENT_TOOL && block.id) {
          reader.spawns.set(block.id, {
            message,
            ts,
            prompt: typeof block.input?.prompt === "string" ? block.input.prompt : null,
            description: typeof block.input?.description === "string" ? block.input.description : null,
            isError: false,
            teammate: null,
          });
        }
        push(message);
      } else if (block?.type === "tool_result") {
        const target = tools.get(block.tool_use_id);
        reader.onResult?.(target ?? null);
        if (target) {
          const text = toolResultText(block.content);
          target.tool.output = clip(text, outputLimit);
          target.tool.isError = Boolean(block.is_error);
          setToolFiles(target.tool, claudeResultFiles(target.tool.name, inputs.get(block.tool_use_id), { isError: target.tool.isError, result: entry.toolUseResult, text, limit: outputLimit }));
          // A followed subagent's reader lives across polls: a call is let go
          // once its result is in, so a long subagent does not pile them up.
          tools.delete(block.tool_use_id);
          inputs.delete(block.tool_use_id);
        }
        const spawn = reader.spawns.get(block.tool_use_id);
        if (spawn) {
          spawn.isError = Boolean(block.is_error);
          // A named Agent call starts a teammate: a session of its own, which
          // the result names by team and name.
          const result = entry.toolUseResult;
          if (result?.status === "teammate_spawned" && typeof result.team_name === "string" && typeof result.name === "string") {
            spawn.teammate = { team: result.team_name, name: result.name, kind: typeof result.agent_type === "string" ? result.agent_type : null };
          }
        }
      }
    });
  }
  // Claude reports a background subagent's end to its parent as a
  // <task-notification> naming the Agent call and how it ended.
  function noteTaskNotification(text) {
    const value = String(text || "");
    if (!value.trimStart().startsWith("<task-notification>")) {
      return;
    }
    const id = xmlTag(value, "tool-use-id");
    const status = xmlTag(value, "status");
    if (id && status) {
      reader.notifications.set(id, status);
    }
  }
  // What other agents sent (peerMessages): a card each in the chat, never the user's bubble.
  function pushPeers(id, ts, peers) {
    peers.forEach(({ body, ...peer }, index) => {
      push({ id: peers.length > 1 ? `${id}:peer${index}` : id, role: "system", kind: "peer", ts, text: body, peer });
    });
  }
  return reader;
}

function xmlTag(text, tag) {
  const match = new RegExp(`<${tag}>([\\s\\S]*?)</${tag}>`).exec(text);
  return match ? match[1].trim() : null;
}

// A slash command's own entries (claudeReader): { name, line } for the
// command line, { output } for what it printed (its terminal colors
// removed), null for anything else.
const ANSI_SGR = /\x1b\[[0-9;]*m/g;

function localCommand(content) {
  const text = typeof content === "string" ? content.trim() : "";
  if (text.startsWith("<command-name>")) {
    const name = xmlTag(text, "command-name");
    const args = xmlTag(text, "command-args");
    return { name, line: args ? `${name} ${args}` : name };
  }
  if (text.startsWith("<local-command-stdout>")) {
    return { output: xmlTag(text, "local-command-stdout").replace(ANSI_SGR, "").trim() };
  }
  return null;
}

// A note about the goal (claudeReader), or null for any other entry.
function claudeGoalNote(entry, offset) {
  const id = entry.uuid || `goal-${offset}`;
  const ts = toMillis(entry.timestamp);
  const status = entry.type === "attachment" ? entry.attachment : null;
  if (status?.type === "goal_status") {
    if (status.sentinel === true) {
      return null;
    }
    return goalNote(id, ts, status.failed === true ? "Goal can't be met" : status.met === true ? "Goal met" : "Goal not met yet", status.reason);
  }
  const content = entry.type === "user" ? entry.message?.content : null;
  if (typeof content !== "string") {
    return null;
  }
  if (entry.queueOrigin?.source === "goal-checkin") {
    return goalNote(id, ts, "Goal check-in", xmlTag(content, "system-reminder"));
  }
  if (entry.isMeta && content.startsWith("Goal check-in: ")) {
    return goalNote(id, ts, "Goal check-in", content);
  }
  return null;
}

// `detail` is what the note opens to: the check-in or the evaluator's reason.
function goalNote(id, ts, text, detail) {
  return { id, role: "system", kind: "goal", ts, text, goal: { detail: typeof detail === "string" ? detail.trim() : "" } };
}

// What other agents send this one, as Claude Code hands it over: null for
// anything else, or one { from, summary, idle, failure, body } per message.
//  - A teammate's (agent teams), or the lead's to a teammate: a user entry of
//    <teammate-message teammate_id="<name>" color="green" summary="...">
//    blocks, one or more. A lead's session gets them after "Another Claude
//    session sent a message:" and before a note that a peer is not the user.
//    A teammate whose turn ended sends an idle_notification as JSON, with its
//    result or why it failed.
//  - A subagent's or another session's (<agent-message>,
//    <cross-session-message>): an isMeta entry or a queued_command whose
//    origin (kind "peer") names the sender and holds the body. A subagent's
//    final report comes without the hand-back preface Claude Code adds.
//  - The lead's to a subagent: origin kind "coordinator", the body between a
//    line saying the coordinator sent it and a closing instruction.
const PEER_ORIGINS = new Set(["peer", "coordinator"]);
const TEAMMATE_INTRO = /^Another Claude session sent a message(?: while you were working)?:\n/;
const TEAMMATE_BLOCK = /^<teammate-message teammate_id="([^"]*)"((?: [\w-]+="[^"]*")*)>\n?([\s\S]*?)\n?<\/teammate-message>(?:\n\n|$)/;
const TEAMMATE_NOTE = /^This came from another Claude session/;
const TEAMMATE_IDLE = /^\{"type":"idle_notification",/;
const COORDINATOR_INTRO = /^The coordinator sent a message while you were working:\n/;
const COORDINATOR_OUTRO = /\n+Address this before completing your current task\.$/;

function peerMessages(text, origin) {
  if (origin?.kind === "peer") {
    const body = origin.handback ? origin.handbackReportText : origin.body;
    return typeof origin.name === "string" && typeof body === "string" ? [{ from: origin.name, summary: null, idle: false, failure: null, body }] : null;
  }
  if (origin?.kind === "coordinator") {
    return [{ from: "the lead", summary: null, idle: false, failure: null, body: text.replace(COORDINATOR_INTRO, "").replace(COORDINATOR_OUTRO, "") }];
  }
  return origin ? null : teammateMessages(text);
}

function teammateMessages(text) {
  let rest = text.replace(TEAMMATE_INTRO, "");
  const wrapped = rest.length < text.length;
  const found = [];
  for (let match; (match = TEAMMATE_BLOCK.exec(rest)); rest = rest.slice(match[0].length)) {
    const summary = /(?:^| )summary="([^"]*)"/.exec(match[2]);
    const message = { from: xmlUnescape(match[1]), summary: summary ? xmlUnescape(summary[1]) : null, idle: false, failure: null, body: match[3] };
    if (TEAMMATE_IDLE.test(message.body)) {
      const note = JSON.parse(message.body);
      message.idle = true;
      message.failure = typeof note.failureReason === "string" ? note.failureReason : null;
      message.body = typeof note.result === "string" ? note.result : "";
    }
    found.push(message);
  }
  return found.length && (!rest || (wrapped && TEAMMATE_NOTE.test(rest))) ? found : null;
}

function xmlUnescape(value) {
  return value.replace(/&quot;/g, '"').replace(/&apos;/g, "'").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
}

// When an agent's context fills up, Claude Code and Codex summarize the
// conversation and carry on from the summary. The chat shows that as one
// marker (with the summary behind it), not as a wall of text from the user.
// `trigger` is "auto" or "manual", the token counts are the context's size
// before and after, and `durationMs` how long it took; each is null when the
// backend does not say.
function compactMessage(id, ts, { trigger = null, preTokens = null, postTokens = null, durationMs = null, summary = "" } = {}) {
  return { id, role: "system", kind: "compact", ts, text: "Context compacted", compact: { trigger, preTokens, postTokens, durationMs, summary } };
}

// A finished turn and how long it took, which the chat shows under the turn's
// last message ("Worked for 1m 05s", as the terminals print it).
function turnMessage(id, ts, durationMs) {
  return { id, role: "system", kind: "turn", ts, text: "", durationMs };
}

const COMPACT_PREAMBLE = /^This session is being continued from a previous conversation that ran out of context\.[^\n]*\n+(?:Summary:\s*\n)?/;
const COMPACT_TRAILER = /\n+(?:If you need specific details from before compaction|Continue the conversation from where it left off|Please continue the conversation from where we left)[\s\S]*$/;

export function compactSummaryText(content) {
  const raw = typeof content === "string"
    ? content
    : Array.isArray(content) ? content.map((part) => (part?.type === "text" ? part.text : "")).join("\n") : "";
  return String(raw || "").trim().replace(COMPACT_PREAMBLE, "").replace(COMPACT_TRAILER, "").trim();
}

function cleanUserText(text) {
  const value = String(text || "").trim();
  if (!value || CLAUDE_HARNESS_TEXT.test(value)) {
    return "";
  }
  return unwrapPastedContent(value.replace(/<system-reminder>[\s\S]*?<\/system-reminder>/g, "")).trim();
}

function toolResultText(content) {
  if (typeof content === "string") {
    return content;
  }
  if (Array.isArray(content)) {
    return content
      .map((part) => (part?.type === "text" ? part.text : part?.type === "image" ? "[image]" : ""))
      .filter(Boolean)
      .join("\n");
  }
  return "";
}

function summarizeClaudeInput(name, input) {
  if (!input || typeof input !== "object") {
    return "";
  }
  const pick =
    input.command ??
    input.file_path ??
    input.path ??
    input.pattern ??
    input.url ??
    input.query ??
    input.description ??
    input.prompt ??
    input.skill ??
    null;
  if (typeof pick === "string") {
    return oneLine(name === "Grep" && input.path ? `${pick}  in ${input.path}` : pick);
  }
  if (name === "TodoWrite" && Array.isArray(input.todos)) {
    return `${input.todos.length} todo(s)`;
  }
  return oneLine(JSON.stringify(input));
}

// ---------------------------------------------------------------------------
// Codex rollout JSONL.
// ---------------------------------------------------------------------------

// A Codex rollout read entry by entry: the agent's own, or a subagent's
// (`child`). A subagent's rollout opens with its session_meta (who spawned it,
// its `/root/<task>` path and nickname) and may carry its parent's history
// before `subagent_history_start_ordinal`, which is not its own.
function codexReader({ stats = null, child = false } = {}) {
  const calls = new Map(); // call_id -> message
  const outputLimit = child ? SUBAGENT_OUTPUT_LIMIT : TOOL_OUTPUT_LIMIT;
  const reader = {
    messages: [],
    // spawn_agent call_id -> { message, ts, taskName, threadId, error }
    spawns: new Map(),
    meta: null,
    start: 0,
    // The newest turn of a subagent: "running", "done" or "aborted".
    turn: null,
    result: null,
    endedAt: null,
    toolCalls: 0,
    dropped: 0,
    // As claudeReader's.
    onResult: null,
    // `offset`: the line's byte offset in the conversation, which names
    // messages that have no id of their own.
    addLine(line, offset) {
      if (!line.trim()) {
        return;
      }
      let entry;
      try {
        entry = JSON.parse(line);
      } catch {
        return;
      }
      add(entry, offset);
      if (child && reader.messages.length > SUBAGENT_KEEP * 2) {
        const extra = reader.messages.length - SUBAGENT_KEEP;
        reader.messages.splice(0, extra);
        reader.dropped += extra;
      }
    },
    // The inherited history (megabytes of it, compacted summaries included)
    // is passed over by its ordinal without parsing.
    skip(prefix) {
      if (!child || !reader.meta) {
        return false;
      }
      const match = /"ordinal":(\d+)/.exec(prefix);
      return match !== null && Number(match[1]) < reader.start;
    },
  };
  const messages = reader.messages;
  // Context in use after the newest response (`token_count`), and the newest
  // compaction's marker while its size after and its duration are still to come.
  let contextTokens = null;
  let compaction = null;
  function add(entry, offset) {
    if (child) {
      if (entry.type === "session_meta") {
        if (!reader.meta) {
          reader.meta = entry.payload || {};
          reader.start = Number(reader.meta.subagent_history_start_ordinal) || 0;
        }
        return;
      }
      if (Number(entry.ordinal) < reader.start) {
        return;
      }
    }
    stats?.add(entry);
    const id = `c${offset}`;
    const ts = toMillis(entry.timestamp);
    // A compaction (0.170): `compacted` with the history it kept (its summary
    // is encrypted, so `message` is empty; older versions wrote the summary
    // there), then the first `token_count` of the smaller context, then the
    // ContextCompaction item with when it started and ended. The size before
    // is the `token_count` that came before it.
    if (entry.type === "compacted") {
      compaction = compactMessage(id, ts, { preTokens: contextTokens, summary: String(entry.payload?.message || "").trim() });
      messages.push(compaction);
      return;
    }
    if (entry.type === "event_msg" && entry.payload?.type === "token_count") {
      const used = Number(entry.payload.info?.last_token_usage?.total_tokens);
      if (Number.isFinite(used) && used > 0) {
        contextTokens = used;
        if (compaction && compaction.compact.postTokens === null) {
          compaction.compact.postTokens = used;
        }
      }
      return;
    }
    if (entry.type === "event_msg" && entry.payload?.type === "item_completed") {
      const item = entry.payload.item;
      if (item?.type === "ContextCompaction") {
        const took = Number(entry.payload.completed_at_ms) - Number(entry.payload.started_at_ms);
        if (compaction && compaction.compact.durationMs === null && took > 0) {
          compaction.compact.durationMs = took;
        }
        return;
      }
      // A patch's changed files, while the call that applied it (apply_patch,
      // or a code-mode script) is open.
      if (item?.type === "FileChange") {
        const call = [...calls.values()].findLast((m) => m.tool.name === "exec" || m.tool.name === "apply_patch");
        if (call) setToolFiles(call.tool, { files: withFiles(call.tool.files ?? [], codexChangedFiles(item)) });
        return;
      }
      // The thread a spawn_agent call started.
      const spawn = item?.type === "SubAgentActivity" && item.kind === "started" ? reader.spawns.get(item.id) : null;
      if (spawn && typeof item.agent_thread_id === "string" && /^[A-Za-z0-9-]+$/.test(item.agent_thread_id)) {
        spawn.threadId = item.agent_thread_id;
      }
      return;
    }
    if (entry.type === "event_msg" && child) {
      const type = entry.payload?.type;
      if (type === "task_started") {
        reader.turn = "running";
        reader.endedAt = null;
      } else if (type === "task_complete") {
        reader.turn = "done";
        reader.endedAt = ts;
        reader.result = typeof entry.payload.last_agent_message === "string" ? entry.payload.last_agent_message.trim() || null : null;
      } else if (type === "turn_aborted") {
        reader.turn = "aborted";
        reader.endedAt = ts;
      }
      return;
    }
    // Codex ends a turn with task_complete, which says how long it took. A
    // stopped turn (turn_aborted) shows no time.
    if (entry.type === "event_msg" && entry.payload?.type === "task_complete") {
      if (Number(entry.payload.duration_ms) > 0) {
        messages.push(turnMessage(id, ts, Number(entry.payload.duration_ms)));
      }
      return;
    }
    if (entry.type !== "response_item") {
      return;
    }
    const payload = entry.payload || {};
    if (payload.type === "message") {
      const text = (payload.content || [])
        .map((part) => part?.text || "")
        .join("\n")
        .trim();
      if (!text) {
        return;
      }
      if (payload.role === "user") {
        // Codex (0.170) tells the model about the goal as the user: what
        // /goal changed (`user_goal`), and each turn it starts to keep at the
        // goal (`goal`, its standing instructions). The chat shows notes, like
        // Claude's (claudeReader): the change as /goal's note, which settles
        // the message the chat sent, and the new turn as a marker.
        const internal = CODEX_GOAL_CONTEXT.exec(text);
        if (internal?.[1] === "user_goal") {
          const said = internal[2].split("\n").map((line) => line.trim()).filter(Boolean).join("\n\n");
          messages.push({ id: payload.id || id, role: "system", kind: "command", ts, text: said, command: "/goal" });
        } else if (internal?.[1] === "goal") {
          messages.push({ id: payload.id || id, role: "system", kind: "goal", ts, text: "Continuing toward the goal", goal: { detail: "" } });
        } else if (codexTypedText(text)) {
          messages.push({ id: payload.id || id, role: "user", ts, text: codexQuestionReply(text) });
        }
      } else if (payload.role === "assistant") {
        messages.push({ id, role: "assistant", ts, text });
      }
    } else if (payload.type === "function_call" || payload.type === "custom_tool_call" || payload.type === "local_shell_call") {
      const message = {
        id: payload.call_id || id,
        role: "tool",
        ts,
        text: "",
        tool: { name: payload.name || payload.type, input: summarizeCodexInput(payload), output: null, isError: false },
      };
      if (payload.call_id) {
        calls.set(payload.call_id, message);
        if (payload.name === "spawn_agent") {
          reader.spawns.set(payload.call_id, { message, ts, taskName: codexArguments(payload).task_name ?? null, threadId: null, error: null });
        }
      }
      reader.toolCalls += 1;
      messages.push(message);
    } else if (payload.type === "function_call_output" || payload.type === "custom_tool_call_output" || payload.type === "local_shell_call_output") {
      const target = calls.get(payload.call_id);
      reader.onResult?.(target ?? null);
      if (target) {
        const { output, isError } = codexOutput(payload.output);
        target.tool.output = clip(output, outputLimit);
        target.tool.isError = isError;
        calls.delete(payload.call_id);
      }
      const spawn = reader.spawns.get(payload.call_id);
      if (spawn) {
        // `{"task_name":"/root/<name>"}` when it started; anything else is
        // why it did not.
        let spawned = null;
        try {
          spawned = JSON.parse(String(payload.output));
        } catch {
          spawned = null;
        }
        if (typeof spawned?.task_name !== "string") {
          spawn.error = codexOutput(payload.output).output || "The subagent did not start.";
        }
      }
    } else if (payload.type === "reasoning") {
      // Summaries only; raw reasoning is encrypted.
      const summary = (payload.summary || []).map((part) => part?.text || "").join("\n").trim();
      if (summary) {
        messages.push({ id, role: "system", ts, text: summary });
      }
    }
  }
  return reader;
}

const CODEX_GOAL_CONTEXT = /^<codex_internal_context source="(user_goal|goal)">\n?([\s\S]*?)\n?<\/codex_internal_context>$/;

// An answer to a `request_user_input_async` question comes back as a user
// message, `<send_user_message_question_reply>[{answer, question,
// questionItemId}]</…>`. The chat shows the answers as they were given.
function codexQuestionReply(text) {
  const match = /^<send_user_message_question_reply>([\s\S]*)<\/send_user_message_question_reply>$/.exec(text);
  if (!match) {
    return text;
  }
  const items = JSON.parse(match[1]);
  return items.map((item) => String(item.answer).trim()).join("\n");
}

function codexArguments(payload) {
  try {
    const value = JSON.parse(String(payload.arguments ?? ""));
    return value && typeof value === "object" ? value : {};
  } catch {
    return {};
  }
}

// Codex tool output is a string, a JSON string ({output, metadata}), or an
// array of content parts ([{type:"input_text", text}]).
function codexOutput(value) {
  let parsed = value;
  if (typeof value === "string") {
    try {
      parsed = JSON.parse(value);
    } catch {
      return { output: value, isError: false };
    }
  }
  if (Array.isArray(parsed)) {
    return { output: parsed.map((part) => part?.text ?? "").filter(Boolean).join("\n"), isError: false };
  }
  if (parsed && typeof parsed === "object") {
    if (typeof parsed.output === "string") {
      return { output: parsed.output, isError: Number(parsed?.metadata?.exit_code ?? 0) !== 0 };
    }
    return { output: JSON.stringify(parsed, null, 2), isError: false };
  }
  return { output: String(parsed ?? ""), isError: false };
}

function summarizeCodexInput(payload) {
  if (payload.name === "spawn_agent" || payload.name === "send_message" || payload.name === "followup_task") {
    // The message is encrypted; the task name or the agent it goes to is
    // what can be shown.
    const args = codexArguments(payload);
    const who = args.task_name ?? args.target;
    if (typeof who === "string") {
      return oneLine(who);
    }
  }
  const raw = payload.arguments ?? payload.input ?? payload.action ?? "";
  // Code-mode Codex runs tools from a script: `tools.exec_command({cmd:"..."})`.
  if (typeof raw === "string" && /tools\.\w+\(/.test(raw)) {
    const cmd = /\bcmd\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`[^`]*`)/.exec(raw);
    if (cmd) {
      try {
        return oneLine(cmd[1].startsWith('"') ? JSON.parse(cmd[1]) : cmd[1].slice(1, -1));
      } catch {
        return oneLine(cmd[1].slice(1, -1));
      }
    }
    const call = /tools\.(\w+)\(/.exec(raw);
    return oneLine(call ? call[1] : raw);
  }
  let value = raw;
  if (typeof raw === "string") {
    try {
      value = JSON.parse(raw);
    } catch {
      return oneLine(raw);
    }
  }
  if (value && typeof value === "object") {
    const command = value.cmd ?? value.command;
    if (Array.isArray(command)) {
      const parts = command[0] === "bash" && command[1] === "-lc" ? command.slice(2) : command;
      return oneLine(parts.join(" "));
    }
    if (typeof command === "string") {
      return oneLine(command);
    }
    return oneLine(JSON.stringify(value));
  }
  return oneLine(String(value));
}

// ---------------------------------------------------------------------------
// Subagents. Claude Code's Agent tool and Codex's spawn_agent start another
// agent whose own log sits next to the parent's. Each spawn call in the chat
// carries a `subagent` with what the app's card shows (cloud/app/subagents.js):
//   { id, backend, kind, name, description, prompt, status, startedAt,
//     endedAt, toolCalls, result, messages, earlier }
// status is running, done, failed or stopped. A subagent's log is followed:
// each poll reads only what was appended since the last one.
// ---------------------------------------------------------------------------

// Claude Code 2.1 writes each subagent to <session>/subagents/agent-<id>.jsonl
// with agent-<id>.meta.json beside it: the Agent call's toolUseId, its
// description and agentType, and `finished` once it ended. Subagents of
// subagents (parentAgentId) are in the same folder. A teammate (a named Agent
// call in an agent team) has no metadata: it is a session of its own in the
// same project folder (`projectDir`), done once a turn of its ends and
// stopped when its process exits mid-turn.
function attachClaudeSubagents(reader, { dir, projectDir, live, shown, depth }) {
  if (!reader.spawns.size) {
    return;
  }
  const metas = claudeSubagentMetas(dir);
  for (const [toolUseId, spawn] of reader.spawns) {
    if (shown && !shown.has(spawn.message)) {
      continue;
    }
    const found = metas.get(toolUseId);
    let card;
    if (found) {
      const sub = followFile(path.join(dir, `agent-${found.agentId}.jsonl`), () => claudeReader({ sub: true, self: toolUseId }));
      if (sub && depth + 1 < SUBAGENT_DEPTH) {
        attachClaudeSubagents(sub, { dir, projectDir, live, shown: null, depth: depth + 1 });
      }
      const meta = found.meta;
      const note = reader.notifications.get(toolUseId);
      const status = !meta.finished
        ? live && !claudeInterrupted(sub) ? "running" : "stopped"
        : spawn.isError || note === "failed" ? "failed" : note && note !== "completed" ? "stopped" : "done";
      card = {
        id: found.agentId,
        kind: typeof meta.agentType === "string" ? meta.agentType : null,
        name: typeof meta.name === "string" ? meta.name : null,
        description: typeof meta.description === "string" ? meta.description : null,
        sub,
        status,
      };
    } else if (spawn.teammate) {
      const session = claudeTeammateFile(projectDir, toolUseId, spawn);
      if (!session) {
        continue; // not started yet
      }
      const sub = followFile(session, () => claudeReader({ sub: true }));
      if (sub && depth + 1 < SUBAGENT_DEPTH) {
        attachClaudeSubagents(sub, { dir: claudeSubagentsDir(session), projectDir, live, shown: null, depth: depth + 1 });
      }
      card = {
        id: path.basename(session, ".jsonl"),
        kind: spawn.teammate.kind,
        name: spawn.teammate.name,
        description: spawn.description,
        sub,
        status: sub?.idle || sub?.shutDown ? "done" : live && !sub?.exited && !claudeInterrupted(sub) ? "running" : "stopped",
      };
    } else {
      continue; // its metadata is not written yet
    }
    const { sub, status } = card;
    const messages = sub?.messages ?? [];
    const last = messages[messages.length - 1];
    spawn.message.subagent = {
      id: card.id,
      backend: "claude",
      kind: card.kind,
      name: card.name,
      description: card.description,
      prompt: spawn.prompt ? clip(spawn.prompt, SUBAGENT_PROMPT_LIMIT) : null,
      status,
      startedAt: spawn.ts,
      endedAt: status === "running" ? null : sub?.lastAt ?? null,
      toolCalls: sub?.toolCalls ?? 0,
      result: status !== "running" && last?.role === "assistant" ? last.text : null,
      ...shownMessages(sub),
    };
  }
}

const claudeMetas = new Map(); // meta file -> { mtimeMs, size, value }

function claudeSubagentMetas(dir) {
  const metas = new Map(); // toolUseId -> { agentId, meta }
  let names;
  try {
    names = fs.readdirSync(dir);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return metas;
    }
    throw error;
  }
  for (const name of names) {
    const match = /^agent-([A-Za-z0-9_-]+)\.meta\.json$/.exec(name);
    if (!match) {
      continue;
    }
    const meta = readSubagentMeta(path.join(dir, name));
    if (typeof meta?.toolUseId === "string") {
      metas.set(meta.toolUseId, { agentId: match[1], meta });
    }
  }
  return metas;
}

function readSubagentMeta(file) {
  const stat = fs.statSync(file);
  const hit = claudeMetas.get(file);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) {
    return hit.value;
  }
  let value;
  try {
    value = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    // Claude Code rewrites the file in place when the subagent ends; a read
    // that lands mid-write is picked up whole on the next poll.
    if (error instanceof SyntaxError) {
      return null;
    }
    throw error;
  }
  claudeMetas.set(file, { mtimeMs: stat.mtimeMs, size: stat.size, value });
  return value;
}

function claudeSubagentsDir(session) {
  return path.join(session.replace(/\.jsonl$/, ""), "subagents");
}

// A subagent stopped from outside (TaskStop, Esc in its pane) ends on Claude's
// interrupt note and is never marked finished.
function claudeInterrupted(reader) {
  const last = reader?.messages[reader.messages.length - 1];
  return last?.role === "user" && last.text.startsWith("[Request interrupted by user");
}

// Teammate spawn -> its session log, found once. A miss (its session is not
// written yet) is searched again at most every CHILD_RECHECK_MS.
const CHILD_RECHECK_MS = 5_000;
const claudeTeammates = new Map(); // `${projectDir}|${toolUseId}` -> { file, checkedAt }

function claudeTeammateFile(projectDir, toolUseId, spawn, now = Date.now()) {
  const key = `${projectDir}|${toolUseId}`;
  const known = claudeTeammates.get(key);
  if (known?.file) {
    return known.file;
  }
  if (known && now - known.checkedAt < CHILD_RECHECK_MS) {
    return null;
  }
  const file = spawn.ts === null || spawn.prompt === null ? null : findClaudeTeammate(projectDir, spawn);
  claudeTeammates.set(key, { file, checkedAt: now });
  return file;
}

// A teammate's session opens with a user entry naming its team and its name
// (`teamName`, `agentName`) and carrying the Agent call's prompt as a
// <teammate-message>. A team reuses names, so the prompt has to match as well;
// of sessions that match, the first to start after the call is the one.
function findClaudeTeammate(projectDir, spawn) {
  let best = null;
  for (const name of fs.readdirSync(projectDir)) {
    if (!name.endsWith(".jsonl")) {
      continue;
    }
    const file = path.join(projectDir, name);
    let stat;
    try {
      stat = fs.statSync(file);
    } catch (error) {
      if (error?.code === "ENOENT") {
        continue; // removed since the listing
      }
      throw error;
    }
    if (stat.mtimeMs < spawn.ts) {
      continue; // last written before the call
    }
    const head = claudeSessionHead(file, stat.size);
    if (
      !head ||
      head.team !== spawn.teammate.team ||
      head.agent !== spawn.teammate.name ||
      head.prompt !== spawn.prompt ||
      head.ts === null ||
      head.ts < spawn.ts ||
      (best && head.ts >= best.ts)
    ) {
      continue;
    }
    best = { file, ts: head.ts };
  }
  return best?.file ?? null;
}

// A session's first user entry, as far as teammates go: { team, agent, prompt,
// ts }, or null for a session that is not a teammate's. It never changes once
// written, so it is read once; a session without one yet is read again.
const HEAD_READ_BYTES = 64 * 1024;
const claudeHeads = new Map(); // session log -> head or null

function claudeSessionHead(file, size) {
  if (claudeHeads.has(file)) {
    return claudeHeads.get(file);
  }
  for (let length = Math.min(size, HEAD_READ_BYTES); length > 0; length = length === size ? 0 : Math.min(size, length * 4)) {
    const buffer = Buffer.alloc(length);
    const fd = fs.openSync(file, "r");
    try {
      fs.readSync(fd, buffer, 0, length, 0);
    } finally {
      fs.closeSync(fd);
    }
    const lines = buffer.toString("utf8").split("\n");
    lines.pop(); // still being written, or empty after the last newline
    for (const line of lines) {
      if (!line.includes('"type":"user"')) {
        continue;
      }
      let entry;
      try {
        entry = JSON.parse(line);
      } catch {
        continue;
      }
      if (entry.type !== "user") {
        continue;
      }
      const message = typeof entry.message?.content === "string" ? peerMessages(entry.message.content, entry.origin) : null;
      const head = typeof entry.teamName === "string" && typeof entry.agentName === "string" && message?.length === 1
        ? { team: entry.teamName, agent: entry.agentName, prompt: message[0].body, ts: toMillis(entry.timestamp) }
        : null;
      claudeHeads.set(file, head);
      return head;
    }
  }
  return null;
}

// Codex (multi-agent v2): spawn_agent({task_name, message}) answers with the
// subagent's path, `/root/<task_name>`; the message itself is encrypted. The
// parent's rollout records the thread each call started (SubAgentActivity),
// and that thread's rollout opens with a session_meta naming it.
function attachCodexSubagents(reader, { home, live, shown, depth }) {
  for (const spawn of reader.spawns.values()) {
    if (shown && !shown.has(spawn.message)) {
      continue;
    }
    const file = spawn.threadId ? codexChildFile(home, spawn.threadId) : null;
    const child = file ? followFile(file, () => codexReader({ child: true })) : null;
    if (child && depth + 1 < SUBAGENT_DEPTH) {
      attachCodexSubagents(child, { home, live, shown: null, depth: depth + 1 });
    }
    const status = spawn.error
      ? "failed"
      : child?.turn === "done" ? "done" : child?.turn === "aborted" ? "stopped" : live ? "running" : "stopped";
    const meta = child?.meta ?? {};
    spawn.message.subagent = {
      id: spawn.threadId,
      backend: "codex",
      kind: typeof meta.agent_role === "string" ? meta.agent_role : null,
      name: typeof meta.agent_nickname === "string" ? meta.agent_nickname : null,
      description: spawn.taskName ? spawn.taskName.replace(/[_-]+/g, " ").trim() : null,
      prompt: null,
      status,
      startedAt: spawn.ts,
      endedAt: status === "running" ? null : child?.endedAt ?? null,
      toolCalls: child?.toolCalls ?? 0,
      result: spawn.error ?? (status === "done" ? child.result : null),
      ...shownMessages(child),
    };
  }
}

function shownMessages(reader) {
  if (!reader) {
    return { messages: [], earlier: 0 };
  }
  const messages = reader.messages.slice(-SUBAGENT_KEEP);
  return { messages, earlier: reader.dropped + reader.messages.length - messages.length };
}

// A subagent's rollout, looked for again at most every CHILD_RECHECK_MS
// while it is not written yet.
const codexMisses = new Map(); // thread id -> when its rollout was last looked for

function codexChildFile(home, threadId, now = Date.now()) {
  const missedAt = codexMisses.get(threadId);
  if (missedAt !== undefined && now - missedAt < CHILD_RECHECK_MS) {
    return null;
  }
  const file = codexRolloutFile(home, threadId);
  if (file) {
    codexMisses.delete(threadId);
  } else {
    codexMisses.set(threadId, now);
  }
  return file;
}

// Logs followed by appended bytes: file -> { offset, carry, reader }. A log
// only grows; one that shrank was replaced and is read again from the start.
const FOLLOW_MAX = 256;
const followed = new Map();

function followFile(file, makeReader) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null;
    }
    throw error;
  }
  let state = followed.get(file);
  if (state && stat.size < state.offset) {
    state = null;
  }
  if (!state) {
    state = { offset: 0, carry: null, reader: makeReader() };
  }
  // Most recently used last, so the oldest is the one let go.
  followed.delete(file);
  followed.set(file, state);
  if (followed.size > FOLLOW_MAX) {
    followed.delete(followed.keys().next().value);
  }
  if (stat.size > state.offset) {
    // The byte offset of the buffer's first byte, which the carried part starts at.
    const at = state.offset - (state.carry?.length ?? 0);
    const chunk = Buffer.alloc(stat.size - state.offset);
    const fd = fs.openSync(file, "r");
    let length;
    try {
      length = fs.readSync(fd, chunk, 0, chunk.length, state.offset);
    } finally {
      fs.closeSync(fd);
    }
    state.offset += length;
    const buffer = state.carry ? Buffer.concat([state.carry, chunk.subarray(0, length)]) : chunk.subarray(0, length);
    let start = 0;
    for (let end = buffer.indexOf(10, start); end !== -1; end = buffer.indexOf(10, start)) {
      if (end > start && !state.reader.skip(buffer.toString("utf8", start, Math.min(end, start + 160)))) {
        state.reader.addLine(buffer.toString("utf8", start, end), at + start);
      }
      start = end + 1;
    }
    // A line still being written waits for the rest of it.
    state.carry = start < buffer.length ? Buffer.from(buffer.subarray(start)) : null;
  }
  return state.reader;
}

// ---------------------------------------------------------------------------
// Live numbers for the agent page (cloud/app/agent-stats.js), from the same
// session log as the chat: output speed and how full the context is.
//
// Output speed is output tokens over wall time per model response, summed
// over the last SPEED_RESPONSES finished responses (so a short reply does not
// count as much as a long one). A response's time runs from its request
// going out to its last recorded block, so it includes time to first token;
// output tokens include thinking/reasoning tokens.
// ---------------------------------------------------------------------------

export const SPEED_RESPONSES = 5;
const KEPT_RESPONSES = 20;

function keepResponse(list, response) {
  list.push(response);
  if (list.length > KEPT_RESPONSES) {
    list.shift();
  }
}

function speedOf(responses) {
  const timed = responses.filter((r) => r.done && r.start !== null && r.end > r.start && r.tokens > 0).slice(-SPEED_RESPONSES);
  if (!timed.length) {
    return null;
  }
  const tokens = timed.reduce((sum, r) => sum + r.tokens, 0);
  const ms = timed.reduce((sum, r) => sum + (r.end - r.start), 0);
  return {
    tokensPerSecond: Math.round((tokens / ms) * 10_000) / 10,
    responses: timed.map((r) => ({ tokens: r.tokens, ms: r.end - r.start, at: r.end })),
  };
}

// Claude Code: one assistant entry per content block, all carrying the
// response's message id and usage. The request goes out at Claude Code's
// `api-request` entry, or right after the tool result or prompt entry that
// precedes it; the newest of those since the last response is its start.
// Context in use is the newest response's input (fresh + cache read + cache
// write) plus its output; after a compaction it is the boundary's postTokens
// until the next response reports.
export function claudeStats() {
  let requestAt = null;
  let current = null;
  const responses = [];
  let model = null;
  let context = null;
  let compactedAt = null;
  return {
    add(entry) {
      if (entry.isSidechain) {
        return;
      }
      const ts = toMillis(entry.timestamp);
      if (entry.type === "api-request" || entry.type === "user") {
        if (ts !== null) {
          requestAt = Math.max(requestAt ?? ts, ts);
        }
        return;
      }
      if (entry.type === "system" && entry.subtype === "compact_boundary") {
        const post = Number(entry.compactMetadata?.postTokens);
        compactedAt = ts;
        context = { used: Number.isFinite(post) && post > 0 ? post : null, at: ts };
        return;
      }
      if (entry.type !== "assistant" || ts === null) {
        return;
      }
      const message = entry.message || {};
      // `<synthetic>`: Claude Code's own error and interrupt notes.
      if (typeof message.model !== "string" || message.model.startsWith("<")) {
        return;
      }
      const usage = message.usage || {};
      const output = Number(usage.output_tokens) || 0;
      const done = message.stop_reason !== null && message.stop_reason !== undefined;
      if (current && message.id && current.id === message.id) {
        current.end = ts;
        current.tokens = Math.max(current.tokens, output);
        current.done = current.done || done;
      } else {
        current = { id: message.id || null, start: requestAt, end: ts, tokens: output, done };
        keepResponse(responses, current);
        requestAt = null;
      }
      model = message.model;
      const input = (Number(usage.input_tokens) || 0) + (Number(usage.cache_read_input_tokens) || 0) + (Number(usage.cache_creation_input_tokens) || 0);
      context = { used: input + current.tokens, at: ts };
    },
    result(home) {
      const window = model ? claudeWindow(home, model) : { window: null, modelWindow: null, autoCompactWindow: null };
      return {
        model,
        speed: speedOf(responses),
        context: context ? { ...context, ...window, compactedAt } : null,
      };
    },
  };
}

// Codex: one `token_usage_record` per finished response, with its usage. Its
// request goes out after the newest input-side entry (turn start, user or
// developer message, tool output, the previous response's record) before the
// response's first output item. Context in use and the window are Codex's own
// `token_count` figures (last_token_usage.total_tokens, model_context_window),
// which Codex also reports right after a compaction.
export function codexStats() {
  let inputAt = null;
  let startAt = null;
  let outputSeen = false;
  const responses = [];
  let model = null;
  let context = null;
  let compactedAt = null;
  return {
    add(entry) {
      const ts = toMillis(entry.timestamp);
      if (ts === null) {
        return;
      }
      const payload = entry.payload || {};
      if (entry.type === "turn_context") {
        if (typeof payload.model === "string" && payload.model) {
          model = payload.model;
        }
        inputAt = ts;
      } else if (entry.type === "compacted") {
        compactedAt = ts;
        inputAt = ts;
      } else if (entry.type === "token_usage_record") {
        keepResponse(responses, { start: outputSeen ? startAt : inputAt, end: ts, tokens: Number(payload.usage?.output_tokens) || 0, done: true });
        startAt = null;
        outputSeen = false;
        inputAt = ts;
      } else if (entry.type === "event_msg") {
        if (payload.type === "task_started") {
          inputAt = ts;
        } else if (payload.type === "token_count" && payload.info) {
          const used = Number(payload.info.last_token_usage?.total_tokens);
          const window = Number(payload.info.model_context_window);
          context = {
            used: Number.isFinite(used) && used >= 0 ? used : null,
            window: Number.isFinite(window) && window > 0 ? window : null,
            at: ts,
          };
        }
      } else if (entry.type === "response_item") {
        if (isCodexOutput(payload)) {
          if (!outputSeen) {
            startAt = inputAt;
            outputSeen = true;
          }
        } else {
          inputAt = ts;
        }
      }
    },
    result() {
      return {
        model,
        speed: speedOf(responses),
        context: context ? { ...context, modelWindow: context.window, autoCompactWindow: null, compactedAt } : null,
      };
    },
  };
}

function isCodexOutput(payload) {
  const type = String(payload.type || "");
  return type === "reasoning" || (type === "message" && payload.role === "assistant") || type.endsWith("_call");
}

// The window a Claude conversation's context is measured against: the
// model's context window from the models.dev catalog `rudder __models` keeps
// in ~/.rudder, or the auto-compact window Claude Code's user settings set
// (`/autocompact` saves it per model) when that is smaller. Claude Code
// compacts as the context nears the smaller of the two.
function claudeWindow(home, model) {
  const catalog = readJsonCached(path.join(home, ".rudder", "models-dev.json"), (data) => {
    const windows = new Map();
    for (const [id, entry] of Object.entries(data?.anthropic?.models ?? {})) {
      const context = Number(entry?.limit?.context);
      if (Number.isInteger(context) && context > 0) {
        windows.set(id, context);
      }
    }
    return windows;
  });
  const settings = readJsonCached(path.join(home, ".claude", "settings.json"), (data) => data ?? {});
  const modelWindow = catalog?.get(model) ?? null;
  // Per-model settings are keyed by the canonical id (no date, no [1m]).
  const canonical = model.replace(/\[1m\]$/i, "").replace(/-\d{8}$/, "");
  const perModel = settings?.modelSettings?.[canonical]?.autoCompactWindow;
  const setting = perModel !== undefined ? perModel : settings?.autoCompactWindow;
  // "auto" (or nothing set) is the window Claude Code tunes for the model.
  const autoCompactWindow = Number.isInteger(setting) && setting > 0 ? setting : null;
  const window = autoCompactWindow && modelWindow ? Math.min(autoCompactWindow, modelWindow) : autoCompactWindow ?? modelWindow;
  return { window, modelWindow, autoCompactWindow: window === autoCompactWindow ? autoCompactWindow : null };
}

const jsonFiles = new Map(); // file -> { mtimeMs, size, value }

/** A parsed JSON file, re-read only when it changes. Null when it does not exist. */
function readJsonCached(file, parse) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch (error) {
    if (error?.code === "ENOENT") {
      jsonFiles.delete(file);
      return null;
    }
    throw error;
  }
  const hit = jsonFiles.get(file);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) {
    return hit.value;
  }
  let value;
  try {
    value = parse(JSON.parse(fs.readFileSync(file, "utf8")));
  } catch (error) {
    throw new Error(`could not read ${file}: ${error instanceof Error ? error.message : String(error)}`);
  }
  jsonFiles.set(file, { mtimeMs: stat.mtimeMs, size: stat.size, value });
  return value;
}

// ---------------------------------------------------------------------------

function oneLine(text) {
  const value = String(text || "").replace(/\s+/g, " ").trim();
  return value.length > 240 ? `${value.slice(0, 239)}…` : value;
}

function clip(text, limit = TOOL_OUTPUT_LIMIT) {
  const value = String(text || "");
  return value.length > limit ? `${value.slice(0, limit)}\n… (${value.length - limit} more characters)` : value;
}

function toMillis(value) {
  if (value === null || value === undefined || value === "") {
    return null;
  }
  const number = Number(value);
  if (Number.isFinite(number)) {
    return number;
  }
  const parsed = Date.parse(String(value));
  return Number.isFinite(parsed) ? parsed : null;
}

// The chat's subagent tray (subagents.mjs) reads subagents with the same readers.
export { SUBAGENT_PROMPT_LIMIT, attachClaudeSubagents, attachCodexSubagents, claudeInterrupted, claudeReader, claudeSessionHead, claudeSubagentMetas, claudeSubagentsDir, clip, codexReader, followFile, oneLine, readRunRecord, shownMessages, subagentsLive };
