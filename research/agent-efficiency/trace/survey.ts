// Fleet survey: every Claude Code and Codex session touched in a date window,
// one row per session plus ranked waste totals. Rows carry numbers, roles and
// project names only, never prompts, outputs or command text, but they still
// describe private work: write them outside the repository.
//
// node research/agent-efficiency/trace/survey.ts --since 2026-10-28 --out DIR
//   [--until 2026-11-18] [--claude ~/.claude/projects] [--codex ~/.codex/sessions]
import { mkdirSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { readClaudeSession, claudeToolCategory } from "./claude.ts";
import { programOf, signature } from "./classify.ts";
import { readCodexRollout } from "./codex.ts";
import type { Trace } from "./model.ts";

// A gap longer than this between requests is the agent idling, not working.
const IDLE_SECONDS = 300;
// One tool output this large is a flood: it rides along on every later request.
const BIG_OUTPUT_TOKENS = 8_000;
const FULL_SUITE = /^(?:npm|pnpm|yarn)\s+(?:run\s+)?test(?::tui)?\s*$|^cargo\s+test\s*(?:--workspace|--all)?\s*$|^cargo\s+test\s+--workspace\b|^npm\s+run\s+test:tui\b/;

export interface Row {
  id: string;
  backend: string;
  role: string;
  project: string;
  model: string;
  effort: string;
  entrypoint: string;
  start: string;
  hours: { wall: number; active: number; model: number; tool: number };
  requests: number;
  tokens: { input: number; cached: number; cacheWrite: number; output: number; reasoning: number; maxContext: number; above150k: number };
  compactions: number;
  compactedFrom: number;
  compactSeconds: number;
  // Prompt-cache writes by cause (Claude): the first request after a
  // compaction, the first of a session or resume, after an idle gap longer than
  // the one-hour cache, and ordinary new turn content.
  writes: { afterCompaction: number; start: number; idle: number; turn: number };
  tools: Record<string, number>;
  categories: Record<string, { calls: number; seconds: number; outputTokens: number }>;
  outputs: { tokens: number; carried: number; big: number; bigTokens: number; bigCarried: number };
  rereads: { calls: number; tokens: number; carried: number };
  sleeps: { calls: number; seconds: number };
  repeats: { calls: number; seconds: number };
  failedRepeats: number;
  fullSuites: { calls: number; seconds: number };
  instructions: { loads: number; bytesPerLoad: number; outerBytesPerLoad: number; duplicateBytesPerLoad: number; carried: number; outerCarried: number; duplicateCarried: number };
  attachments: Record<string, { count: number; bytes: number }>;
}

export function roleOf(trace: Trace): string {
  const cwd = trace.cwd ?? "";
  if (trace.parentId) return "subagent";
  if (cwd.includes("/.rudder-workspaces/")) return "worker";
  if (cwd.includes("/.rudder/")) return "helper";
  if (/rudder-e2e|rudder-board-scheduler|claude-worktrees|rudder-scratch|\/scratch\//.test(cwd)) return "test-or-scratch";
  if (cwd === "/tmp" || cwd.startsWith("/tmp/")) return trace.entrypoint === "sdk-cli" || trace.entrypoint === "codex_exec" ? "probe" : "tmp";
  if (/^\/workspace\/code\/[^/]+(\/|$)/.test(cwd)) return "main";
  return "other";
}

export function projectOf(cwd = ""): string {
  return /^\/workspace\/code\/([^/]+)/.exec(cwd)?.[1] ?? (cwd.startsWith("/tmp") ? "tmp" : "other");
}

// The checkout a Rudder workspace lives in, when the workspace is nested inside it.
function outerRoot(cwd = ""): string | null {
  const at = cwd.indexOf("/.rudder-workspaces/");
  return at === -1 ? null : cwd.slice(0, at);
}

export function summarize(trace: Trace): Row {
  const requests = trace.requests;
  const nextBoundary = (index: number) => trace.compactions.find((at) => at > index) ?? requests.length;
  const tools: Record<string, number> = {};
  const categories: Row["categories"] = {};
  const outputs = { tokens: 0, carried: 0, big: 0, bigTokens: 0, bigCarried: 0 };
  const rereads = { calls: 0, tokens: 0, carried: 0 };
  const sleeps = { calls: 0, seconds: 0 };
  const repeats = { calls: 0, seconds: 0 };
  const fullSuites = { calls: 0, seconds: 0 };
  let failedRepeats = 0;
  const failed = new Set<string>();
  // File (with range) -> request index of its last read, cleared by an edit.
  const lastRead = new Map<string, number>();
  let lastSig = "";
  let active = 0;
  let tool = 0;
  let modelTime = 0;
  let above150k = 0;
  let maxContext = 0;
  let compactionIndex = 0;
  const writes = { afterCompaction: 0, start: 0, idle: 0, turn: 0 };
  const afterCompaction = new Set(trace.compactions);

  requests.forEach((request, index) => {
    while (compactionIndex < trace.compactions.length && trace.compactions[compactionIndex] <= index) {
      compactionIndex++;
      lastRead.clear();
    }
    const written = request.cacheWrite ?? 0;
    const sincePrevious = index ? request.at - requests[index - 1].at : Infinity;
    if (index === 0) writes.start += written;
    else if (afterCompaction.has(index)) writes.afterCompaction += written;
    else if (sincePrevious > 3600) writes.idle += written;
    else writes.turn += written;
    maxContext = Math.max(maxContext, request.input);
    above150k += Math.max(0, request.input - 150_000);
    const next = requests[index + 1];
    const doneAt = Math.max(request.at, ...request.calls.map((call) => call.doneAt ?? request.at));
    const gap = next ? Math.min(IDLE_SECONDS, Math.max(0, next.at - request.at)) : Math.min(IDLE_SECONDS, Math.max(0, doneAt - request.at));
    const blocked = Math.min(gap, Math.max(0, doneAt - request.at));
    active += gap;
    tool += blocked;
    modelTime += request.modelSeconds ?? Math.max(0, gap - blocked);
    const span = Math.max(0, nextBoundary(index) - index - 1);
    for (const call of request.calls) {
      const name = call.tools[0] ?? "?";
      tools[name] = (tools[name] ?? 0) + 1;
      const category = call.commands[0]?.segments.length
        ? dominantOf(call.commands[0].segments.map((segment) => segment.category))
        : trace.backend === "claude" ? claudeToolCategory(name) : call.patches.length ? "edit" : "other";
      const entry = (categories[category] ??= { calls: 0, seconds: 0, outputTokens: 0 });
      entry.calls++;
      entry.seconds += Math.min(call.wallSeconds ?? 0, 3600);
      entry.outputTokens += call.outputTokens;
      outputs.tokens += call.outputTokens;
      outputs.carried += call.outputTokens * span;
      if (call.outputTokens >= BIG_OUTPUT_TOKENS) {
        outputs.big++;
        outputs.bigTokens += call.outputTokens;
        outputs.bigCarried += call.outputTokens * span;
      }
      for (const patch of call.patches) for (const key of [...lastRead.keys()]) if (key.startsWith(patch.path)) lastRead.delete(key);
      for (const command of call.commands) {
        const sig = signature(command.text);
        const seconds = Math.min(command.wallSeconds ?? 0, 3600);
        if (sig && sig === lastSig) {
          repeats.calls++;
          repeats.seconds += seconds;
        }
        lastSig = sig;
        if (command.exitCode) {
          if (failed.has(sig)) failedRepeats++;
          failed.add(sig);
        }
        for (const segment of command.segments) {
          const { prog, args } = programOf(segment.text);
          if (prog === "sleep") {
            sleeps.calls++;
            sleeps.seconds += Number.parseFloat(args[0] ?? "0") || 0;
          }
          if (FULL_SUITE.test(segment.text.trim())) {
            fullSuites.calls++;
            fullSuites.seconds += seconds;
          }
          if (segment.writes) for (const file of segment.files) for (const key of [...lastRead.keys()]) if (key.startsWith(file)) lastRead.delete(key);
        }
        // A read of the same file and range with no edit or compaction since.
        const read = command.segments.length === 1 && command.segments[0].category === "read" && command.segments[0].files.length === 1
          ? `${command.segments[0].files[0]}|${command.text.replace(/\s+/g, " ")}` : "";
        if (read) {
          if (lastRead.has(read)) {
            rereads.calls++;
            rereads.tokens += command.outputTokens;
            rereads.carried += command.outputTokens * span;
          }
          lastRead.set(read, index);
        }
      }
    }
  });

  // Instruction loads: each one rides on every request until the next load.
  const loads = trace.contextLoads ?? [];
  const outer = outerRoot(trace.cwd);
  const instructions = { loads: loads.length, bytesPerLoad: 0, outerBytesPerLoad: 0, duplicateBytesPerLoad: 0, carried: 0, outerCarried: 0, duplicateCarried: 0 };
  loads.forEach((load, n) => {
    const from = requests.findIndex((request) => request.at >= load.at);
    const until = n + 1 < loads.length ? requests.findIndex((request) => request.at >= loads[n + 1].at) : requests.length;
    const carriedRequests = from === -1 ? 0 : Math.max(0, (until === -1 ? requests.length : until) - from);
    const total = load.files.reduce((sum, file) => sum + file.bytes, 0);
    const outerBytes = outer ? load.files.filter((file) => file.kind === "Project" && dirOf(file.path) === outer).reduce((sum, file) => sum + file.bytes, 0) : 0;
    const sizes = new Map<number, number>();
    let duplicate = 0;
    for (const file of load.files) {
      if (file.bytes > 0 && sizes.has(file.bytes)) duplicate += file.bytes;
      sizes.set(file.bytes, (sizes.get(file.bytes) ?? 0) + 1);
    }
    instructions.bytesPerLoad += total / loads.length;
    instructions.outerBytesPerLoad += outerBytes / loads.length;
    instructions.duplicateBytesPerLoad += duplicate / loads.length;
    instructions.carried += (total / 4) * carriedRequests;
    instructions.outerCarried += (outerBytes / 4) * carriedRequests;
    instructions.duplicateCarried += (duplicate / 4) * carriedRequests;
  });

  const sum = (pick: (request: Trace["requests"][number]) => number) => requests.reduce((total, request) => total + pick(request), 0);
  const compactedFrom = trace.compactedFrom?.filter(Boolean) ?? [];
  return {
    id: trace.id,
    backend: trace.backend,
    role: roleOf(trace),
    project: projectOf(trace.cwd),
    model: trace.model ?? "",
    effort: trace.effort ?? "",
    entrypoint: trace.entrypoint ?? "",
    start: new Date((requests[0]?.at ?? trace.startedAt) * 1000).toISOString(),
    hours: { wall: (trace.endedAt - trace.startedAt) / 3600, active: active / 3600, model: modelTime / 3600, tool: tool / 3600 },
    requests: requests.length,
    tokens: { input: sum((r) => r.input), cached: sum((r) => r.cached), cacheWrite: sum((r) => r.cacheWrite ?? 0), output: sum((r) => r.output), reasoning: sum((r) => r.reasoning), maxContext, above150k },
    compactions: trace.compactions.length,
    compactedFrom: compactedFrom.length ? compactedFrom.reduce((a, b) => a + b, 0) / compactedFrom.length : 0,
    compactSeconds: (trace.compactSeconds ?? []).reduce((a, b) => a + b, 0),
    writes,
    tools,
    categories,
    outputs,
    rereads,
    sleeps,
    repeats,
    failedRepeats,
    fullSuites,
    instructions,
    attachments: trace.attachments ?? {},
  };
}

function dirOf(path: string): string {
  return path.slice(0, path.lastIndexOf("/"));
}

const RANK = ["edit", "release", "test", "build", "cloud", "ci", "vcs", "coordinate", "web", "read", "wait", "other"];
function dominantOf(categories: string[]): string {
  const meaningful = categories.filter((category) => category !== "other");
  const pool = meaningful.length ? meaningful : categories;
  return RANK.find((category) => pool.includes(category)) ?? "other";
}

function walk(dir: string, since: number, until: number, out: { path: string; mtime: number; size: number }[]) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, since, until, out);
    else if (entry.name.endsWith(".jsonl")) {
      const info = statSync(path);
      if (info.mtimeMs / 1000 >= since && info.mtimeMs / 1000 < until + 86_400) out.push({ path, mtime: info.mtimeMs, size: info.size });
    }
  }
}

async function main() {
  const args = process.argv.slice(2);
  const flag = (name: string) => {
    const at = args.indexOf(name);
    return at === -1 ? undefined : args[at + 1];
  };
  const sinceText = flag("--since");
  const out = flag("--out");
  if (!sinceText || !out) {
    console.error("usage: survey.ts --since YYYY-MM-DD --out DIR [--until YYYY-MM-DD] [--claude DIR] [--codex DIR]");
    process.exit(2);
  }
  const since = Date.parse(`${sinceText}T00:00:00Z`) / 1000;
  const untilText = flag("--until");
  const until = untilText ? Date.parse(`${untilText}T00:00:00Z`) / 1000 : Date.now() / 1000;
  const claudeDir = flag("--claude") ?? join(homedir(), ".claude", "projects");
  const codexDir = flag("--codex") ?? join(homedir(), ".codex", "sessions");
  mkdirSync(out, { recursive: true });

  const claudeFiles: { path: string; mtime: number; size: number }[] = [];
  const codexFiles: { path: string; mtime: number; size: number }[] = [];
  walk(claudeDir, since, until, claudeFiles);
  walk(codexDir, since, until, codexFiles);
  // Oldest first, so a fork's copied history counts in the session it came from.
  claudeFiles.sort((a, b) => a.mtime - b.mtime);
  const rows: Row[] = [];
  const seen = new Set<string>();
  let done = 0;
  for (const file of claudeFiles) {
    const trace = await readClaudeSession(file.path, { seen, since });
    if (trace.requests.length && trace.requests[0].at < until) rows.push(summarize(trace));
    if (++done % 500 === 0) console.error(`claude ${done}/${claudeFiles.length}`);
  }
  const codexSeen = new Set<string>();
  for (const file of codexFiles) {
    const trace = await readCodexRollout(file.path);
    if (codexSeen.has(trace.id)) continue;
    codexSeen.add(trace.id);
    trace.requests = trace.requests.filter((request) => request.at >= since && request.at < until + 86_400);
    if (trace.requests.length) rows.push(summarize(trace));
  }
  writeFileSync(join(out, "sessions.jsonl"), rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
  console.error(`wrote ${rows.length} session rows from ${claudeFiles.length} Claude and ${codexFiles.length} Codex files`);
}

if (process.argv[1]?.endsWith("survey.ts")) await main();
