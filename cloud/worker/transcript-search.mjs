// Search everything (cloud/app/global-search.js): what the user and the
// agents said in every chat on this machine. Tool calls and their output,
// which the chat folds away, are left out.
//
// A session file can be gigabytes. Each is read in chunks of whole lines and
// a chunk is searched raw, for the query as a JSON string holds it (Claude
// Code, Codex and agy all write UTF-8 as is). Only the newest lines that hit
// are read into messages, by the chat's own reader, so a message found here
// has the id the chat gives it (chat-find.js opens the chat at it) and its
// text as the chat shows it. Chats are searched newest first until the time
// runs out; `partial` says the oldest were not reached.

import fs from "node:fs";
import path from "node:path";
import { antigravityReader, claudeReader, codexReader, readRunRecord, showAttachments, showUserTask, transcriptFile } from "./transcripts.mjs";
import { codexLineage } from "./session-lineage.mjs";

const RESULTS = 50;
const PER_CHAT = 5;
// Of the lines that hit in one chat, the newest this many are read into
// messages: a common word hits in tool output on most lines.
const LINES_READ = 200;
const BUDGET_MS = 4_000;
// A chunk, and the longest line searched: a longer one (a pasted image) holds no words.
const CHUNK = 8 * 1024 * 1024;
const BEFORE = 50;
const AFTER = 110;

// OpenCode keeps its chats in a database, so its agents are not searched.
const READERS = { claude: claudeReader, codex: codexReader, antigravity: antigravityReader };
// Whether a line can hold something the user or an agent said, from its
// first HEAD bytes. The others repeat words (Claude's copies of its API
// requests, Codex's events) or hold tool calls and output, and would crowd
// the lines that matter out of LINES_READ. Claude Code writes each block of
// a message on a line of its own, so a call or a result is the first block.
const HEAD = 1000;
const SAYS = {
  claude: (head) => !/^\{"type":"(?!(?:user|assistant|attachment)")/.test(head)
    && !/"content":\[\{"(?:tool_use_id"|type":"(?:tool_use|tool_result)")/.test(head),
  codex: (head) => head.includes('"type":"response_item"') && head.includes('"payload":{"type":"message"'),
  antigravity: (head) => /"type":"(?:USER_INPUT|PLANNER_RESPONSE)"/.test(head),
};

// The newest search: one the person typed past stops at its next chunk.
let latest = 0;

/**
 * Messages matching `query` in the chats of `projects` ({ workspaceId, dir }),
 * newest first: `{ results: [{ workspaceId, runId, messageId, role, ts,
 * snippet }], partial }`.
 */
export async function searchChats(projects, { home, query, budgetMs = BUDGET_MS }) {
  const words = String(query ?? "").replace(/\s+/g, " ").trim();
  if (words.length < 2) {
    throw new Error("Type at least 2 characters to search messages");
  }
  const mine = ++latest;
  const deadline = Date.now() + budgetMs;
  const search = { pattern: rawPattern(words), needle: words.toLowerCase(), deadline, mine };
  const results = [];
  let partial = false;
  for (const chat of listChats(projects, home)) {
    const found = await searchChat(chat, home, search);
    if (!found) {
      partial = true;
      break;
    }
    results.push(...found);
  }
  results.sort((a, b) => (b.ts ?? 0) - (a.ts ?? 0));
  return { results: results.slice(0, RESULTS), partial };
}

// Every chat with a session file, the most recently written first. Runs that
// share a session (a branch that has not said anything yet) are searched once.
function listChats(projects, home) {
  const chats = [];
  const files = new Set();
  for (const project of projects) {
    let runIds;
    try {
      runIds = fs.readdirSync(path.join(project.dir, ".rudder", "runs"));
    } catch (error) {
      if (error?.code === "ENOENT") {
        continue; // no agent has run in this project yet
      }
      throw error;
    }
    for (const runId of runIds) {
      const record = readRunRecord(project.dir, runId);
      if (!record || !READERS[record.backend || "claude"]) {
        continue;
      }
      const file = transcriptFile(record, home);
      if (!file || files.has(file)) {
        continue;
      }
      files.add(file);
      chats.push({ workspaceId: project.workspaceId, record, file, mtimeMs: fs.statSync(file).mtimeMs });
    }
  }
  return chats.sort((a, b) => b.mtimeMs - a.mtimeMs);
}

// A chat's newest PER_CHAT matching messages, or null when the time ran out.
async function searchChat(chat, home, { pattern, needle, deadline, mine }) {
  const backend = chat.record.backend || "claude";
  // A Codex fork shows its parent's history first; message ids count bytes across both.
  const segments = backend === "codex" ? codexLineage(chat.file, home) : [{ file: chat.file, end: fs.statSync(chat.file).size }];
  // The newest LINES_READ lines that hit, in a ring: { file, offset, length, at (in the conversation) }.
  const ring = [];
  const says = SAYS[backend];
  let hits = 0;
  let base = 0;
  for (const segment of segments) {
    for await (const { at, text } of wholeLines(segment.file, segment.end)) {
      if (mine !== latest) {
        throw new Error("A newer search replaced this one");
      }
      if (Date.now() > deadline) {
        return null;
      }
      pattern.lastIndex = 0;
      for (let hit = pattern.exec(text); hit; hit = pattern.exec(text)) {
        const start = text.lastIndexOf("\n", hit.index) + 1;
        const newline = text.indexOf("\n", hit.index);
        const end = newline < 0 ? text.length : newline;
        pattern.lastIndex = end + 1;
        if (!says(text.slice(start, Math.min(end, start + HEAD)))) {
          continue;
        }
        ring[hits % LINES_READ] = { file: segment.file, offset: at + start, length: end - start, at: base + at + start };
        hits += 1;
      }
    }
    base += segment.end;
  }
  const found = [];
  const seen = new Set();
  for (let index = hits - 1; index >= Math.max(0, hits - LINES_READ) && found.length < PER_CHAT; index -= 1) {
    const hit = ring[index % LINES_READ];
    const reader = READERS[backend]({});
    reader.addLine(readLine(hit.file, hit.offset, hit.length), hit.at);
    const messages = showAttachments(showUserTask(reader.messages, chat.record));
    for (const message of messages.reverse()) {
      if ((message.role !== "user" && message.role !== "assistant") || seen.has(message.id) || found.length >= PER_CHAT) {
        continue;
      }
      const flat = String(message.text || "").replace(/\s+/g, " ").trim();
      const at = flat.toLowerCase().indexOf(needle);
      if (at < 0) {
        continue;
      }
      seen.add(message.id);
      found.push({
        workspaceId: chat.workspaceId,
        runId: chat.record.id,
        messageId: message.id,
        role: message.role,
        ts: message.ts ?? null,
        snippet: snippet(flat, at, needle.length),
      });
    }
  }
  return found;
}

/**
 * The query as a session line holds it: inside a JSON string, its UTF-8
 * bytes read one per character (latin1), so a match's index is its byte
 * offset. ASCII letters match in either case, as does any other letter in the
 * cases the query's own has; what that lets through, the messages read after
 * it check.
 */
export function rawPattern(query) {
  let source = "";
  for (const char of JSON.stringify(query).slice(1, -1)) {
    if (char.charCodeAt(0) < 0x80) {
      source += char.replace(/[\\^$.*+?()[\]{}|/]/g, "\\$&");
      continue;
    }
    const forms = new Set([char, char.toLowerCase(), char.toUpperCase()].filter((form) => [...form].length === 1));
    source += `(?:${[...forms].map((form) => Buffer.from(form, "utf8").toString("latin1")).join("|")})`;
  }
  return new RegExp(source, "gi");
}

/**
 * A session's bytes up to `end` as latin1 text, a chunk of whole lines at a
 * time: `{ at, text }`, `at` being where the text starts in the file. A line
 * longer than a chunk is passed over. The last piece may be a line still
 * being written.
 */
async function* wholeLines(file, end) {
  const handle = await fs.promises.open(file, "r");
  try {
    const buffer = Buffer.allocUnsafe(CHUNK);
    let at = 0; // where buffer[0] is in the file
    let filled = 0;
    let skipping = false;
    while (at + filled < end) {
      const { bytesRead } = await handle.read(buffer, filled, Math.min(CHUNK - filled, end - at - filled), at + filled);
      if (bytesRead === 0) {
        throw new Error(`${file} is shorter than the ${end} bytes it had`);
      }
      filled += bytesRead;
      let from = 0;
      if (skipping) {
        const newline = buffer.subarray(0, filled).indexOf(0x0a);
        if (newline < 0) {
          at += filled;
          filled = 0;
          continue;
        }
        from = newline + 1;
        skipping = false;
      }
      const cut = at + filled >= end ? filled : buffer.subarray(0, filled).lastIndexOf(0x0a) + 1;
      if (cut > from) {
        yield { at: at + from, text: buffer.latin1Slice(from, cut) };
      } else if (from === 0 && filled === CHUNK) {
        skipping = true;
        at += filled;
        filled = 0;
        continue;
      }
      const keep = Math.max(cut, from);
      buffer.copy(buffer, 0, keep, filled);
      at += keep;
      filled -= keep;
    }
  } finally {
    await handle.close();
  }
}

function readLine(file, offset, length) {
  const buffer = Buffer.allocUnsafe(length);
  const fd = fs.openSync(file, "r");
  try {
    const read = fs.readSync(fd, buffer, 0, length, offset);
    return buffer.toString("utf8", 0, read);
  } finally {
    fs.closeSync(fd);
  }
}

// The words around the match, cut at spaces.
function snippet(flat, at, length) {
  let start = Math.max(0, at - BEFORE);
  if (start > 0) {
    const space = flat.indexOf(" ", start);
    if (space >= 0 && space < at) start = space + 1;
  }
  let end = Math.min(flat.length, at + length + AFTER);
  if (end < flat.length) {
    const space = flat.lastIndexOf(" ", end);
    if (space > at + length) end = space;
  }
  return `${start > 0 ? "…" : ""}${flat.slice(start, end)}${end < flat.length ? "…" : ""}`;
}
