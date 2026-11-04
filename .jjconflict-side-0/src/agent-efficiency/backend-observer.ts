import path from "node:path";
import { randomUUID } from "node:crypto";
import type { RunRecord } from "../types.js";
import { runDir } from "../state.js";
import { supportedUsage, TelemetryRecorder } from "./telemetry.js";
import { BackendReadTrace } from "./production.js";

/** Observe supported provider usage on the existing stream. This private local
 * evidence cannot admit work or change completion/integration state.
 */
export class BackendTelemetryObserver {
  private readonly run: RunRecord;
  #recorder: TelemetryRecorder;
  #attempt = randomUUID();
  #sequence = 0;
  #pending = 0;
  #reads: BackendReadTrace;
  #inflight = new Set<Promise<void>>();
  constructor(run: RunRecord) {
    this.run = run;
    this.#recorder = new TelemetryRecorder(path.join(runDir(run.repoRoot, run.id), "efficiency.json"));
    this.#reads = new BackendReadTrace(run);
  }
  observe(event: unknown): Promise<void> {
    if (this.#inflight.size >= 16) return Promise.resolve();
    const work = this.#observe(event);
    this.#inflight.add(work);
    return work.finally(() => this.#inflight.delete(work));
  }
  /** Finish bounded local evidence before the integration path can learn it.
   * A slow store cannot keep backend completion waiting indefinitely. */
  async flush(timeoutMs = 1000): Promise<void> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        Promise.allSettled([...this.#inflight]),
        new Promise<void>(resolve => { timer = setTimeout(resolve, Math.min(2000, Math.max(0, timeoutMs))); }),
      ]);
    } finally { if (timer) clearTimeout(timer); }
  }
  async #observe(event: unknown): Promise<void> {
    await this.#reads.observe(event);
    // Observation cannot backpressure terminal rendering or grow an unbounded
    // queue when the private evidence store is slow.
    if (this.#pending >= 16) return;
    if (this.run.backend !== "codex" && this.run.backend !== "claude") return;
    const usage = supportedUsage(this.run.backend, event);
    if (!usage.tokens) return;
    const data = event as { uuid?: unknown; message?: { id?: unknown }; payload?: { info?: { total_token_usage?: unknown } } };
    // Claude emits partial messages with the same id. Codex repeats token_count
    // for rate-limit notifications; its cumulative totals identify that usage
    // observation but are never charged as per-step token amounts.
    const message = this.run.backend === "claude" ? data.message?.id ?? data.uuid : undefined;
    const total = this.run.backend === "codex" ? data.payload?.info?.total_token_usage : undefined;
    const counters = total && typeof total === "object" ? Object.fromEntries(
      ["input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"]
        .flatMap(key => {
          const value = (total as Record<string, unknown>)[key];
          return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? [[key, value]] : [];
        })) : undefined;
    const stepId = `${this.#attempt}:${typeof message === "string" ? message : counters && Object.keys(counters).length ? JSON.stringify(counters) : ++this.#sequence}`;
    this.#pending++;
    try {
      await this.#recorder.record({ projectId: this.run.repoRoot, runId: this.run.id, stepId,
        provider: this.run.backend, model: usage.model ?? this.run.model, effort: this.run.effort, ...usage });
    } finally { this.#pending--; }
  }
}
