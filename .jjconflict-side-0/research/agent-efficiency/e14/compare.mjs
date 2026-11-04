import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const taskFile = new URL("./tasks.json", import.meta.url);
// This digest pins task prompts, acceptance criteria, repeats and source together.
export const TASK_SET_SHA256 = "3cf3361867a42b409a289ec1488dbaacaee5526ea5dd402de2b2811d6ef2dd5a";
export const POLICIES = ["legacy", "shipped"];
const numbers = ["inputTokens", "cachedInputTokens", "outputTokens", "costUsd", "wallMs", "coordinationMs", "waitMs", "reads", "postCompactionReads", "repeatedReadsAfterCompaction", "releaseAttempts", "waitOnlyRequests", "clockChecks", "carriedReadTokens"];
const arrays = ["modelStepMs", "contextTokens"];
const outcomes = ["failedChecks", "reviewRejections", "acceptedChange"];
const counters = new Set(["inputTokens", "cachedInputTokens", "outputTokens", "reads", "postCompactionReads", "repeatedReadsAfterCompaction", "releaseAttempts", "waitOnlyRequests", "clockChecks", "carriedReadTokens", "failedChecks", "reviewRejections"]);
const efforts = ["low", "medium", "high", "xhigh", "max"];
const plainRecord = (value) => value !== null && typeof value === "object" && !Array.isArray(value) && Object.getPrototypeOf(value) === Object.prototype;
const positiveTokenLimit = (value) => value === null || Number.isSafeInteger(value) && value > 0;

export async function loadTaskSet() {
  const bytes = await readFile(taskFile);
  if (createHash("sha256").update(bytes).digest("hex") !== TASK_SET_SHA256) throw new Error("E14 task set digest mismatch");
  return JSON.parse(bytes);
}
const validNumber = (n) => typeof n === "number" && Number.isFinite(n) && n >= 0;
const ratio = (a, b) => a === null || b === null || b === 0 ? null : a / b;
const quantile = (values, p) => values.length ? [...values].sort((a,b) => a-b)[Math.ceil(p * values.length) - 1] : null;
const mean = (values) => values.length ? values.reduce((a,b) => a+b, 0) / values.length : null;
const completeSum = (samples, key) => samples.length && samples.every((s) => validNumber(s[key])) ? samples.reduce((sum,s) => sum+s[key],0) : null;

function validate(sample, manifest) {
  if (sample.schemaVersion !== 1 || sample.taskSetSha256 !== TASK_SET_SHA256 || sample.sourceRevision !== manifest.sourceRevision) throw new Error("E14 sample is not pinned to this task set/source");
  if (!manifest.tasks.some((t) => t.id === sample.taskId) || !POLICIES.includes(sample.policy) || !Number.isSafeInteger(sample.repeat) || sample.repeat < 1) throw new Error("Invalid E14 task, policy or repeat");
  if (!["codex", "claude"].includes(sample.backend)) throw new Error("E14 requires a named Codex or Claude adapter");
  for (const field of ["model", "backendVersion", "hardware", "platform", "cacheState"]) if (typeof sample[field] !== "string" || !sample[field].trim() || sample[field].length > 200) throw new Error(`Missing or invalid pin: ${field}`);
  if (!["warm", "cold"].includes(sample.cacheState) || !["actual", "fixture"].includes(sample.evidenceKind)) throw new Error("Invalid cache state or evidence kind");
  if (!plainRecord(sample.launchSettings) || Object.keys(sample.launchSettings).length !== 4 || !["effort", "compactionTokens", "toolOutputTokens", "waitAndReadRule"].every((key) => Object.hasOwn(sample.launchSettings, key))) throw new Error("Effective launch settings are required");
  const settings = sample.launchSettings;
  if (settings.effort !== null && !efforts.includes(settings.effort)) throw new Error("Invalid effective effort");
  if (![settings.compactionTokens, settings.toolOutputTokens].every(positiveTokenLimit) || typeof settings.waitAndReadRule !== "boolean") throw new Error("Invalid effective launch settings");
  if (!plainRecord(sample.explicitUserSettings)) throw new Error("Explicit user settings must be recorded, including an empty object");
  const explicit = sample.explicitUserSettings;
  if (Object.keys(explicit).some((key) => !["effort", "compactionTokens", "toolOutputTokens"].includes(key))) throw new Error("Unknown explicit user setting");
  if (Object.hasOwn(explicit, "effort") && explicit.effort !== null && !efforts.includes(explicit.effort)) throw new Error("Invalid explicit effort");
  for (const key of ["compactionTokens", "toolOutputTokens"]) if (Object.hasOwn(explicit, key) && !positiveTokenLimit(explicit[key])) throw new Error("Invalid explicit token limit");
  if (explicit.effort != null && settings.effort !== (sample.backend === "codex" && explicit.effort === "max" ? "xhigh" : explicit.effort)) throw new Error("Explicit effort was not preserved");
  if (sample.backend === "codex" && sample.policy === "shipped" && settings.effort !== (explicit.effort === "max" ? "xhigh" : explicit.effort ?? "high")) throw new Error("Shipped Codex effort differs from the one launch policy");
  if (sample.backend === "codex") {
    const expectedCompaction = explicit.compactionTokens ?? (sample.policy === "shipped" ? 150000 : null);
    const expectedOutput = explicit.toolOutputTokens ?? (sample.policy === "shipped" ? 6000 : null);
    if (settings.compactionTokens !== expectedCompaction || settings.toolOutputTokens !== expectedOutput) throw new Error("Codex launch limits differ from the pinned policy");
  } else if (settings.effort !== (explicit.effort ?? null) || settings.compactionTokens !== (explicit.compactionTokens ?? null) || settings.toolOutputTokens !== (explicit.toolOutputTokens ?? null)) throw new Error("Claude defaults were changed without evidence");
  if (settings.waitAndReadRule !== (sample.policy === "shipped")) throw new Error("Wait-and-read rule differs from policy");
  for (const key of [...numbers, ...outcomes.filter((x) => x !== "acceptedChange")]) if (sample[key] != null && (!validNumber(sample[key]) || counters.has(key) && !Number.isSafeInteger(sample[key]))) throw new Error(`Invalid metric ${key}`);
  if (sample.acceptedChange != null && typeof sample.acceptedChange !== "boolean") throw new Error("Invalid accepted outcome");
  for (const key of arrays) if (sample[key] != null && (!Array.isArray(sample[key]) || !sample[key].every((value) => validNumber(value) && (key !== "contextTokens" || Number.isSafeInteger(value))))) throw new Error(`Invalid metric ${key}`);
  if (validNumber(sample.cachedInputTokens) && validNumber(sample.inputTokens) && sample.cachedInputTokens > sample.inputTokens) throw new Error("Cached input exceeds all input tokens");
  if (validNumber(sample.reads) && validNumber(sample.postCompactionReads) && sample.postCompactionReads > sample.reads) throw new Error("Post-compaction reads exceed all reads");
  if (validNumber(sample.postCompactionReads) && validNumber(sample.repeatedReadsAfterCompaction) && sample.repeatedReadsAfterCompaction > sample.postCompactionReads) throw new Error("Repeated reads exceed post-compaction reads");
  if (!Array.isArray(sample.evidenceSha256) || !sample.evidenceSha256.length || !sample.evidenceSha256.every((s) => typeof s === "string" && /^[a-f0-9]{64}$/.test(s))) throw new Error("Checks/review evidence requires private artifact SHA256 references");
}

// Fixed field order makes settings pins deterministic without preserving arbitrary text in reports.
function stable(value) {
  if (Array.isArray(value)) return `[${value.map(stable).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.keys(value).sort().map((k) => `${JSON.stringify(k)}:${stable(value[k])}`).join(",")}}`;
  return JSON.stringify(value);
}
function groupPin(sample) {
  return createHash("sha256").update(stable([sample.backend, sample.model, sample.backendVersion, sample.hardware, sample.platform, sample.cacheState, sample.explicitUserSettings])).digest("hex");
}
function aggregate(samples) {
  const result = { samples: samples.length, acceptedChanges: samples.length && samples.every((s) => typeof s.acceptedChange === "boolean") ? samples.filter((s) => s.acceptedChange).length : null };
  for (const key of [...numbers, "failedChecks", "reviewRejections"]) result[key] = completeSum(samples, key);
  result.evidenceSha256 = [...new Set(samples.flatMap((sample) => sample.evidenceSha256))].sort();
  result.observedTotals = Object.fromEntries([...numbers, "failedChecks", "reviewRejections"].map((key) => {
    const observed = samples.filter((sample) => validNumber(sample[key]));
    return [key, { samples: observed.length, total: observed.length ? observed.reduce((sum, sample) => sum + sample[key], 0) : null }];
  }));
  for (const key of arrays) {
    const values = samples.flatMap((s) => s[key] ?? []);
    const complete = samples.length > 0 && samples.every((s) => Array.isArray(s[key]) && s[key].length > 0);
    result[key] = { observed: values.length, complete, mean: complete ? mean(values) : null, p50: complete ? quantile(values,.5) : null, p95: complete ? quantile(values,.95) : null };
  }
  result.tokensPerAcceptedChange = ratio(result.inputTokens === null || result.outputTokens === null ? null : result.inputTokens+result.outputTokens, result.acceptedChanges);
  result.costPerAcceptedChange = ratio(result.costUsd, result.acceptedChanges);
  result.wallMsPerAcceptedChange = ratio(result.wallMs, result.acceptedChanges);
  result.reReadRateAfterCompaction = ratio(result.repeatedReadsAfterCompaction, result.postCompactionReads);
  result.coordinationShare = ratio(result.coordinationMs, result.wallMs);
  result.waitShare = ratio(result.waitMs, result.wallMs);
  result.missingFields = [...numbers, ...arrays, ...outcomes].filter((key) => samples.some((s) => s[key] == null));
  return result;
}

export async function compare(samples) {
  const manifest = await loadTaskSet();
  if (!Array.isArray(samples)) throw new Error("E14 input must be an array of samples");
  const groups = new Map();
  const seen = new Set();
  for (const sample of samples) {
    validate(sample, manifest);
    const pin = groupPin(sample), key = `${pin}:${sample.taskId}:${sample.repeat}:${sample.policy}`;
    if (seen.has(key)) throw new Error("Duplicate E14 sample");
    seen.add(key);
    if (!groups.has(pin)) groups.set(pin, []);
    const existing = groups.get(pin).find((s) => s.policy === sample.policy);
    if (existing && stable(existing.launchSettings) !== stable(sample.launchSettings)) throw new Error("Effective launch settings drift within a comparison");
    groups.get(pin).push(sample);
  }
  const comparisons = [];
  for (const [pin, records] of groups) {
    const paired = [], unpaired = [];
    for (const record of records) {
      const peer = records.find((s) => s.taskId === record.taskId && s.repeat === record.repeat && s.policy !== record.policy);
      (peer ? paired : unpaired).push(record);
    }
    const byPolicy = Object.fromEntries(POLICIES.map((policy) => [policy, aggregate(paired.filter((s) => s.policy === policy))]));
    const allPolicies = Object.fromEntries(POLICIES.map((policy) => [policy, aggregate(records.filter((s) => s.policy === policy))]));
    const unpairedPolicies = Object.fromEntries(POLICIES.map((policy) => [policy, aggregate(unpaired.filter((s) => s.policy === policy))]));
    const coverage = manifest.tasks.map((task) => ({ taskId: task.id, pairedRepeats: paired.filter((s) => s.taskId === task.id && s.policy === "legacy").length }));
    const missing = coverage.some((c) => c.pairedRepeats < manifest.minimumRepeatsPerTask) || Object.values(byPolicy).some((s) => s.missingFields.length > 0 || !s.modelStepMs.complete || !s.contextTokens.complete);
    const fixture = records.some((r) => r.evidenceKind !== "actual");
    const before = byPolicy.legacy, after = byPolicy.shipped;
    const comparableReads = before.reReadRateAfterCompaction !== null && after.reReadRateAfterCompaction !== null;
    const qualityPreserved = !missing && after.acceptedChanges >= before.acceptedChanges && after.failedChecks <= before.failedChecks && after.reviewRejections <= before.reviewRejections && comparableReads && after.reReadRateAfterCompaction <= before.reReadRateAfterCompaction;
    const improves = qualityPreserved && after.tokensPerAcceptedChange < before.tokensPerAcceptedChange && after.costPerAcceptedChange < before.costPerAcceptedChange && after.wallMsPerAcceptedChange < before.wallMsPerAcceptedChange;
    const complete = !missing && !fixture && !unpaired.length && comparableReads && before.acceptedChanges > 0 && after.acceptedChanges > 0;
    comparisons.push({ pinSha256: pin, backend: records[0].backend, coverage, unpairedSamples: unpaired.length, policies: byPolicy, allPolicies, unpairedPolicies,
      status: complete ? "measured" : "pending", gate: complete ? (improves ? "candidate-supported" : "retune-or-revert") : "pending",
      qualityPreserved: complete ? qualityPreserved : null,
      reason: fixture ? "Fixture evidence validates the harness only" : missing || unpaired.length ? "Missing paired repeats or supported fields" : !comparableReads ? "No measured post-compaction read denominator" : !complete ? "No accepted changes for a cost/quality comparison" : "Descriptive paired comparison; retain evidence and review before policy changes" });
  }
  const adapters = Object.fromEntries(["codex", "claude"].map((backend) => [backend, comparisons.some((c) => c.backend === backend && c.status === "measured") ? "measured" : "pending"]));
  return { adapters, schemaVersion: 1, taskSetSha256: TASK_SET_SHA256, sourceRevision: manifest.sourceRevision, status: Object.values(adapters).every((s) => s === "measured") && comparisons.every((c) => c.status === "measured") ? "measured" : "pending", comparisons,
    actualAdapterSamples: samples.filter((s) => s.evidenceKind === "actual").length,
    limitation: "No automatic launch-policy change. Fixtures and session correlations do not establish model speed or platform qualification. Baseline settings are experiment-only; production keeps one launch policy." };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const samples = process.argv[2] ? JSON.parse(await readFile(process.argv[2], "utf8")) : [];
  process.stdout.write(`${JSON.stringify(await compare(samples), null, 2)}\n`);
}
