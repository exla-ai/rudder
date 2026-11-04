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

function same(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

// The pending call's tool_use id: the newest tool_use with this name and input.
function findToolUseId(lines, toolName, toolInput) {
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
      if (part?.type === "tool_use" && part.name === toolName && same(part.input, toolInput)) {
        return part.id;
      }
    }
  }
  return null;
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

function main() {
  if (!dir) return;
  let input;
  try {
    input = JSON.parse(readStdin());
  } catch {
    return;
  }
  const transcript = typeof input.transcript_path === "string" ? input.transcript_path : "";
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
  let toolUseId = transcript ? findToolUseId(tailLines(transcript), input.tool_name, input.tool_input) : null;
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

  const tick = () => {
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
    if (transcript) {
      if (!toolUseId) toolUseId = findToolUseId(tailLines(transcript), input.tool_name, input.tool_input);
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
