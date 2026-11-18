// Claude Code transcript adapter: ~/.claude/projects/<cwd-slug>/<session>.jsonl
// and <session>/subagents/*.jsonl. One API response is split across several
// `assistant` records that share a message id and its usage; tool results come
// back as `user` records. Resumed and forked sessions copy earlier records into
// a new file, so callers pass one `seen` set across files to count each API
// response, tool call and attachment once.
import { createReadStream } from "node:fs";
import { basename, dirname } from "node:path";
import { createInterface } from "node:readline";
import { classifySegment, splitShell } from "./classify.ts";
import type { Category, PatchFile, Request, ToolCall, Trace } from "./model.ts";

type Json = Record<string, any>;

// Records that carry no usage, tool or context data, skipped before parsing.
const SKIP = /^\{"type":"(?:api-request-blob|api-request-shape|queue-operation|last-prompt|bridge-session|file-history-snapshot|summary)"/;

export interface ClaudeOptions {
  seen?: Set<string>;
  // Seconds since the epoch; earlier records are ignored.
  since?: number;
}

export async function readClaudeSession(path: string, options: ClaudeOptions = {}): Promise<Trace> {
  const seen = options.seen ?? new Set<string>();
  const since = options.since ?? 0;
  const sub = basename(dirname(path)) === "subagents";
  const trace: Trace = {
    id: basename(path, ".jsonl"),
    backend: "claude",
    agentPath: sub ? `/sub/${basename(path, ".jsonl")}` : "/root",
    parentId: sub ? basename(dirname(dirname(path))) : undefined,
    path,
    startedAt: 0,
    endedAt: 0,
    requests: [],
    compactions: [],
    compactedFrom: [],
    compactSeconds: [],
    contextLoads: [],
    attachments: {},
    prompts: [],
    updates: [],
  };
  const requestStart = new Map<string, number>();
  const byMessage = new Map<string, Request>();
  const calls = new Map<string, ToolCall>();
  let promptId = "";

  const lines = createInterface({ input: createReadStream(path, { encoding: "utf8" }), crlfDelay: Infinity });
  for await (const line of lines) {
    if (SKIP.test(line)) continue;
    let record: Json;
    try { record = JSON.parse(line); } catch { continue; }
    const at = record.timestamp ? Date.parse(record.timestamp) / 1000 : 0;
    if (at && at < since) continue;
    if (at) {
      trace.startedAt ||= at;
      trace.endedAt = at;
    }
    if (!sub && record.sessionId && trace.id !== record.sessionId && record.type !== "api-request") trace.id = record.sessionId;
    trace.cwd ??= record.cwd;
    trace.entrypoint ??= record.entrypoint;
    switch (record.type) {
      case "api-request":
        requestStart.set(record.id, at);
        break;
      case "assistant":
        onAssistant(trace, record, at, { seen, requestStart, byMessage, calls, promptId });
        break;
      case "user":
        promptId = record.promptId ?? promptId;
        onUser(trace, record, at, seen, calls);
        break;
      case "system":
        if (record.subtype === "compact_boundary" && !seen.has(`u:${record.uuid}`)) {
          seen.add(`u:${record.uuid}`);
          trace.compactions.push(trace.requests.length);
          trace.compactedFrom!.push(Number(record.compactMetadata?.preTokens) || 0);
          trace.compactSeconds!.push((Number(record.compactMetadata?.durationMs) || 0) / 1000);
        }
        break;
      case "attachment":
        onAttachment(trace, record, at, seen);
        break;
    }
  }
  return trace;
}

function onAssistant(
  trace: Trace,
  record: Json,
  at: number,
  state: { seen: Set<string>; requestStart: Map<string, number>; byMessage: Map<string, Request>; calls: Map<string, ToolCall>; promptId: string },
) {
  const message: Json = record.message ?? {};
  const id: string = message.id ?? record.uuid;
  if (message.model === "<synthetic>") return;
  let request = state.byMessage.get(id);
  if (!request) {
    if (state.seen.has(`m:${id}`)) return;
    state.seen.add(`m:${id}`);
    const usage: Json = message.usage ?? {};
    const cached = usage.cache_read_input_tokens ?? 0;
    const write = usage.cache_creation_input_tokens ?? 0;
    request = {
      at: state.requestStart.get(record.requestRef) ?? at,
      turnId: state.promptId,
      input: (usage.input_tokens ?? 0) + cached + write,
      cached,
      cacheWrite: write,
      output: usage.output_tokens ?? 0,
      reasoning: 0,
      calls: [],
    };
    state.byMessage.set(id, request);
    trace.requests.push(request);
    trace.model ??= message.model;
    trace.effort ??= record.effort;
  }
  if (state.requestStart.has(record.requestRef)) request.modelSeconds = Math.max(0, at - request.at);
  for (const part of message.content ?? []) {
    if (part?.type === "text" && part.text) trace.updates.push({ at, text: part.text });
    if (part?.type !== "tool_use" || state.calls.has(part.id)) continue;
    const call = parseCall(at, part);
    state.calls.set(part.id, call);
    request.calls.push(call);
  }
}

function onUser(trace: Trace, record: Json, at: number, seen: Set<string>, calls: Map<string, ToolCall>) {
  const content = record.message?.content;
  if (typeof content === "string") {
    if (!record.isMeta && !record.isCompactSummary && !content.startsWith("<") && !seen.has(`u:${record.uuid}`)) {
      seen.add(`u:${record.uuid}`);
      trace.prompts.push({ at, text: content });
    }
    return;
  }
  for (const part of Array.isArray(content) ? content : []) {
    if (part?.type !== "tool_result") continue;
    const call = calls.get(part.tool_use_id);
    if (!call || call.doneAt !== undefined) continue;
    call.doneAt = at;
    call.wallSeconds = Math.max(0, at - call.at);
    call.outputTokens = tokens(resultText(part.content));
    const command = call.commands[0];
    if (command) {
      command.wallSeconds = call.wallSeconds;
      command.outputTokens = call.outputTokens;
      command.exitCode = part.is_error ? 1 : 0;
    }
  }
}

function onAttachment(trace: Trace, record: Json, at: number, seen: Set<string>) {
  const key = `u:${record.uuid}`;
  if (seen.has(key)) return;
  seen.add(key);
  const attachment: Json = record.attachment ?? {};
  const kind: string = attachment.type ?? "unknown";
  const bytes = (record.rendered ?? []).reduce((sum: number, part: Json) => sum + (typeof part?.content === "string" ? part.content.length : 0), 0);
  const entry = (trace.attachments![kind] ??= { count: 0, bytes: 0 });
  entry.count++;
  entry.bytes += bytes;
  if (kind === "instructions" && Array.isArray(attachment.files) && attachment.files.length) {
    trace.contextLoads!.push({
      at,
      files: attachment.files.map((file: Json) => ({
        path: String(file.path ?? ""),
        kind: String(file.type ?? ""),
        bytes: typeof file.content === "string" ? file.content.length : 0,
      })),
    });
  }
}

const READ_TOOLS = new Set(["Read", "Grep", "Glob", "LS", "NotebookRead"]);
const EDIT_TOOLS = new Set(["Edit", "MultiEdit", "Write", "NotebookEdit"]);
const WAIT_TOOLS = new Set(["BashOutput", "TaskOutput", "Monitor", "KillShell", "TaskStop"]);
const COORDINATE_TOOLS = new Set(["Agent", "Task", "SendMessage", "ListAgents", "TeamCreate", "TaskCreate", "TaskUpdate", "TaskList"]);

// The category of a non-shell Claude tool.
export function claudeToolCategory(name: string): Category {
  if (READ_TOOLS.has(name)) return "read";
  if (EDIT_TOOLS.has(name)) return "edit";
  if (WAIT_TOOLS.has(name)) return "wait";
  if (COORDINATE_TOOLS.has(name)) return "coordinate";
  if (name === "WebFetch" || name === "WebSearch") return "web";
  return "other";
}

function parseCall(at: number, part: Json): ToolCall {
  const name: string = part.name ?? "";
  const input: Json = part.input ?? {};
  const call: ToolCall = { at, callId: part.id, tools: [name], commands: [], patches: [], emptyPolls: 0, polled: [], outputTokens: 0 };
  if (name === "Bash") {
    const text = String(input.command ?? "");
    const segments = splitShell(text).map((part) => classifySegment(part.text, part.heredoc));
    call.commands.push({ text, segments, outputTokens: 0, polls: 0 });
  } else if (READ_TOOLS.has(name)) {
    const file = String(input.file_path ?? input.path ?? input.notebook_path ?? "");
    const range = input.offset || input.limit ? `:${input.offset ?? 0}+${input.limit ?? ""}` : "";
    call.commands.push({ text: `${name} ${file}${range}`, segments: [{ text: `${name} ${file}${range}`, category: "read", files: file ? [file] : [], writes: false }], outputTokens: 0, polls: 0 });
  } else if (EDIT_TOOLS.has(name)) {
    call.patches.push(patchOf(name, input));
  }
  return call;
}

function patchOf(name: string, input: Json): PatchFile {
  const path = String(input.file_path ?? input.notebook_path ?? "");
  const lines = (text: unknown) => (typeof text === "string" && text ? text.split("\n").length : 0);
  if (name === "Write") return { path, op: "update", added: lines(input.content), removed: 0 };
  if (name === "MultiEdit") {
    const edits: Json[] = Array.isArray(input.edits) ? input.edits : [];
    return { path, op: "update", added: edits.reduce((sum, edit) => sum + lines(edit.new_string), 0), removed: edits.reduce((sum, edit) => sum + lines(edit.old_string), 0) };
  }
  return { path, op: "update", added: lines(input.new_string ?? input.new_source), removed: lines(input.old_string) };
}

function resultText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  // Images count by their encoded size; the API bills them separately, but they
  // stay in context like text.
  return content.map((part: Json) => (typeof part?.text === "string" ? part.text : typeof part?.source?.data === "string" ? part.source.data.slice(0, 6000) : "")).join("\n");
}

// The rough 4-bytes-per-token rule; good enough to rank outputs.
export function tokens(text: string): number {
  return Math.ceil(text.length / 4);
}
