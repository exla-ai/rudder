import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import { runDir } from "./state.js";
import type { RunRecord } from "./types.js";

/**
 * Per-run timeline: `.rudder/runs/<id>/timeline.jsonl`, append-only JSON lines.
 * `workspace` events carry workspace startup ms; `summary` events (written by
 * the native dashboard from the backend session log) carry where the rest of
 * the time and tokens went. Readers take the latest of each.
 */
export type RunTimeline = {
  startMs?: number;
  installMs: number;
  buildMs: number;
  testMs: number;
  toolMs: number;
  waitMs: number;
  modelMs: number;
  input: number;
  cached: number;
  /** Headless runs know only the input total, not how much was cached. */
  cacheUnknown?: boolean;
  /** No per-phase summary was recorded (headless runs): show only what is known. */
  phasesUnknown?: boolean;
  output: number;
};

export function timelinePath(repoRoot: string, runId: string): string {
  return path.join(runDir(repoRoot, runId), "timeline.jsonl");
}

/** Best effort: timing telemetry never fails a run. */
export async function appendTimelineEvent(repoRoot: string, runId: string, event: Record<string, unknown>): Promise<void> {
  try {
    await fsp.mkdir(runDir(repoRoot, runId), { recursive: true });
    await fsp.appendFile(timelinePath(repoRoot, runId), `${JSON.stringify({ ...event, ts: new Date().toISOString() })}\n`);
  } catch {
    // ignore
  }
}

const num = (value: unknown): number => (typeof value === "number" && Number.isFinite(value) ? value : 0);

export function parseTimeline(content: string, run?: Pick<RunRecord, "tokens">): RunTimeline | undefined {
  let startMs: number | undefined;
  let summary: Record<string, unknown> | undefined;
  for (const line of content.split("\n")) {
    if (!line.trim()) continue;
    let value: any;
    try {
      value = JSON.parse(line);
    } catch {
      continue;
    }
    if (value?.kind === "workspace" && typeof value.ms === "number") startMs = value.ms;
    if (value?.kind === "summary") summary = value;
  }
  if (!summary && startMs === undefined && !run?.tokens) return undefined;
  const timeline: RunTimeline = {
    installMs: num(summary?.installMs),
    buildMs: num(summary?.buildMs),
    testMs: num(summary?.testMs),
    toolMs: num(summary?.toolMs),
    waitMs: num(summary?.waitMs),
    modelMs: num(summary?.modelMs),
    input: num(summary?.input),
    cached: num(summary?.cached),
    output: num(summary?.output),
  };
  if (startMs !== undefined) timeline.startMs = startMs;
  if (!summary) timeline.phasesUnknown = true;
  // Headless runs record only run.tokens (input includes cache reads).
  if (!summary && run?.tokens) {
    timeline.input = run.tokens.input;
    timeline.cacheUnknown = true;
    timeline.output = run.tokens.output;
  }
  return timeline;
}

export function readRunTimeline(repoRoot: string, run: RunRecord): RunTimeline | undefined {
  let content = "";
  try {
    content = fs.readFileSync(timelinePath(repoRoot, run.id), "utf8");
  } catch {
    // no timeline yet
  }
  return parseTimeline(content, run);
}

export function formatDuration(ms: number): string {
  const secs = ms / 1000;
  if (secs < 10) {
    const s = `${secs.toFixed(1)}s`;
    return s === "0.0s" ? "0s" : s;
  }
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) {
    const m = Math.floor(secs / 60);
    const s = Math.round(secs - m * 60);
    return s === 0 ? `${m}m` : `${m}m${String(s).padStart(2, "0")}s`;
  }
  const h = Math.floor(secs / 3600);
  return `${h}h${String(Math.round((secs - h * 3600) / 60)).padStart(2, "0")}m`;
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Same shape as the native dashboard line (native/src/timeline.rs). */
export function formatTimeline(t: RunTimeline): string {
  const parts: string[] = [];
  if (t.startMs !== undefined) parts.push(`start ${formatDuration(t.startMs)}`);
  if (!t.phasesUnknown) {
    parts.push(`install ${formatDuration(t.installMs)}`, `build ${formatDuration(t.buildMs)}`, `test ${formatDuration(t.testMs)}`);
    if (t.toolMs > 0) parts.push(`tools ${formatDuration(t.toolMs)}`);
    parts.push(`model ${formatDuration(t.modelMs)}`);
  }
  if (t.waitMs > 0) parts.push(`wait ${formatDuration(t.waitMs)}`);
  const totalIn = t.input + t.cached;
  const total = totalIn + t.output;
  if (total > 0) {
    const pct = totalIn > 0 ? Math.floor((t.cached * 100) / totalIn) : 0;
    parts.push(t.cacheUnknown ? `${formatTokens(total)} tok` : `${formatTokens(total)} tok, ${pct}% cached`);
  }
  return parts.join(" · ");
}
