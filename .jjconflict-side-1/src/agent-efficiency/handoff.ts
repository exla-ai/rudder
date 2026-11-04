import { evidenceId, redact } from "./privacy.js";

export type HandoffInput = {
  task: string; constraints: string[]; openObligations: string[];
  recentChanges: string[]; evidenceRefs?: string[];
};

/** Data snapshot after compaction, never a new grant of authority. */
export function compactionHandoff(input: HandoffInput, maxBytes = 8192): string {
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 1024 || maxBytes > 16384) throw new Error("invalid_handoff_budget");
  const fields = ["constraints", "openObligations", "recentChanges"] as const;
  const result = {
    schema: 1, kind: "compaction_handoff", authority: "context_only",
    task: redact(input.task, 1200), constraints: [] as string[], openObligations: [] as string[], recentChanges: [] as string[],
    evidenceRefs: (input.evidenceRefs ?? []).slice(0, 4).map(evidenceId),
    omitted: { constraints: input.constraints.length, openObligations: input.openObligations.length, recentChanges: input.recentChanges.length },
    needsRetrieval: input.task.length > 1200 || (input.evidenceRefs?.length ?? 0) > 4,
  };
  // Reserve room for all four sections even with a minimum-size budget.
  result.task = redact(input.task, Math.min(1200, Math.floor(maxBytes / 6)));
  result.needsRetrieval ||= result.task.length < input.task.length;
  // Round robin keeps a long constraints list from hiding every obligation.
  for (let index = 0; index < 8; index++) {
    for (const field of fields) {
      const text = input[field][index];
      if (text === undefined) continue;
      const safe = redact(text, Math.min(320, Math.floor(maxBytes / 12)));
      result[field].push(safe);
      result.omitted[field]--;
      if (Buffer.byteLength(JSON.stringify(result)) > maxBytes) {
        result[field].pop();
        result.omitted[field]++;
      } else if (safe.length < text.length) result.needsRetrieval = true;
    }
  }
  result.needsRetrieval ||= Object.values(result.omitted).some(count => count > 0);
  // A multibyte task may consume more bytes than its character budget.
  while (Buffer.byteLength(JSON.stringify(result)) > maxBytes && result.task.length) {
    result.task = result.task.slice(0, Math.floor(result.task.length / 2));
    result.needsRetrieval = true;
  }
  return JSON.stringify(result);
}
