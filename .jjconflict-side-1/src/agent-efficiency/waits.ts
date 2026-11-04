import { performance } from "node:perf_hooks";

/** Event-driven observations. This registry never schedules work or releases a
 * worker slot: only the process owner may call exited after observing exit. */
export interface WaitScope {
  projectId: string;
  ownerEpoch: number;
  runId: string;
  runEpoch: number;
  kind: "job" | "release";
  id: string;
}

export type SettledOutcome = "succeeded" | "failed" | "cancelled";
export type WaitResult =
  | { status: "settled"; scope: WaitScope; outcome: SettledOutcome }
  | { status: "timeout"; scope: WaitScope; name: "job_wait_timeout" | "release_wait_timeout" }
  | { status: "cancelled"; scope: WaitScope; name: "wait_cancelled" };

interface Waiter {
  deadline: number;
  finish(result: WaitResult): void;
}
interface Entry {
  scope: WaitScope;
  admitted: boolean;
  exited: boolean;
  outcome?: SettledOutcome;
  waiters: Set<Waiter>;
}

function positiveBound(value: number, name: string): number {
  if (!Number.isSafeInteger(value) || value < 1) throw new Error(`invalid ${name}`);
  return value;
}

function snapshot(scope: WaitScope): WaitScope {
  for (const value of [scope.projectId, scope.runId, scope.id]) {
    if (typeof value !== "string" || !value.length || value.length > 256 || /[\x00-\x1f\x7f]/.test(value)) {
      throw new Error("invalid wait scope identity");
    }
  }
  positiveBound(scope.ownerEpoch, "owner epoch");
  positiveBound(scope.runEpoch, "run epoch");
  if (scope.kind !== "job" && scope.kind !== "release") throw new Error("invalid wait kind");
  return Object.freeze({
    projectId: scope.projectId, ownerEpoch: scope.ownerEpoch,
    runId: scope.runId, runEpoch: scope.runEpoch, kind: scope.kind, id: scope.id,
  });
}

export class BoundedWaitRegistry {
  private readonly entries = new Map<string, Entry>();
  private waiterCount = 0;
  private readonly maxWaiters: number;
  private readonly maxEntries: number;
  private readonly maxTimeoutMs: number;

  constructor(options: { maxWaiters?: number; maxEntries?: number; maxTimeoutMs?: number } = {}) {
    this.maxWaiters = positiveBound(options.maxWaiters ?? 256, "waiter bound");
    this.maxEntries = positiveBound(options.maxEntries ?? 1024, "entry bound");
    this.maxTimeoutMs = positiveBound(options.maxTimeoutMs ?? 300_000, "timeout bound");
    if (this.maxTimeoutMs > 2_147_483_647) throw new Error("timeout bound exceeds timer capability");
  }

  private entry(scope: WaitScope): Entry {
    const saved = snapshot(scope);
    const key = JSON.stringify(saved);
    const existing = this.entries.get(key);
    if (existing) return existing;
    if (this.entries.size >= this.maxEntries) {
      // Retain live admissions and outstanding subscriptions. Settled cache
      // eviction may require the caller to replay its authoritative snapshot.
      for (const [oldKey, old] of this.entries) {
        if (!old.admitted && old.waiters.size === 0) {
          this.entries.delete(oldKey);
          break;
        }
      }
    }
    if (this.entries.size >= this.maxEntries) throw new Error("wait_entry_limit");
    const entry: Entry = { scope: saved, admitted: false, exited: false, waiters: new Set() };
    this.entries.set(key, entry);
    return entry;
  }

  /** Mirror an already admitted process. This is not an admission decision. */
  admit(scope: WaitScope): void {
    const entry = this.entry(scope);
    if (entry.exited) throw new Error("cannot readmit an exited attempt");
    entry.admitted = true;
  }

  isAdmitted(scope: WaitScope): boolean {
    return this.entries.get(JSON.stringify(snapshot(scope)))?.admitted ?? false;
  }

  /** First terminal observation wins. Duplicate/conflicting observations return
   * false and must be reconciled by the authority rather than replayed here. */
  observe(scope: WaitScope, outcome: SettledOutcome): boolean {
    if (!["succeeded", "failed", "cancelled"].includes(outcome)) throw new Error("invalid settled outcome");
    const entry = this.entry(scope);
    if (entry.outcome !== undefined) return false;
    entry.outcome = outcome;
    for (const waiter of [...entry.waiters]) {
      waiter.finish(performance.now() >= waiter.deadline
        ? this.timeout(entry.scope)
        : { status: "settled", scope: entry.scope, outcome });
    }
    return true;
  }

  /** Call only after actual process exit. Completion, timeout, interruption and
   * cancellation deliberately never call this method. */
  exited(scope: WaitScope): void {
    const entry = this.entry(scope);
    entry.admitted = false;
    entry.exited = true;
  }

  private timeout(scope: WaitScope): WaitResult {
    return { status: "timeout", scope, name: scope.kind === "job" ? "job_wait_timeout" : "release_wait_timeout" };
  }

  wait(scope: WaitScope, options: { timeoutMs: number; signal?: AbortSignal }): Promise<WaitResult> {
    const { timeoutMs, signal } = options;
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 0 || timeoutMs > this.maxTimeoutMs) {
      throw new Error("invalid bounded wait timeout");
    }
    const entry = this.entry(scope);
    if (signal?.aborted) return Promise.resolve({ status: "cancelled", scope: entry.scope, name: "wait_cancelled" });
    if (entry.outcome !== undefined) return Promise.resolve({ status: "settled", scope: entry.scope, outcome: entry.outcome });
    if (timeoutMs === 0) return Promise.resolve(this.timeout(entry.scope));
    if (this.waiterCount >= this.maxWaiters) throw new Error("wait_subscription_limit");
    return new Promise<WaitResult>((resolve) => {
      let done = false;
      const cancel = (): void => waiter.finish({ status: "cancelled", scope: entry.scope, name: "wait_cancelled" });
      const waiter: Waiter = {
        deadline: performance.now() + timeoutMs,
        finish: (result) => {
          if (done) return;
          done = true;
          clearTimeout(timer);
          signal?.removeEventListener("abort", cancel);
          entry.waiters.delete(waiter);
          this.waiterCount--;
          resolve(result);
        },
      };
      const timer = setTimeout(() => waiter.finish(this.timeout(entry.scope)), timeoutMs);
      entry.waiters.add(waiter);
      this.waiterCount++;
      signal?.addEventListener("abort", cancel, { once: true });
    });
  }

  get pendingSubscriptions(): number { return this.waiterCount; }
}
