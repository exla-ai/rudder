// Read an agent's conversation off disk for the phone app.
//
// Claude Code keeps one JSONL per session at
//   ~/.claude/projects/<cwd with every non-alphanumeric char as "->/<id>.jsonl
// and Codex keeps rollouts at ~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl.
// Both are normalized to { id, role, ts, text, tool } messages.

import fs from "node:fs";
import path from "node:path";
import { promptSuggestion } from "./suggestion.mjs";
import { activeClaudeLines, editablePrompt, readCodexLineage } from "./session-lineage.mjs";

const TAIL_BYTES = 6 * 1024 * 1024;
const PREVIEW_TAIL_BYTES = 256 * 1024;
const TOOL_OUTPUT_LIMIT = 12_000;

export function readTranscript(repoDir, runId, { limit = 300, home }) {
  const record = readRunRecord(repoDir, runId);
  if (!record) {
    throw new Error("agent not found");
  }
  const file = transcriptFile(record, home);
  const base = {
    runId: record.id,
    title: (record.taskSummary || record.task || record.id || "").toString(),
    backend: record.backend || "claude",
    status: record.status || null,
    truncated: false,
    messages: [],
  };
  if (!file) {
    // No session on disk yet (just launched, or a backend without transcripts):
    // show the prompts Rudder itself recorded.
    base.messages = (record.turns || []).map((turn, index) => ({
      id: `turn-${index}`,
      role: "user",
      ts: toMillis(turn.ts),
      text: String(turn.prompt || ""),
    }));
    return base;
  }
  const codex = record.backend === "codex";
  // A Codex fork references its parent's history; a Claude rewind leaves the
  // old branch in the file. Both read as the one conversation the agent has.
  const { text, truncated } = codex ? readCodexLineage(file, home, TAIL_BYTES) : readTail(file, TAIL_BYTES);
  const lines = codex ? text.split("\n") : activeClaudeLines(text.split("\n"));
  const stats = codex ? codexStats() : claudeStats();
  const messages = showUserTask(codex ? parseCodex(lines, stats) : parseClaude(lines, stats), record);
  base.truncated = truncated || messages.length > limit;
  base.messages = messages.slice(-limit);
  // The one prompt the app may offer to edit (chat-edit.js).
  base.edit = editablePrompt(lines, codex ? "codex" : "claude");
  // Claude Code's suggested next prompt, saved by Rudder's Claude plugin.
  base.suggestion = promptSuggestion(home, record.id);
  // The numbers are extra: a model catalog or settings file that cannot be
  // read says so in `stats.error` and the conversation still shows.
  try {
    base.stats = stats.result(home);
  } catch (error) {
    base.stats = { error: error instanceof Error ? error.message : String(error) };
  }
  return base;
}

// The newest assistant text, for the agent list's preview line. Re-reads only
// when the transcript file changed.
// A working agent's transcript changes every second; its preview line only
// needs to keep up with a person glancing at the phone.
const PREVIEW_REFRESH_MS = 5_000;

export function lastAssistantText(record, { home, cache, now = Date.now() }) {
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
  const codex = record.backend === "codex";
  const { text } = codex ? readCodexLineage(file, home, PREVIEW_TAIL_BYTES) : readTail(file, PREVIEW_TAIL_BYTES);
  const lines = codex ? text.split("\n") : activeClaudeLines(text.split("\n"));
  const messages = codex ? parseCodex(lines) : parseClaude(lines);
  const last = [...messages].reverse().find((message) => message.role === "assistant" && message.text?.trim());
  const preview = last ? last.text.replace(/\s+/g, " ").trim().slice(0, 280) : null;
  const remoteUrl = codex ? null : claudeSessionUrl(lines) ?? cache?.remoteUrl ?? null;
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
// ("Objective: ...", "Rudder-specific context injected by Rudder: ..."). In the
// chat that turn is what the user asked for, so it shows as the task itself.
const RUDDER_FRAMING = /Rudder-specific context injected by Rudder|^Objective: [\s\S]*\nDone when:/;

function showUserTask(messages, record) {
  const task = userTask(record.task);
  return messages.map((message) => {
    if (message.role !== "user" || !RUDDER_FRAMING.test(message.text)) {
      return message;
    }
    return task ? { ...message, text: task } : message;
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
  try {
    const record = JSON.parse(fs.readFileSync(path.join(repoDir, ".rudder", "runs", runId, "run.json"), "utf8"));
    return record && record.id === runId ? record : null;
  } catch {
    return null;
  }
}

// Where each session's transcript lives, found once. A miss means searching
// every Claude project folder (hundreds on a long-lived home), so it is
// remembered too and searched again at most every MISS_RECHECK_MS.
const MISS_RECHECK_MS = 30_000;
const transcriptPaths = new Map(); // sessionId -> { file, checkedAt }

export function transcriptFile(record, home, now = Date.now()) {
  const sessionId = record?.session?.nativeSessionId;
  if (!sessionId || !/^[A-Za-z0-9-]+$/.test(sessionId)) {
    return null;
  }
  const known = transcriptPaths.get(sessionId);
  if (known?.file && fs.existsSync(known.file)) {
    return known.file;
  }
  if (known && !known.file && now - known.checkedAt < MISS_RECHECK_MS) {
    return null;
  }
  const file = findTranscript(record, home, sessionId);
  transcriptPaths.set(sessionId, { file, checkedAt: now });
  return file;
}

function findTranscript(record, home, sessionId) {
  if ((record.backend || "claude") === "codex") {
    return codexRollout(home, sessionId);
  }
  const cwd = record.workspace?.path || record.worktree?.path || record.repoRoot || "";
  const projects = path.join(home, ".claude", "projects");
  const direct = path.join(projects, String(cwd).replace(/[^A-Za-z0-9-]/g, "-"), `${sessionId}.jsonl`);
  if (fs.existsSync(direct)) {
    return direct;
  }
  try {
    for (const dir of fs.readdirSync(projects)) {
      const candidate = path.join(projects, dir, `${sessionId}.jsonl`);
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
  } catch {
    // no projects dir yet
  }
  return null;
}

const codexCache = new Map();

function codexRollout(home, sessionId) {
  const cached = codexCache.get(sessionId);
  if (cached && fs.existsSync(cached)) {
    return cached;
  }
  const root = path.join(home, ".codex", "sessions");
  let newest = null;
  let newestMtime = 0;
  const walk = (dir, depth) => {
    let entries = [];
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory() && depth < 3) {
        walk(full, depth + 1);
      } else if (entry.isFile() && entry.name.includes(sessionId) && entry.name.endsWith(".jsonl")) {
        const mtime = fs.statSync(full).mtimeMs;
        if (mtime > newestMtime) {
          newest = full;
          newestMtime = mtime;
        }
      }
    }
  };
  walk(root, 0);
  if (newest) {
    codexCache.set(sessionId, newest);
  }
  return newest;
}

function readTail(file, maxBytes) {
  const stat = fs.statSync(file);
  const start = Math.max(0, stat.size - maxBytes);
  const length = stat.size - start;
  const buffer = Buffer.alloc(length);
  const fd = fs.openSync(file, "r");
  try {
    fs.readSync(fd, buffer, 0, length, start);
  } finally {
    fs.closeSync(fd);
  }
  let text = buffer.toString("utf8");
  if (start > 0) {
    // Drop the partial first line.
    text = text.slice(text.indexOf("\n") + 1);
  }
  return { text, truncated: start > 0 };
}

// ---------------------------------------------------------------------------
// Claude Code JSONL.
// ---------------------------------------------------------------------------

function parseClaude(lines, stats = null) {
  const messages = [];
  const tools = new Map(); // tool_use id -> message
  for (const line of lines) {
    if (!line.trim()) {
      continue;
    }
    let entry;
    try {
      entry = JSON.parse(line);
    } catch {
      continue;
    }
    stats?.add(entry);
    if (entry.type === "system" && entry.subtype === "compact_boundary") {
      const meta = entry.compactMetadata || {};
      messages.push(compactMessage(entry.uuid || `compact-${messages.length}`, toMillis(entry.timestamp), {
        trigger: typeof meta.trigger === "string" ? meta.trigger : null,
        preTokens: Number(meta.preTokens) || null,
      }));
      continue;
    }
    // A message typed while the agent works: Claude Code hands it to the running
    // turn as a queued_command attachment, never as a user entry.
    const queued = entry.type === "attachment" && !entry.isSidechain ? entry.attachment : null;
    if (queued?.type === "queued_command" && queued.commandMode === "prompt" && queued.origin?.kind === "human") {
      const prompt = Array.isArray(queued.prompt)
        ? queued.prompt.filter((part) => part?.type === "text").map((part) => part.text).join("\n")
        : queued.prompt;
      const text = cleanUserText(prompt);
      if (text) {
        messages.push({ id: entry.uuid, role: "user", ts: toMillis(entry.timestamp), text });
      }
      continue;
    }
    if ((entry.type !== "user" && entry.type !== "assistant") || entry.isSidechain || entry.isMeta) {
      continue;
    }
    const ts = toMillis(entry.timestamp);
    if (entry.isCompactSummary) {
      // The summary Claude starts over from is not something the user said:
      // it belongs to the compaction marker just before it.
      const summary = compactSummaryText(entry.message?.content);
      const last = messages[messages.length - 1];
      if (last?.kind === "compact" && !last.compact.summary) {
        last.compact.summary = summary;
      } else {
        messages.push(compactMessage(entry.uuid || `compact-${messages.length}`, ts, { summary }));
      }
      continue;
    }
    const content = entry.message?.content;
    const uuid = entry.uuid || `${messages.length}`;
    if (typeof content === "string") {
      const text = cleanUserText(content);
      if (text) {
        messages.push({ id: uuid, role: entry.type === "user" ? "user" : "assistant", ts, text });
      }
      continue;
    }
    if (!Array.isArray(content)) {
      continue;
    }
    content.forEach((block, index) => {
      const id = `${uuid}:${index}`;
      if (block?.type === "text") {
        const text = entry.type === "user" ? cleanUserText(block.text) : String(block.text || "").trim();
        if (text) {
          messages.push({ id, role: entry.type === "user" ? "user" : "assistant", ts, text });
        }
      } else if (block?.type === "tool_use") {
        const message = {
          id: block.id || id,
          role: "tool",
          ts,
          text: "",
          tool: { name: block.name || "tool", input: summarizeClaudeInput(block.name, block.input), output: null, isError: false },
        };
        tools.set(block.id, message);
        messages.push(message);
      } else if (block?.type === "tool_result") {
        const target = tools.get(block.tool_use_id);
        const output = clip(toolResultText(block.content));
        if (target) {
          target.tool.output = output;
          target.tool.isError = Boolean(block.is_error);
        }
      }
    });
  }
  return messages;
}

// When an agent's context fills up, Claude Code and Codex summarize the
// conversation and carry on from the summary. The chat shows that as one
// marker (with the summary behind it), not as a wall of text from the user.
function compactMessage(id, ts, { trigger = null, preTokens = null, summary = "" } = {}) {
  return { id, role: "system", kind: "compact", ts, text: "Context compacted", compact: { trigger, preTokens, summary } };
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
  // Harness-injected turns (command output, reminders, caveats) are not the user.
  if (!value || /^<(local-command|command-|system-reminder|bash-|user-memory|task-notification)/.test(value)) {
    return "";
  }
  return value.replace(/<system-reminder>[\s\S]*?<\/system-reminder>/g, "").trim();
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

function parseCodex(lines, stats = null) {
  const messages = [];
  const calls = new Map(); // call_id -> message
  let index = 0;
  for (const line of lines) {
    if (!line.trim()) {
      continue;
    }
    let entry;
    try {
      entry = JSON.parse(line);
    } catch {
      continue;
    }
    stats?.add(entry);
    index += 1;
    if (entry.type === "compacted") {
      messages.push(compactMessage(`c${index}`, toMillis(entry.timestamp), { summary: String(entry.payload?.message || "").trim() }));
      continue;
    }
    if (entry.type !== "response_item") {
      continue;
    }
    const payload = entry.payload || {};
    const ts = toMillis(entry.timestamp);
    const id = `c${index}`;
    if (payload.type === "message") {
      const text = (payload.content || [])
        .map((part) => part?.text || "")
        .join("\n")
        .trim();
      if (!text) {
        continue;
      }
      if (payload.role === "user") {
        const cleaned = /^<(environment_context|user_instructions|permissions|turn_aborted)/.test(text) ? "" : text;
        if (cleaned && !text.startsWith("# AGENTS.md instructions")) {
          messages.push({ id: payload.id || id, role: "user", ts, text: cleaned });
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
      }
      messages.push(message);
    } else if (payload.type === "function_call_output" || payload.type === "custom_tool_call_output" || payload.type === "local_shell_call_output") {
      const target = calls.get(payload.call_id);
      if (target) {
        const { output, isError } = codexOutput(payload.output);
        target.tool.output = clip(output);
        target.tool.isError = isError;
      }
    } else if (payload.type === "reasoning") {
      // Summaries only; raw reasoning is encrypted.
      const summary = (payload.summary || []).map((part) => part?.text || "").join("\n").trim();
      if (summary) {
        messages.push({ id, role: "system", ts, text: summary });
      }
    }
  }
  return messages;
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

function clip(text) {
  const value = String(text || "");
  return value.length > TOOL_OUTPUT_LIMIT ? `${value.slice(0, TOOL_OUTPUT_LIMIT)}\n… (${value.length - TOOL_OUTPUT_LIMIT} more characters)` : value;
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
