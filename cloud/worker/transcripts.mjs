// Read an agent's conversation off disk for the phone app.
//
// Claude Code keeps one JSONL per session at
//   ~/.claude/projects/<cwd with every non-alphanumeric char as "->/<id>.jsonl
// and Codex keeps rollouts at ~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl.
// Both are normalized to { id, role, ts, text, tool } messages.

import fs from "node:fs";
import path from "node:path";

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
  const { text, truncated } = readTail(file, TAIL_BYTES);
  const lines = text.split("\n");
  const messages = showUserTask(record.backend === "codex" ? parseCodex(lines) : parseClaude(lines), record);
  base.truncated = truncated || messages.length > limit;
  base.messages = messages.slice(-limit);
  return base;
}

// The newest assistant text, for the agent list's preview line. Re-reads only
// when the transcript file changed.
export function lastAssistantText(record, { home, cache }) {
  const file = transcriptFile(record, home);
  if (!file) {
    return null;
  }
  const stat = fs.statSync(file);
  if (cache && cache.file === file && cache.mtimeMs === stat.mtimeMs) {
    return { text: cache.text, remoteUrl: cache.remoteUrl ?? null, cacheEntry: cache };
  }
  const { text } = readTail(file, PREVIEW_TAIL_BYTES);
  const lines = text.split("\n");
  const messages = record.backend === "codex" ? parseCodex(lines) : parseClaude(lines);
  const last = [...messages].reverse().find((message) => message.role === "assistant" && message.text?.trim());
  const preview = last ? last.text.replace(/\s+/g, " ").trim().slice(0, 280) : null;
  const remoteUrl = record.backend === "codex" ? null : claudeSessionUrl(lines) ?? cache?.remoteUrl ?? null;
  const entry = { file, mtimeMs: stat.mtimeMs, text: preview, remoteUrl };
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

function transcriptFile(record, home) {
  const sessionId = record?.session?.nativeSessionId;
  if (!sessionId || !/^[A-Za-z0-9-]+$/.test(sessionId)) {
    return null;
  }
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

function parseClaude(lines) {
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
    if (entry.type === "system" && entry.subtype === "compact_boundary") {
      const meta = entry.compactMetadata || {};
      messages.push(compactMessage(entry.uuid || `compact-${messages.length}`, toMillis(entry.timestamp), {
        trigger: typeof meta.trigger === "string" ? meta.trigger : null,
        preTokens: Number(meta.preTokens) || null,
      }));
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

function parseCodex(lines) {
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
          messages.push({ id, role: "user", ts, text: cleaned });
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
