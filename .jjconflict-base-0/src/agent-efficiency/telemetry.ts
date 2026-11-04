import { evidenceId, finiteMetric, label } from "./privacy.js";
import { boundedRows, editEvidence } from "./storage.js";
import type { JsonValue } from "../types.js";

export type Provider = "codex" | "claude";
export type Tokens = Partial<Record<"input" | "cachedInput" | "cacheWrite" | "output" | "reasoning", number>>;
export type StepObservation = {
  projectId: string; runId: string; stepId: string; provider: Provider;
  model?: string; effort?: string; tool?: string; tokens?: Tokens;
  contextTokens?: number; modelMs?: number; toolMs?: number; outputBytes?: number;
  durationMs?: number; waitMs?: number; coordinationMs?: number; retries?: number;
  failure?: string;
};
export type StepTelemetry = {
  schema: 1; project: string; run: string; step: string; provider: Provider;
  model?: string; effort?: string; tool?: string; tokens?: Tokens; failure?: string;
  contextTokens?: number; modelMs?: number; toolMs?: number; outputBytes?: number;
  durationMs?: number; waitMs?: number; coordinationMs?: number; retries?: number;
  waitShare?: number; coordinationShare?: number;
};

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
}

/** Per-step fields only. Cumulative totals must never be charged to each step. */
export function supportedUsage(provider: Provider, event: unknown): { tokens?: Tokens; contextTokens?: number; model?: string; effort?: string } {
  const row = record(event);
  if (!row) return {};
  let usage: Record<string, unknown> | undefined;
  let contextTokens: number | undefined;
  let model: string | undefined;
  if (provider === "codex") {
    const payload = record(row.payload);
    if (row.type !== "event_msg" || payload?.type !== "token_count") return {};
    const info = record(payload.info);
    usage = record(info?.last_token_usage);
    // model_context_window is capacity, not observed context occupancy.
    contextTokens = finiteMetric(usage?.input_tokens);
  } else {
    if (row.type !== "assistant") return {};
    const message = record(row.message);
    usage = record(message?.usage);
    model = label(message?.model);
    // Claude input_tokens excludes cache reads/writes. Only sum if all are known.
    const input = finiteMetric(usage?.input_tokens);
    const read = finiteMetric(usage?.cache_read_input_tokens);
    const write = finiteMetric(usage?.cache_creation_input_tokens);
    if (input !== undefined && read !== undefined && write !== undefined) contextTokens = input + read + write;
  }
  if (!usage) return {};
  const tokens: Tokens = {};
  const fields = provider === "codex"
    ? { input: "input_tokens", cachedInput: "cached_input_tokens", output: "output_tokens", reasoning: "reasoning_output_tokens" }
    : { input: "input_tokens", cachedInput: "cache_read_input_tokens", cacheWrite: "cache_creation_input_tokens", output: "output_tokens" };
  for (const [target, source] of Object.entries(fields)) {
    const value = finiteMetric(usage[source]);
    if (value !== undefined && Number.isSafeInteger(value)) tokens[target as keyof Tokens] = value;
  }
  return { ...(Object.keys(tokens).length ? { tokens } : {}), ...(contextTokens !== undefined ? { contextTokens } : {}), ...(model ? { model } : {}) };
}

/** Allowlisting is the primary redaction: no prompt, command, output or error text. */
export function normalizeStep(observation: StepObservation): StepTelemetry {
  if (!["codex", "claude"].includes(observation.provider)) throw new Error("unsupported_telemetry_provider");
  const result: StepTelemetry = { schema: 1, project: evidenceId(observation.projectId), run: evidenceId(observation.runId), step: evidenceId(observation.stepId), provider: observation.provider };
  for (const key of ["model", "tool"] as const) {
    const value = label(observation[key]);
    if (value) result[key] = value;
  }
  if (["low", "medium", "high", "xhigh", "max"].includes(observation.effort ?? "")) result.effort = observation.effort;
  for (const key of ["contextTokens", "modelMs", "toolMs", "outputBytes", "durationMs", "waitMs", "coordinationMs", "retries"] as const) {
    const value = finiteMetric(observation[key]);
    if (value !== undefined) result[key] = value;
  }
  if (observation.failure) result.failure = evidenceId(observation.failure);
  if (observation.tokens) {
    const tokens: Tokens = {};
    for (const key of ["input", "cachedInput", "cacheWrite", "output", "reasoning"] as const) {
      const value = finiteMetric(observation.tokens[key]);
      if (value !== undefined && Number.isSafeInteger(value)) tokens[key] = value;
    }
    if (Object.keys(tokens).length) result.tokens = tokens;
  }
  for (const [time, share] of [["waitMs", "waitShare"], ["coordinationMs", "coordinationShare"]] as const) {
    if (result.durationMs && result[time] !== undefined && result[time]! <= result.durationMs) result[share] = result[time]! / result.durationMs;
  }
  return result;
}

export class TelemetryRecorder {
  readonly file: string;
  readonly maxRecords: number;
  readonly maxBytes: number;
  constructor(file: string, maxRecords = 512, maxBytes = 256 * 1024) {
    if (!Number.isSafeInteger(maxRecords) || maxRecords < 1 || maxRecords > 2048 || !Number.isSafeInteger(maxBytes) || maxBytes < 1024 || maxBytes > 1024 * 1024) throw new Error("invalid_telemetry_bounds");
    this.file = file;
    this.maxRecords = maxRecords;
    this.maxBytes = maxBytes;
  }

  async record(step: StepObservation): Promise<void> {
    const clean = normalizeStep(step);
    await editEvidence<{ schema: number; steps: StepTelemetry[] }>(this.file, current => {
      // Discard unknown persisted schemas. Re-normalizing pre-existing JSON could
      // accidentally bless arbitrary text as telemetry, so retain only our rows.
      const prior = current?.schema === 1 && Array.isArray(current.steps) ? current.steps.filter(isTelemetry) : [];
      const rows = boundedRows(prior.filter(row => row.project !== clean.project || row.run !== clean.run || row.step !== clean.step).concat(clean), this.maxRecords, this.maxBytes);
      // writeJson pretty prints. The bound includes the envelope and newline.
      while (rows.length && Buffer.byteLength(JSON.stringify({ schema: 1, steps: rows }, null, 2) + "\n") > this.maxBytes) rows.shift();
      return { schema: 1, steps: rows } as unknown as JsonValue;
    });
  }
}

function isTelemetry(value: unknown): value is StepTelemetry {
  const row = record(value);
  if (!row || row.schema !== 1 || !["codex", "claude"].includes(String(row.provider))) return false;
  if (![row.project, row.run, row.step].every(value => typeof value === "string" && /^[a-f0-9]{64}$/.test(value))) return false;
  const allowed = new Set(["schema", "project", "run", "step", "provider", "model", "effort", "tool", "tokens", "failure", "contextTokens", "modelMs", "toolMs", "outputBytes", "durationMs", "waitMs", "coordinationMs", "retries", "waitShare", "coordinationShare"]);
  if (!Object.keys(row).every(key => allowed.has(key))) return false;
  for (const [key, value] of Object.entries(row)) {
    if (["schema", "project", "run", "step", "provider"].includes(key)) continue;
    if (["model", "tool"].includes(key)) { if (!label(value)) return false; }
    else if (key === "effort") { if (!["low", "medium", "high", "xhigh", "max"].includes(String(value))) return false; }
    else if (key === "failure") { if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value)) return false; }
    else if (key === "tokens") {
      const tokens = record(value);
      if (!tokens || !Object.entries(tokens).every(([k, v]) => ["input", "cachedInput", "cacheWrite", "output", "reasoning"].includes(k) && finiteMetric(v) !== undefined && Number.isSafeInteger(v))) return false;
    } else if (finiteMetric(value) === undefined) return false;
  }
  return true;
}
