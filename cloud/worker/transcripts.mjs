// Read an agent's conversation off disk for the phone app.
//
// Claude Code keeps one JSONL per session at
//   ~/.claude/projects/<cwd with every non-alphanumeric char as "->/<id>.jsonl
// and Codex keeps rollouts at ~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl.
// Both are normalized to { id, role, ts, text, tool } messages.

import fs from "node:fs";
import path from "node:path";
import { promptSuggestion } from "./suggestion.mjs";
import { CLAUDE_HARNESS_TEXT, activeClaudeLines, codexRolloutFile, codexTypedText, editablePrompts, readCodexLineage, unwrapPastedContent } from "./session-lineage.mjs";

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
// The run is alive, so its unfinished subagents are still running. Otherwise
// they stopped with it.
const LIVE_RUN = new Set(["created", "running", "steering", "verifying"]);

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
  const reader = codex ? parseCodex(lines, stats) : parseClaude(lines, stats);
  const messages = showUserTask(reader.messages, record);
  base.truncated = truncated || messages.length > limit;
  base.messages = messages.slice(-limit);
  // Subagents the shown part of the chat started, each on its spawn call.
  const shown = new Set(base.messages);
  const live = LIVE_RUN.has(record.status);
  if (codex) {
    attachCodexSubagents(reader, { home, live, shown, depth: 0 });
  } else {
    attachClaudeSubagents(reader, { dir: claudeSubagentsDir(file), projectDir: path.dirname(file), live, shown, depth: 0 });
  }
  // The prompts the app may offer to edit (chat-edit.js).
  base.edits = editablePrompts(lines, codex ? "codex" : "claude");
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
  const { messages } = codex ? parseCodex(lines) : parseClaude(lines);
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
  if (record.backend === "codex") {
    return codexRolloutFile(home, sessionId);
  }
  // Claude files a session under its cwd with every other character as "-",
  // except that a long cwd is cut and hashed: those are found by name.
  const projects = path.join(home, ".claude", "projects");
  const cwd = record.workspace?.path;
  if (typeof cwd === "string" && cwd) {
    const direct = path.join(projects, cwd.replace(/[^A-Za-z0-9-]/g, "-"), `${sessionId}.jsonl`);
    if (fs.existsSync(direct)) {
      return direct;
    }
  }
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
  const reader = claudeReader({ stats });
  for (const line of lines) {
    reader.addLine(line);
  }
  return reader;
}

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
  const outputLimit = sub ? SUBAGENT_OUTPUT_LIMIT : TOOL_OUTPUT_LIMIT;
  const reader = {
    messages: [],
    // Agent tool_use id -> { message, ts, prompt, description, isError, teammate }
    spawns: new Map(),
    // Agent tool_use id -> the status of its newest <task-notification>
    notifications: new Map(),
    toolCalls: 0,
    lastAt: null,
    // A teammate's turn ended and it waits for its next message, or its
    // process exited.
    idle: false,
    exited: false,
    dropped: 0,
    addLine(line) {
      if (!line.trim()) {
        return;
      }
      let entry;
      try {
        entry = JSON.parse(line);
      } catch {
        return;
      }
      add(entry);
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
  function add(entry) {
    stats?.add(entry);
    if (entry.type === "system" && entry.subtype === "compact_boundary") {
      const meta = entry.compactMetadata || {};
      push(compactMessage(entry.uuid || `compact-${messages.length}`, toMillis(entry.timestamp), {
        trigger: typeof meta.trigger === "string" ? meta.trigger : null,
        preTokens: Number(meta.preTokens) || null,
      }));
      return;
    }
    const own = sub || !entry.isSidechain;
    if (sub && entry.type === "system" && entry.subtype === "turn_duration") {
      reader.idle = true;
      reader.lastAt = toMillis(entry.timestamp) ?? reader.lastAt;
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
      if (queued.commandMode === "prompt" && queued.origin?.kind === "human") {
        const text = cleanUserText(prompt);
        if (text) {
          push({ id: entry.uuid, role: "user", ts: toMillis(entry.timestamp), text });
        }
      }
      return;
    }
    if ((entry.type !== "user" && entry.type !== "assistant") || !own || entry.isMeta) {
      return;
    }
    const ts = toMillis(entry.timestamp);
    reader.idle = false;
    reader.exited = false;
    if (sub && ts !== null) {
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
        push(compactMessage(entry.uuid || `compact-${messages.length}`, ts, { summary }));
      }
      return;
    }
    const content = entry.message?.content;
    const uuid = entry.uuid || `${messages.length}`;
    const userText = (text) => {
      noteTaskNotification(text);
      const clean = cleanUserText(text);
      if (clean && sub && !promptSeen) {
        promptSeen = true;
        return "";
      }
      return sub ? teammateMessage(clean)?.body ?? clean : clean;
    };
    if (typeof content === "string") {
      const text = entry.type === "user" ? userText(content) : cleanUserText(content);
      if (text) {
        push({ id: uuid, role: entry.type === "user" ? "user" : "assistant", ts, text });
      }
      return;
    }
    if (!Array.isArray(content)) {
      return;
    }
    content.forEach((block, index) => {
      const id = `${uuid}:${index}`;
      if (block?.type === "text") {
        const text = entry.type === "user" ? userText(block.text) : String(block.text || "").trim();
        if (text) {
          push({ id, role: entry.type === "user" ? "user" : "assistant", ts, text });
        }
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
        tools.set(block.id, message);
        reader.toolCalls += 1;
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
        if (target) {
          target.tool.output = clip(toolResultText(block.content), outputLimit);
          target.tool.isError = Boolean(block.is_error);
          // A followed subagent's reader lives across polls: a call is let go
          // once its result is in, so a long subagent does not pile them up.
          tools.delete(block.tool_use_id);
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
  return reader;
}

function xmlTag(text, tag) {
  const match = new RegExp(`<${tag}>([\\s\\S]*?)</${tag}>`).exec(text);
  return match ? match[1].trim() : null;
}

// What a teammate is told arrives wrapped as
// <teammate-message teammate_id="team-lead">\n<body>\n</teammate-message>.
function teammateMessage(text) {
  const match = /^<teammate-message teammate_id="[^"]*"[^>]*>\n([\s\S]*)\n<\/teammate-message>$/.exec(text);
  return match ? { body: match[1] } : null;
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

function parseCodex(lines, stats = null) {
  const reader = codexReader({ stats });
  for (const line of lines) {
    reader.addLine(line);
  }
  return reader;
}

// A Codex rollout read entry by entry: the agent's own, or a subagent's
// (`child`). A subagent's rollout opens with its session_meta (who spawned it,
// its `/root/<task>` path and nickname) and may carry its parent's history
// before `subagent_history_start_ordinal`, which is not its own.
function codexReader({ stats = null, child = false } = {}) {
  const calls = new Map(); // call_id -> message
  const outputLimit = child ? SUBAGENT_OUTPUT_LIMIT : TOOL_OUTPUT_LIMIT;
  let index = 0;
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
    addLine(line) {
      if (!line.trim()) {
        return;
      }
      let entry;
      try {
        entry = JSON.parse(line);
      } catch {
        return;
      }
      add(entry);
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
  function add(entry) {
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
    index += 1;
    const ts = toMillis(entry.timestamp);
    if (entry.type === "compacted") {
      messages.push(compactMessage(`c${index}`, ts, { summary: String(entry.payload?.message || "").trim() }));
      return;
    }
    if (entry.type === "event_msg" && entry.payload?.type === "item_completed") {
      // The thread a spawn_agent call started.
      const item = entry.payload.item;
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
    if (entry.type !== "response_item") {
      return;
    }
    const payload = entry.payload || {};
    const id = `c${index}`;
    if (payload.type === "message") {
      const text = (payload.content || [])
        .map((part) => part?.text || "")
        .join("\n")
        .trim();
      if (!text) {
        return;
      }
      if (payload.role === "user") {
        if (codexTypedText(text)) {
          messages.push({ id: payload.id || id, role: "user", ts, text });
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
        ? live ? "running" : "stopped"
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
        status: sub?.idle ? "done" : live && !sub?.exited ? "running" : "stopped",
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
      const message = typeof entry.message?.content === "string" ? teammateMessage(entry.message.content) : null;
      const head = typeof entry.teamName === "string" && typeof entry.agentName === "string" && message
        ? { team: entry.teamName, agent: entry.agentName, prompt: message.body, ts: toMillis(entry.timestamp) }
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
        state.reader.addLine(buffer.toString("utf8", start, end));
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
