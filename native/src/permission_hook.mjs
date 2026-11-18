// Claude Code `PermissionRequest` hook for agents on the cloud machine, so the
// phone can answer what the terminal would: a tool approval, Claude's questions
// (AskUserQuestion) and a plan (ExitPlanMode).
//
// Claude shows its own dialog WHILE this hook runs, and whichever answers first
// wins (measured on 2.1.302): the phone and the terminal both work. The hook
// writes the request to <asks dir>/<id>.json, where the supervisor lists it to
// the phone, and waits for <id>.answer.json, printed as the hook's decision.
// Answered in the terminal instead, the tool's result lands in the transcript;
// the hook sees it, withdraws the request and exits without a decision (Claude
// does not cancel a hook whose dialog was answered).
//
// Usage: node permission-hook.mjs <asks dir>

import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";

const dir = process.argv[2];
const POLL_MS = 250;
const TAIL_BYTES = 2 * 1024 * 1024;

function readStdin() {
  try {
    return fs.readFileSync(0, "utf8");
  } catch {
    return "";
  }
}

// Lines of the transcript, newest last; only the tail, transcripts get large.
function tailLines(file) {
  try {
    const fd = fs.openSync(file, "r");
    try {
      const size = fs.fstatSync(fd).size;
      const start = Math.max(0, size - TAIL_BYTES);
      const buffer = Buffer.alloc(size - start);
      fs.readSync(fd, buffer, 0, buffer.length, start);
      const lines = buffer.toString("utf8").split("\n");
      if (start > 0) lines.shift();
      return lines;
    } finally {
      fs.closeSync(fd);
    }
  } catch {
    return [];
  }
}

// Key order does not count: the hook gets the input after Claude's schema
// parse, in the schema's key order, while the transcript keeps the model's
// (measured on 2.1.327: `question, header, options, multiSelect` against
// `question, header, multiSelect, options`).
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
  }
  return value;
}

function same(a, b) {
  return JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
}

// Ids of the calls that already have a result in these lines.
function settledIds(lines) {
  const ids = new Set();
  for (const line of lines) {
    if (!line.includes('"tool_result"')) continue;
    for (const match of line.matchAll(/"tool_use_id":"([^"]+)"/g)) ids.add(match[1]);
  }
  return ids;
}

// The pending call's tool_use id: the newest tool_use with this name and input
// that had no result when the hook started. A plan is matched by name alone:
// Claude replaces ExitPlanMode's input with the plan file (`plan`,
// `planFilePath`) before hooks see it, and the transcript keeps the model's `{}`.
function findToolUseId(lines, toolName, toolInput, settled) {
  const anyInput = toolName === "ExitPlanMode";
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const line = lines[i];
    if (!line.includes('"tool_use"') || !line.includes(toolName)) continue;
    let entry;
    try {
      entry = JSON.parse(line);
    } catch {
      continue;
    }
    const content = entry?.message?.content;
    if (!Array.isArray(content)) continue;
    for (let j = content.length - 1; j >= 0; j -= 1) {
      const part = content[j];
      if (part?.type === "tool_use" && part.name === toolName && !settled.has(part.id) && (anyInput || same(part.input, toolInput))) {
        return part.id;
      }
    }
  }
  return null;
}

// A subagent's calls are in its own transcript, under the session's
// `subagents` directory (sometimes one folder deeper), not in transcript_path.
function callTranscript(transcript, agentId) {
  if (!transcript || !agentId) return transcript;
  const root = path.join(transcript.replace(/\.jsonl$/, ""), "subagents");
  const name = `agent-${agentId}.jsonl`;
  try {
    const found = fs.readdirSync(root, { recursive: true }).find((entry) => path.basename(String(entry)) === name);
    return found ? path.join(root, String(found)) : "";
  } catch {
    return "";
  }
}

// Answered somewhere else: the call has a result, or the turn was interrupted
// after the request was made.
function resolvedElsewhere(file, toolUseId, sizeAtStart) {
  let size = 0;
  try {
    size = fs.statSync(file).size;
  } catch {
    return false;
  }
  if (size <= sizeAtStart) return false;
  const fresh = tailLines(file).join("\n");
  if (toolUseId && fresh.includes(`"tool_use_id":"${toolUseId}"`)) return true;
  // Esc on the dialog writes this and ends the turn without a tool_result for
  // calls that never started.
  return /\[Request interrupted by user/.test(lastLines(file, size - sizeAtStart));
}

function lastLines(file, bytes) {
  try {
    const fd = fs.openSync(file, "r");
    try {
      const size = fs.fstatSync(fd).size;
      const length = Math.min(bytes, size, TAIL_BYTES);
      const buffer = Buffer.alloc(length);
      fs.readSync(fd, buffer, 0, length, size - length);
      return buffer.toString("utf8");
    } finally {
      fs.closeSync(fd);
    }
  } catch {
    return "";
  }
}

// The Claude process that ran this hook (through `sh -c`): our parent's parent.
// When it is gone nobody can use our answer, so the hook must not keep polling.
function agentPid() {
  try {
    const raw = execFileSync("ps", ["-o", "ppid=", "-p", String(process.ppid)], { encoding: "utf8" });
    const pid = Number(raw.trim());
    return Number.isInteger(pid) && pid > 1 ? pid : null;
  } catch {
    return null;
  }
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

function main() {
  if (!dir) return;
  let input;
  try {
    input = JSON.parse(readStdin());
  } catch {
    return;
  }
  const transcript = callTranscript(
    typeof input.transcript_path === "string" ? input.transcript_path : "",
    typeof input.agent_id === "string" ? input.agent_id : "",
  );
  let sizeAtStart = 0;
  try {
    sizeAtStart = transcript ? fs.statSync(transcript).size : 0;
  } catch {
    sizeAtStart = 0;
  }
  const id = `${Date.now()}-${randomUUID().slice(0, 8)}`;
  fs.mkdirSync(dir, { recursive: true });
  const requestFile = path.join(dir, `${id}.json`);
  const answerFile = path.join(dir, `${id}.answer.json`);
  const startLines = transcript ? tailLines(transcript) : [];
  const settled = settledIds(startLines);
  let toolUseId = transcript ? findToolUseId(startLines, input.tool_name, input.tool_input, settled) : null;
  const request = {
    id,
    ts: Date.now(),
    pid: process.pid,
    tool: String(input.tool_name || ""),
    input: input.tool_input ?? {},
    suggestions: Array.isArray(input.permission_suggestions) ? input.permission_suggestions : [],
    mode: input.permission_mode || null,
    toolUseId,
  };
  const tmp = `${requestFile}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(request));
  fs.renameSync(tmp, requestFile);

  const cleanup = () => {
    for (const file of [requestFile, answerFile]) {
      try {
        fs.unlinkSync(file);
      } catch {
        // already gone
      }
    }
  };
  for (const signal of ["SIGTERM", "SIGINT", "SIGHUP"]) {
    process.on(signal, () => {
      cleanup();
      process.exit(0);
    });
  }

  const agent = agentPid();
  let checkedSize = sizeAtStart;
  const tick = () => {
    if ((agent && !alive(agent)) || process.ppid === 1) {
      cleanup();
      process.exit(0);
    }
    let answer = null;
    try {
      answer = fs.readFileSync(answerFile, "utf8");
    } catch {
      answer = null;
    }
    if (answer !== null) {
      cleanup();
      // An empty answer means "leave it to the terminal" (a plan approved by
      // key): exit without a decision.
      if (answer.trim()) process.stdout.write(answer);
      process.exit(0);
    }
    // Re-read the transcript only when it grew: reading its 2 MB tail four
    // times a second kept a core busy per waiting hook.
    let size = checkedSize;
    try {
      size = transcript ? fs.statSync(transcript).size : checkedSize;
    } catch {
      size = checkedSize;
    }
    if (transcript && size !== checkedSize) {
      checkedSize = size;
      if (!toolUseId) toolUseId = findToolUseId(tailLines(transcript), input.tool_name, input.tool_input, settled);
      if (resolvedElsewhere(transcript, toolUseId, sizeAtStart)) {
        cleanup();
        process.exit(0);
      }
    }
    setTimeout(tick, POLL_MS);
  };
  tick();
}

main();
