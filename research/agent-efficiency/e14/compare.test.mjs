import assert from "node:assert/strict";
import test from "node:test";
import { compare, loadTaskSet, TASK_SET_SHA256 } from "./compare.mjs";

const manifest = await loadTaskSet();
function sample(taskId, policy, repeat = 1, overrides = {}) {
  return { schemaVersion: 1, taskSetSha256: TASK_SET_SHA256, sourceRevision: manifest.sourceRevision,
    taskId, policy, repeat, backend: "codex", model: "pinned-test-model", backendVersion: "test-version", hardware: "test-machine", platform: "linux-x64", cacheState: "warm", explicitUserSettings: {}, launchSettings: {effort: "high", compactionTokens: policy === "legacy" ? null : 150000, toolOutputTokens: policy === "legacy" ? null : 6000, waitAndReadRule: policy === "shipped"}, evidenceKind: "fixture", evidenceSha256: ["a".repeat(64)],
    acceptedChange: true, failedChecks: 0, reviewRejections: 0,
    inputTokens: policy === "legacy" ? 200 : 100, cachedInputTokens: 10, outputTokens: 20, costUsd: policy === "legacy" ? 2 : 1, wallMs: policy === "legacy" ? 100 : 50,
    coordinationMs: 5, waitMs: 5, reads: 10, postCompactionReads: 5, repeatedReadsAfterCompaction: 1, releaseAttempts: 0, waitOnlyRequests: 1, clockChecks: 0, carriedReadTokens: 40,
    modelStepMs: [10, 20], contextTokens: [100, 200], ...overrides };
}
function complete(overrides = {}) {
  return manifest.tasks.flatMap((task) => Array.from({length: manifest.minimumRepeatsPerTask}, (_, n) => [sample(task.id, "legacy", n+1, overrides), sample(task.id, "shipped", n+1, overrides)]).flat());
}

test("empty input keeps actual model/platform E14 pending", async () => {
  const report = await compare([]);
  assert.equal(report.status, "pending");
  assert.equal(report.actualAdapterSamples, 0);
  assert.deepEqual(report.comparisons, []);
});

test("fixed task pin protects prompt, checks and source", async () => {
  assert.equal(manifest.tasks.length, 4);
  assert.equal(manifest.minimumRepeatsPerTask, 3);
  assert.match(TASK_SET_SHA256, /^[a-f0-9]{64}$/);
  await assert.rejects(compare([sample(manifest.tasks[0].id,"legacy",1,{sourceRevision:"different"})]), /pinned/);
});

test("synthetic improvements never pass the actual model gate", async () => {
  const report = await compare(complete());
  assert.equal(report.status, "pending");
  assert.equal(report.comparisons[0].gate, "pending");
  assert.match(report.comparisons[0].reason, /Fixture/);
});

test("missing cost and failures remain unknown instead of zero", async () => {
  const records = complete();
  delete records[1].costUsd;
  delete records[1].failedChecks;
  const { policies } = (await compare(records)).comparisons[0];
  assert.equal(policies.shipped.costUsd, null);
  assert.equal(policies.shipped.costPerAcceptedChange, null);
  assert.equal(policies.shipped.failedChecks, null);
  assert.ok(policies.shipped.missingFields.includes("costUsd"));
});

test("failed samples and reviews count against accepted outcomes", async () => {
  const records = complete();
  Object.assign(records[1], { acceptedChange: false, failedChecks: 2, reviewRejections: 1, costUsd: 10 });
  const { policies } = (await compare(records)).comparisons[0];
  assert.equal(policies.shipped.samples, 12);
  assert.equal(policies.shipped.acceptedChanges, 11);
  assert.equal(policies.shipped.failedChecks, 2);
  assert.equal(policies.shipped.reviewRejections, 1);
  assert.equal(policies.shipped.costPerAcceptedChange, 21/11);
  assert.equal(policies.shipped.reReadRateAfterCompaction, 0.2);
});

test("model, cache, hardware and explicit settings cannot be paired across pins", async () => {
  for (const override of [{ model: "other" }, {cacheState:"cold"}, {hardware:"other"}, {explicitUserSettings:{effort:"low"},launchSettings:{effort:"low",compactionTokens:150000,toolOutputTokens:6000,waitAndReadRule:true}}]) {
    const records = [sample(manifest.tasks[0].id,"legacy"), sample(manifest.tasks[0].id,"shipped",1,override)];
    const report = await compare(records);
    assert.equal(report.comparisons.length, 2);
    assert.ok(report.comparisons.every((c) => c.unpairedSamples === 1));
  }
});

test("incomplete repeats, impossible metrics and duplicate samples are rejected or pending", async () => {
  const one = sample(manifest.tasks[0].id,"legacy");
  await assert.rejects(compare([one,one]), /Duplicate/);
  await assert.rejects(compare([{...one,inputTokens:-1}]), /Invalid metric/);
  await assert.rejects(compare([{...one,postCompactionReads:11}]), /exceed/);
  assert.equal((await compare([one,sample(one.taskId,"shipped")])).status,"pending");
});

test("complete adapter-shaped records evaluate cost, quality and wall time together", async () => {
  // Unit fixture for gate branching only. It is never saved as actual evidence.
  const records = complete({evidenceKind:"actual"});
  const report = await compare(records);
  assert.equal(report.comparisons[0].gate,"candidate-supported");
  records[1].failedChecks = 1;
  assert.equal((await compare(records)).comparisons[0].gate,"retune-or-revert");
});

test("reports exclude arbitrary raw payload fields and private source references", async () => {
  const records = complete();
  records[0].rawTranscript = "PRIVATE_MARKER";
  records[0].rawTracePath = "/private/SECRET";
  const output = JSON.stringify(await compare(records));
  assert.ok(!output.includes("PRIVATE_MARKER"));
  assert.ok(!output.includes("/private/SECRET"));
  assert.ok(!output.includes("pinned-test-model"));
});

test("effective policy, explicit user choices and Claude defaults are verified", async () => {
  const base = sample(manifest.tasks[0].id,"shipped");
  await assert.rejects(compare([{...base, launchSettings:{...base.launchSettings, effort:"low"}}]), /effort/);
  await assert.rejects(compare([{...base, explicitUserSettings:{effort:"low"}}]), /preserved/);
  await assert.rejects(compare([{...base, backend:"claude"}]), /Claude/);
  const valid = {...base, backend:"claude", launchSettings:{effort:null,compactionTokens:null,toolOutputTokens:null,waitAndReadRule:true}};
  assert.equal((await compare([valid])).adapters.claude,"pending");
  assert.equal((await compare(complete({evidenceKind:"actual"}))).adapters.claude,"pending");
});

test("unpaired failures retain costs, outcomes, observed partial totals and safe evidence refs", async () => {
  const failed = sample(manifest.tasks[0].id,"shipped",4,{acceptedChange:false,failedChecks:3,reviewRejections:2,costUsd:9,evidenceSha256:["b".repeat(64)]});
  const unknown = sample(manifest.tasks[1].id,"shipped",4,{failedChecks:null,costUsd:null,evidenceSha256:["c".repeat(64)]});
  const report = await compare([...complete(),failed,unknown]);
  const group = report.comparisons[0];
  assert.equal(group.unpairedSamples,2);
  assert.equal(group.policies.shipped.failedChecks,0);
  assert.equal(group.unpairedPolicies.shipped.failedChecks,null);
  assert.deepEqual(group.unpairedPolicies.shipped.observedTotals.failedChecks,{samples:1,total:3});
  assert.deepEqual(group.unpairedPolicies.shipped.observedTotals.costUsd,{samples:1,total:9});
  assert.equal(group.unpairedPolicies.shipped.reviewRejections,2);
  assert.equal(group.allPolicies.shipped.samples,14);
  assert.ok(group.unpairedPolicies.shipped.evidenceSha256.includes("b".repeat(64)));
  assert.ok(group.allPolicies.shipped.evidenceSha256.includes("c".repeat(64)));
  assert.equal(group.gate,"pending");
});

test("explicit settings and integral counters reject malformed private records", async () => {
  const base = sample(manifest.tasks[0].id,"shipped");
  for (const explicitUserSettings of [{unknown:true},{effort:"future"},{effort:3},{compactionTokens:0},{toolOutputTokens:1.5},{compactionTokens:-1}]) {
    await assert.rejects(compare([{...base,explicitUserSettings}]),/explicit|Unknown/);
  }
  for (const malformed of [{reads:0.5},{inputTokens:1.5},{failedChecks:0.1},{reviewRejections:0.5},{contextTokens:[1.5]},{repeat:Number.MAX_SAFE_INTEGER+1}]) {
    await assert.rejects(compare([{...base,...malformed}]),/Invalid/);
  }
  await assert.rejects(compare([{...base,cachedInputTokens:base.inputTokens+1}]),/Cached input/);
  await assert.rejects(compare([{...base,launchSettings:{...base.launchSettings,toolOutputTokens:0}}]),/Invalid effective/);
});
