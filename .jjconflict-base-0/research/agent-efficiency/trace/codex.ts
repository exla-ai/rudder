// Codex rollout adapter: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl.
// Reasoning and inter-agent messages are encrypted in the rollout, so this
// reads only what is in the clear: tool calls, outputs, token usage, prompts
// and progress updates.
import { createReadStream, openSync, readSync, closeSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { classifySegment, splitShell } from "./classify.ts";
import type { Command, PatchFile, Request, ToolCall, Trace } from "./model.ts";

type Running = Map<number, { command: Command; startAt: number }>;

type Json = Record<string, any>;

const LITERAL = /"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`/;
const CMD = new RegExp(String.raw`(?:"cmd"|\bcmd)\s*:\s*(` + LITERAL.source + ")");

export async function readCodexRollout(path: string): Promise<Trace> {
  const trace: Trace = { id: "", agentPath: "/root", path, startedAt: 0, endedAt: 0, requests: [], compactions: [], prompts: [], updates: [] };
  const calls = new Map<string, ToolCall>();
  const running = new Map<number, { command: Command; startAt: number }>();
  let pending: ToolCall[] = [];
  let sawUsageRecord = false;
  let turnId = "";

  const close = (at: number, usage: Json | undefined) => {
    const request: Request = {
      at,
      turnId,
      input: usage?.input_tokens ?? 0,
      cached: usage?.cached_input_tokens ?? 0,
      output: usage?.output_tokens ?? 0,
      reasoning: usage?.reasoning_output_tokens ?? 0,
      calls: pending,
    };
    trace.requests.push(request);
    pending = [];
  };

  const lines = createInterface({ input: createReadStream(path, { encoding: "utf8" }), crlfDelay: Infinity });
  for await (const line of lines) {
    let record: Json;
    try { record = JSON.parse(line); } catch { continue; }
    const at = record.timestamp ? Date.parse(record.timestamp) / 1000 : trace.endedAt;
    if (at) {
      trace.startedAt ||= at;
      trace.endedAt = at;
    }
    const payload: Json = record.payload && typeof record.payload === "object" ? record.payload : {};
    switch (record.type) {
      case "session_meta":
        // Forked histories replay their parent's meta later in the file.
        if (trace.id) break;
        trace.id = payload.id ?? "";
        trace.agentPath = payload.agent_path ?? "/root";
        trace.parentId = payload.parent_thread_id;
        trace.cwd = payload.cwd;
        break;
      case "turn_context":
        trace.model ??= payload.model;
        turnId = payload.turn_id ?? turnId;
        break;
      case "compacted":
        trace.compactions.push(trace.requests.length);
        break;
      case "token_usage_record":
        sawUsageRecord = true;
        turnId = payload.turn_id ?? turnId;
        close(at, payload.usage);
        break;
      case "event_msg":
        if (payload.type === "token_count") {
          if (!sawUsageRecord && payload.info?.last_token_usage) close(at, payload.info.last_token_usage);
          noteCredits(trace, payload.rate_limits);
        }
        break;
      case "response_item":
        if (payload.type === "message") noteMessage(trace, at, payload);
        else if (payload.type === "custom_tool_call" || payload.type === "function_call") {
          const call = parseCall(at, payload);
          calls.set(call.callId, call);
          pending.push(call);
        } else if (payload.type === "custom_tool_call_output" || payload.type === "function_call_output") {
          const call = calls.get(payload.call_id);
          if (call) applyOutput(call, payload.output, at, running);
        }
        break;
    }
  }
  if (pending.length) close(trace.endedAt, undefined);
  return trace;
}

function noteMessage(trace: Trace, at: number, payload: Json) {
  const text = (payload.content ?? []).map((part: Json) => part?.text ?? "").join(" ").trim();
  if (!text) return;
  if (payload.role === "user" && !text.startsWith("<") && !text.startsWith("# ")) trace.prompts.push({ at, text });
  if (payload.role === "assistant") trace.updates.push({ at, text });
}

function noteCredits(trace: Trace, limits: Json | undefined) {
  const balance = Number(limits?.credits?.balance);
  const used = Number(limits?.primary?.used_percent);
  if (!Number.isFinite(balance) || !Number.isFinite(used)) return;
  trace.credits ??= { first: balance, last: balance, windowFirst: used, windowLast: used };
  trace.credits.last = balance;
  trace.credits.windowLast = used;
}

function parseCall(at: number, payload: Json): ToolCall {
  const call: ToolCall = { at, callId: payload.call_id ?? payload.id ?? "", tools: [], commands: [], patches: [], emptyPolls: 0, polled: [], outputTokens: 0 };
  const name: string = payload.name ?? "";
  const input: string = payload.input ?? payload.arguments ?? "";
  if (payload.type === "custom_tool_call" && name === "exec") {
    parseExecScript(call, input);
  } else if (name === "apply_patch") {
    call.tools.push(name);
    call.patches.push(...parsePatch(typeof input === "string" && input.includes("*** Begin Patch") ? input : argField(input, "input")));
  } else if (name === "exec_command" || name === "shell" || name === "local_shell") {
    call.tools.push("exec_command");
    const args = parseJson(input);
    const cmd = Array.isArray(args?.command) ? args.command.join(" ") : args?.cmd ?? args?.command ?? "";
    call.commands.push(command(String(cmd)));
  } else {
    call.tools.push(name);
    if (name === "write_stdin") {
      const args = parseJson(input);
      if (!args?.chars) call.emptyPolls++;
      call.polled.push(Number(args?.session_id ?? -1));
    }
  }
  return call;
}

// The Codex `exec` tool runs JavaScript that calls nested tools, so one model
// request can run several shell commands, patches and polls.
function parseExecScript(call: ToolCall, script: string) {
  const starts = [...script.matchAll(/tools\.([A-Za-z0-9_]+)\(/g)];
  starts.forEach((match, n) => {
    const tool = match[1];
    const body = script.slice(match.index! + match[0].length, starts[n + 1]?.index ?? script.length);
    call.tools.push(tool);
    if (tool === "exec_command") {
      const cmd = CMD.exec(body);
      call.commands.push(command(cmd ? decode(cmd[1]) : "<dynamic>"));
    } else if (tool === "apply_patch") {
      const literal = LITERAL.exec(body);
      if (literal) call.patches.push(...parsePatch(decode(literal[0])));
    } else if (tool === "write_stdin") {
      if (/\bchars"?\s*:\s*(""|''|``)/.test(body)) call.emptyPolls++;
      call.polled.push(Number(/session_id"?\s*:\s*(\d+)/.exec(body)?.[1] ?? -1));
    }
  });
}

function command(text: string): Command {
  const segments = splitShell(text).map((part) => classifySegment(part.text, part.heredoc));
  return { text, segments, outputTokens: 0, polls: 0 };
}

function applyOutput(call: ToolCall, output: unknown, at: number, running: Running) {
  const texts = Array.isArray(output) ? output.map((part: Json) => String(part?.text ?? "")) : [typeof output === "string" ? output : JSON.stringify(output ?? "")];
  call.doneAt = at;
  call.outputTokens = Math.ceil(texts.reduce((sum, text) => sum + text.length, 0) / 4);
  const wall = /Wall time:?\s*([\d.]+)\s*s/.exec(texts[0] ?? "");
  if (wall) call.wallSeconds = Number(wall[1]);
  // Nested exec results arrive as JSON in the order the script printed them,
  // sometimes wrapped by Promise.allSettled.
  const results = texts.flatMap(execResults);
  const slots = call.tools.filter((tool) => tool === "exec_command" || tool === "write_stdin");
  let commandIndex = 0;
  let pollIndex = 0;
  results.forEach((result, n) => {
    const tokens = Math.ceil(String(result.output ?? "").length / 4);
    const exit = typeof result.exit_code === "number" ? result.exit_code : undefined;
    if (slots[n] === "exec_command") {
      const target = call.commands[commandIndex++];
      if (!target) return;
      target.outputTokens = tokens;
      target.exitCode = exit;
      // A nested result's wall_time_seconds covers only its final read. The
      // script's own wall time is reliable when it ran a single command.
      if (exit !== undefined && call.commands.length === 1) target.wallSeconds = call.wallSeconds;
      if (exit === undefined && typeof result.session_id === "number") running.set(result.session_id, { command: target, startAt: call.at });
    } else if (slots[n] === "write_stdin") {
      const process = running.get(call.polled[pollIndex++]);
      if (!process) return;
      process.command.polls++;
      process.command.outputTokens += tokens;
      if (exit !== undefined) {
        process.command.exitCode = exit;
        process.command.wallSeconds = Math.max(0, at - process.startAt);
        running.delete(call.polled[pollIndex - 1]);
      }
    }
  });
  if (call.commands.length === 1 && call.commands[0].exitCode === undefined && !results.length) {
    const code = /exit(?:ed with)? code:?\s*(\d+)/i.exec(texts.join("\n"));
    if (code) call.commands[0].exitCode = Number(code[1]);
    call.commands[0].outputTokens ||= call.outputTokens;
  }
}

function execResults(text: string): Json[] {
  const trimmed = text.trimStart();
  if (!trimmed.startsWith("{") && !trimmed.startsWith("[")) return [];
  let parsed: unknown;
  try { parsed = JSON.parse(trimmed); } catch { return []; }
  const items = Array.isArray(parsed) ? parsed : [parsed];
  return items
    .map((item: Json) => (item && typeof item.value === "object" && item.value ? item.value : item))
    .filter((item: Json) => item && typeof item.wall_time_seconds === "number");
}

export function parsePatch(patch: string): PatchFile[] {
  const files: PatchFile[] = [];
  let current: PatchFile | undefined;
  for (const line of patch.split("\n")) {
    const header = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(line);
    if (header) {
      current = { path: header[2].trim(), op: header[1].toLowerCase() as PatchFile["op"], added: 0, removed: 0 };
      files.push(current);
    } else if (current && line.startsWith("+")) current.added++;
    else if (current && line.startsWith("-")) current.removed++;
  }
  return files;
}

function decode(literal: string): string {
  const quote = literal[0];
  const body = literal.slice(1, -1);
  try {
    if (quote === '"') return JSON.parse(literal);
    if (quote === "'") return JSON.parse('"' + body.replace(/\\'/g, "'").replace(/"/g, '\\"') + '"');
  } catch {
    return body;
  }
  return body.replace(/\\`/g, "`").replace(/\\\\/g, "\\");
}

function parseJson(text: unknown): Json | undefined {
  if (typeof text !== "string" || !text.startsWith("{")) return undefined;
  try { return JSON.parse(text); } catch { return undefined; }
}

function argField(input: string, field: string): string {
  return String(parseJson(input)?.[field] ?? "");
}

// Every rollout in the same Codex session tree shares the root session_id.
export function findSessionTree(sessionsDir: string, idOrPath: string): string[] {
  const files = listRollouts(sessionsDir);
  const metas = new Map<string, Json>();
  for (const file of files) {
    const meta = firstRecord(file)?.payload;
    if (meta) metas.set(file, meta);
  }
  const root = idOrPath.endsWith(".jsonl") ? metas.get(idOrPath)?.session_id ?? firstRecord(idOrPath)?.payload?.session_id : idOrPath;
  if (!root) return [];
  return files.filter((file) => {
    const meta = metas.get(file);
    return meta && (meta.session_id === root || meta.id === root);
  });
}

function listRollouts(dir: string): string[] {
  const out: string[] = [];
  let entries: string[] = [];
  try { entries = readdirSync(dir); } catch { return out; }
  for (const entry of entries) {
    const full = join(dir, entry);
    let isDir = false;
    try { isDir = statSync(full).isDirectory(); } catch { continue; }
    if (isDir) out.push(...listRollouts(full));
    else if (entry.startsWith("rollout-") && entry.endsWith(".jsonl")) out.push(full);
  }
  return out.sort();
}

function firstRecord(file: string): Json | undefined {
  const fd = openSync(file, "r");
  try {
    const chunks: Buffer[] = [];
    const buffer = Buffer.alloc(1 << 16);
    for (let offset = 0, total = 0; total < 8 << 20; ) {
      const read = readSync(fd, buffer, 0, buffer.length, offset);
      if (!read) break;
      const newline = buffer.subarray(0, read).indexOf(10);
      chunks.push(Buffer.from(buffer.subarray(0, newline === -1 ? read : newline)));
      if (newline !== -1) break;
      offset += read;
      total += read;
    }
    return parseJson(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return undefined;
  } finally {
    closeSync(fd);
  }
}
