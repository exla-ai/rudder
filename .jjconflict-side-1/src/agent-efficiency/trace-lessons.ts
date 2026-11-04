import { deriveLesson, type Lesson, type LessonScope, type TraceFinding } from "./lessons.js";

/** Instrumented action receipts, not command text or guesses from model prose.
 * inputDigest covers the exact read range/content, awaited attempt/epoch, or
 * check's declared inputs. The adapter keeps their raw inputs private. */
export type TraceAction = {
  stepId: string;
  kind: "read" | "wait_poll" | "check";
  inputDigest: string;
  status: "succeeded" | "failed";
};
export type AcceptedTrace = {
  traceId: string; scope: LessonScope; observedAt: number;
  outcome: TraceFinding["outcome"];
  actions: TraceAction[];
};

/** Bounded trace reduction yields at most three advisory lessons. Acceptance
 * must come from the existing authority's integration/review receipt. */
export function traceFindings(trace: AcceptedTrace): TraceFinding[] {
  if (trace.actions.length > 4096) throw new Error("trace_action_limit");
  const groups = new Map<string, TraceAction[]>();
  for (const action of trace.actions) {
    if (!/^[a-f0-9]{64}$/.test(action.inputDigest) || !action.stepId || action.stepId.length > 256 || !["read", "wait_poll", "check"].includes(action.kind) || !["succeeded", "failed"].includes(action.status)) continue;
    const key = `${action.kind}:${action.inputDigest}`;
    const group = groups.get(key) ?? [];
    // Duplicate delivery of the same receipt is not repeated work.
    if (!group.some(row => row.stepId === action.stepId)) group.push(action);
    groups.set(key, group);
  }
  const candidates = [...groups.values()].filter(group => group.length >= 2 && group.every(action => action.status === "succeeded"))
    .sort((a, b) => b.length - a.length || a[0].inputDigest.localeCompare(b[0].inputDigest));
  const result: TraceFinding[] = [];
  for (const group of candidates) {
    const kind = ({ read: "targeted_read", wait_poll: "bounded_wait", check: "affected_check" } as const)[group[0].kind];
    if (result.some(row => row.kind === kind)) continue;
    result.push({ kind, traceId: trace.traceId, stepIds: group.slice(0, 6).map(action => action.stepId), outcome: trace.outcome,
      scope: trace.scope, observedAt: trace.observedAt, repetitions: group.length, unchangedInputs: true });
  }
  return result;
}

export function deriveTraceLessons(trace: AcceptedTrace, now: number, ttlMs?: number): Lesson[] {
  return traceFindings(trace).flatMap(finding => {
    const lesson = deriveLesson(finding, now, ttlMs);
    return lesson ? [lesson] : [];
  });
}
