// How one run.json reads on every surface: the supervisor's status word
// (cloud/worker/agent-status.mjs) and the control plane's one needs-you rule
// (cloud/src/agent-status.ts). Pure; the server wiring is in cloud-home.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { phoneStatus, statusFields, prInfo } from "../cloud/worker/agent-status.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const built = path.resolve(here, "..", "cloud", "dist", "agent-status.js");
const test = fs.existsSync(built)
  ? nodeTest
  : (name, fn) => nodeTest(name, { skip: "cloud not built (npm --prefix cloud run build)" }, fn);
const plane = fs.existsSync(built) ? await import(built) : null;

const alive = () => true;
const dead = () => false;

nodeTest("a running row reads working only while its dashboard and process are alive", () => {
  const record = { status: "running", process: { pid: 4242 } };
  assert.equal(phoneStatus(record, { alive }), "working");
  assert.equal(phoneStatus({ ...record, attention: "permission" }, { alive }), "waiting");
  assert.equal(phoneStatus(record, { alive: dead }), "orphaned", "a dead pid is not working");
  assert.equal(phoneStatus(record, { dashboardRunning: false, alive }), "paused", "no dashboard, no agent");
  assert.equal(phoneStatus({ status: "created" }, { dashboardRunning: false }), "paused");
  assert.equal(phoneStatus({ status: "steering" }, { alive: dead }), "working", "no recorded process: trust the dashboard");
  // The real check, on this process and an impossible pid.
  assert.equal(phoneStatus({ status: "running", process: { pid: process.pid } }), "working");
  assert.equal(phoneStatus({ status: "running", process: { pid: 2 ** 22 + 12345 } }), "orphaned");
});

nodeTest("finished rows: delivery blocked fails, a merged PR is merged, work out of trunk is not merged", () => {
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "delivery blocked" }), "failed");
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "delivery proof needed" }), "review");
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "review" }), "review");
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "completed" }), "done");
  assert.equal(phoneStatus({ status: "merge-conflict", lifecyclePhase: "integration-blocked" }), "review");
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "review", publish: { number: 3, observedState: "merged" } }), "merged");
  assert.equal(phoneStatus({ status: "completed", lifecyclePhase: "review", publish: { number: 3, observedState: "open" } }), "review");
  assert.equal(phoneStatus({ status: "merged", lifecyclePhase: "merged-locally" }), "merged");
  assert.equal(phoneStatus({ status: "merged", lifecyclePhase: "merged-not-in-trunk" }), "review");
  assert.equal(phoneStatus({ status: "merged", integration: { inTrunk: false } }), "review");
  assert.equal(phoneStatus({ status: "cancelled" }), "stopped");
  assert.equal(phoneStatus({ status: "migrated" }), "done");
});

nodeTest("statusFields carries the raw facts; a PR without observedState has an unknown state", () => {
  const fields = statusFields({ status: "merge-conflict", lifecyclePhase: "integration-blocked", publish: { number: 9, url: "u" } });
  assert.equal(fields.conflict, true);
  assert.deepEqual(fields.pr, { state: null, number: 9, url: "u" });
  assert.equal(prInfo({}), null);
  assert.equal(prInfo({ publish: { branch: "x" } }), null);
  assert.equal(prInfo({ publish: { observedState: "bogus", number: 1 } }).state, null);
  assert.equal(statusFields({ status: "merged", lifecyclePhase: "merged-not-in-trunk" }).notInTrunk, true);
});

test("needs you: one rule, a reason for every case", () => {
  const { agentNeeds } = plane;
  const cases = [
    [{ status: "working", asks: [{ kind: "permission" }] }, true, "permission"],
    [{ status: "working", asks: [{ kind: "question" }] }, true, "question"],
    [{ status: "working", asks: [{ kind: "plan" }] }, true, "plan"],
    [{ status: "working", asks: [{ kind: "weird" }] }, true, "permission"],
    [{ status: "waiting", attention: "input" }, true, "input"],
    [{ status: "waiting", attention: "interrupted" }, true, "interrupted"],
    [{ status: "waiting", attention: "error" }, true, "error"],
    [{ status: "waiting", attention: "permission" }, true, "permission"],
    [{ status: "waiting" }, true, "input"],
    [{ status: "failed" }, true, "failed"],
    [{ status: "failed", phase: "delivery blocked" }, true, "delivery-blocked"],
    [{ status: "done", phase: "delivery blocked" }, true, "delivery-blocked"],
    [{ status: "review", phase: "review" }, true, "review"],
    [{ status: "review", phase: "delivery proof needed" }, true, "delivery"],
    [{ status: "review", conflict: true }, true, "conflict"],
    [{ status: "review", phase: "integration-blocked", pr: { state: "open" } }, true, "conflict"],
    [{ status: "review", notInTrunk: true }, true, "not-in-trunk"],
    [{ status: "review", pr: { state: "open" } }, false, null],
    [{ status: "review", pr: { state: "draft" } }, false, null],
    [{ status: "review", pr: { state: "closed" } }, true, "review"],
    [{ status: "review", pr: { state: null, number: 4 } }, true, "review"],
    [{ status: "done", pr: { state: "merged" } }, false, null],
    [{ status: "working" }, false, null],
    [{ status: "starting", pending: true, asks: [{ kind: "question" }] }, false, null],
    [{ status: "done" }, false, null],
    [{ status: "merged" }, false, null],
    [{ status: "paused" }, false, null],
    [{ status: "orphaned" }, false, null],
  ];
  for (const [agent, needsYou, reason] of cases) {
    assert.deepEqual(agentNeeds(agent), { needsYou, reason }, JSON.stringify(agent));
  }
  assert.equal(plane.isNeedsYou({ status: "review", needsYou: false }), false, "a stamped answer wins");
});

test("annotate: a disconnected home freezes live rows as paused with when they were last seen", () => {
  const { annotateAgent } = plane;
  const live = annotateAgent({ id: "a", status: "working" }, { live: true, lastSeen: 5 });
  assert.equal(live.status, "working");
  assert.equal(live.stale, undefined);
  const frozen = annotateAgent({ id: "a", status: "waiting", attention: "input", asks: [{ kind: "question" }] }, { live: false, lastSeen: 5 });
  assert.deepEqual([frozen.status, frozen.stale, frozen.lastSeen, frozen.asks, frozen.needsYou, frozen.reason], ["paused", true, 5, [], false, null]);
  const review = annotateAgent({ id: "r", status: "review" }, { live: false, lastSeen: 5 });
  assert.deepEqual([review.status, review.stale, review.needsYou, review.reason], ["review", undefined, true, "review"]);
  const merged = annotateAgent({ id: "m", status: "done", pr: { state: "merged" } }, { live: true });
  assert.equal(merged.status, "merged");
});
