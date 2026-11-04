// The home scaler (observability/scaler/): its rules at their boundaries, the
// Prometheus queries folded into homes, the Fly executor's guards, and the
// /metrics it serves. Prometheus and Fly are fakes; no network, no Docker.
import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { ACTIONS, DEFAULT_CONFIG, STATES, evaluate, memoryStep } from "../observability/scaler/policy.mjs";
import { blankHome, collectHomes, createPrometheusClient, homeQueries } from "../observability/scaler/prometheus.mjs";
import { createFlyExecutor } from "../observability/scaler/fly.mjs";
import { emptyState, loadState, recordEvaluation, recordOutcome, saveState } from "../observability/scaler/state.mjs";
import { configFromEnv, createScaler, createServer, decisionChanges, scalerOptions } from "../observability/scaler/main.mjs";

const NOW = Date.parse("2026-11-02T22:00:00Z");
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const MiB = 1024 ** 2;
const GiB = 1024 ** 3;
// What the kernel reports as MemTotal on each machine size.
const MEM16 = 16379904 * 1024;
const MEM24 = (24576 - 500) * MiB;
const MEM32 = (32768 - 700) * MiB;

function home(overrides = {}) {
  return {
    homeId: "home-1",
    machineId: "m-1",
    region: "iad",
    memoryTotalBytes: MEM16,
    memoryAvailableBytes: 8 * GiB,
    swapTotalBytes: 4 * GiB,
    swapFreeBytes: 4 * GiB,
    memoryFullAvg60: 0.01,
    memoryFullAvg60Over30m: 0.01,
    availableRatioOver30mAvg: 0.5,
    usedRatioMax24h: 0.6,
    memoryFullMax24h: 0.05,
    memoryHistorySeconds: 86_400,
    diskWorkspaceTotalBytes: 50 * GiB,
    diskWorkspaceFreeBytes: 30 * GiB,
    agentsWorking: 0,
    agentsWaiting: 0,
    agentsIdle: 3,
    agentsAsleep: 1,
    busyMax15m: 0,
    cleanupRunsLast1h: 1,
    cleanupRunsLast15m: 0,
    lastSeenSeconds: 30,
    clients: 0,
    controlConnected: 1,
    ...overrides,
  };
}

const run = (input, options = {}) => evaluate([input], { now: NOW, ...options });
const find = (decisions, action) => decisions.filter((decision) => decision.action === action);
const one = (decisions, action) => {
  const found = find(decisions, action);
  assert.equal(found.length, 1, `expected one ${action} in ${JSON.stringify(decisions.map((d) => d.action))}`);
  return found[0];
};
const history = (entry) => ({ homes: { "home-1": entry } });
// Sustained pressure on an idle 16 GB home that already ran its cleanup.
const pressured = (overrides = {}) => home({ memoryFullAvg60Over30m: 0.15, ...overrides });
// A 24 GB home that used little memory all day.
const underused = (overrides = {}) => home({ memoryTotalBytes: MEM24, memoryAvailableBytes: 18 * GiB, usedRatioMax24h: 0.3, memoryFullMax24h: 0.002, ...overrides });

// ---------------------------------------------------------------------------
// policy.mjs
// ---------------------------------------------------------------------------

test("a healthy home gets no decisions", () => {
  assert.deepEqual(run(home()), []);
});

test("missing readings never act by themselves", () => {
  assert.deepEqual(evaluate([{ homeId: "home-1" }], { now: NOW }), []);
  assert.deepEqual(evaluate([blankHome("home-1", "m-1")], { now: NOW }), []);
  assert.deepEqual(evaluate([null, {}, { homeId: "" }], { now: NOW }), []);
  // Each trigger present on its own with the rest unknown.
  for (const field of ["memoryAvailableBytes", "swapTotalBytes", "memoryFullAvg60Over30m", "usedRatioMax24h", "diskWorkspaceFreeBytes"]) {
    assert.deepEqual(evaluate([{ ...blankHome("home-1", "m-1"), [field]: 0 }], { now: NOW }), [], field);
  }
  // A home with a short reading but unknown totals stays quiet.
  assert.deepEqual(run(home({ memoryTotalBytes: null, memoryAvailableBytes: 0, memoryFullAvg60: null })), []);
  assert.deepEqual(run(home({ diskWorkspaceTotalBytes: null, diskWorkspaceFreeBytes: 0 })), []);
});

test("run-cleanup fires when available memory drops below 12% of total", () => {
  const total = 25 * GiB; // 12% of it is exactly 3 GiB
  assert.deepEqual(find(run(home({ memoryTotalBytes: total, memoryAvailableBytes: 3 * GiB })), "run-cleanup"), []);
  const decision = one(run(home({ memoryTotalBytes: total, memoryAvailableBytes: 3 * GiB - 1 })), "run-cleanup");
  assert.equal(decision.state, "recommended");
  assert.match(decision.reason, /3\.0 GiB of 25\.0 GiB available \(12%\)/);
  assert.match(decision.reason, /No cleanup ran in the last 15 minutes/);
});

test("run-cleanup fires when memory full avg60 reaches 20%", () => {
  assert.equal(find(run(home({ memoryFullAvg60: 0.1999 })), "run-cleanup").length, 0);
  assert.match(one(run(home({ memoryFullAvg60: 0.2 })), "run-cleanup").reason, /memory full avg60 20%/);
});

test("run-cleanup waits 15 minutes after a cleanup run and names a home without cleanup metrics", () => {
  assert.equal(find(run(home({ memoryFullAvg60: 0.5, cleanupRunsLast15m: 1 })), "run-cleanup").length, 0);
  assert.equal(find(run(home({ memoryFullAvg60: 0.5, cleanupRunsLast15m: 0 })), "run-cleanup").length, 1);
  const unknown = one(run(home({ memoryFullAvg60: 0.5, cleanupRunsLast15m: null })), "run-cleanup");
  assert.match(unknown.reason, /reports no cleanup runs/);
});

test("add-swap only for a home with no swap that is short of memory", () => {
  const short = { memoryFullAvg60: 0.3 };
  const decision = one(run(home({ ...short, swapTotalBytes: 0, swapFreeBytes: 0 })), "add-swap");
  assert.equal(decision.state, "recommended");
  assert.match(decision.reason, /no swap/);
  assert.equal(find(run(home({ ...short, swapTotalBytes: null })), "add-swap").length, 0);
  assert.equal(find(run(home({ ...short, swapTotalBytes: 1 })), "add-swap").length, 0);
  assert.equal(find(run(home({ swapTotalBytes: 0, swapFreeBytes: 0 })), "add-swap").length, 0);
});

test("scale-up-memory fires at 10% sustained memory full avg60", () => {
  assert.equal(find(run(home({ memoryFullAvg60Over30m: 0.0999 })), "scale-up-memory").length, 0);
  const decision = one(run(home({ memoryFullAvg60Over30m: 0.1 })), "scale-up-memory");
  assert.equal(decision.state, "recommended");
  assert.deepEqual(decision.target, { memoryMb: 24576 });
  assert.equal(decision.machineId, "m-1");
  assert.match(decision.reason, /averaged 10% over 30 minutes/);
  assert.match(decision.reason, /Resize from 16384 MiB to 24576 MiB/);
});

test("scale-up-memory on low available memory needs swap more than half used", () => {
  const low = { availableRatioOver30mAvg: 0.0999, swapTotalBytes: 4 * GiB };
  assert.equal(find(run(home({ ...low, swapFreeBytes: 2 * GiB - 1 })), "scale-up-memory").length, 1);
  assert.equal(find(run(home({ ...low, swapFreeBytes: 2 * GiB })), "scale-up-memory").length, 0);
  assert.equal(find(run(home({ ...low, availableRatioOver30mAvg: 0.1, swapFreeBytes: 0 })), "scale-up-memory").length, 0);
  assert.equal(find(run(home({ ...low, swapTotalBytes: 0, swapFreeBytes: 0 })), "scale-up-memory").length, 0);
  assert.equal(find(run(home({ ...low, swapFreeBytes: null })), "scale-up-memory").length, 0);
});

test("scale-up-memory waits for the cleanup unless the home has no cleanup metrics", () => {
  assert.equal(find(run(pressured({ cleanupRunsLast1h: 0 })), "scale-up-memory").length, 0);
  assert.equal(find(run(pressured({ cleanupRunsLast1h: 1 })), "scale-up-memory").length, 1);
  const unknown = one(run(pressured({ cleanupRunsLast1h: null })), "scale-up-memory");
  assert.match(unknown.reason, /no cleanup metrics/);
});

test("scale-up-memory climbs one step and stops at the largest", () => {
  assert.deepEqual(one(run(pressured({ memoryTotalBytes: MEM24 })), "scale-up-memory").target, { memoryMb: 32768 });
  assert.equal(find(run(pressured({ memoryTotalBytes: MEM32 })), "scale-up-memory").length, 0);
  // A home sized by hand below the ladder goes to its first step.
  assert.deepEqual(one(run(pressured({ memoryTotalBytes: 7990 * MiB })), "scale-up-memory").target, { memoryMb: 16384 });
  assert.deepEqual(memoryStep(MEM16), { mb: MEM16 / MiB, index: 0, stepMb: 16384 });
  assert.equal(memoryStep(null), null);
});

test("a resize waits until no agent worked or waited for 15 minutes", () => {
  assert.equal(one(run(pressured({ busyMax15m: 0 })), "scale-up-memory").state, "recommended");
  const busy = one(run(pressured({ busyMax15m: 1 })), "scale-up-memory");
  assert.equal(busy.state, "waiting-for-idle");
  assert.match(busy.reason, /restarts the machine/);
  assert.equal(one(run(pressured({ busyMax15m: null })), "scale-up-memory").state, "waiting-for-idle");
  // A sample newer than the 15 minute window still counts.
  assert.equal(one(run(pressured({ busyMax15m: 0, agentsWaiting: 1 })), "scale-up-memory").state, "waiting-for-idle");
  assert.equal(one(run(underused({ busyMax15m: 2 })), "scale-down-memory").state, "waiting-for-idle");
  assert.match(one(run(pressured({ clients: 2 })), "scale-up-memory").reason, /2 attached clients will see it reconnect/);
});

test("scale-up-memory is in cooldown for 6 hours after any applied resize", () => {
  const applied = (action, at) => history({ applied: { [action]: { at } } });
  assert.equal(one(run(pressured(), { state: applied("scale-up-memory", NOW - 6 * HOUR + 1) }), "scale-up-memory").state, "cooldown");
  assert.equal(one(run(pressured(), { state: applied("scale-up-memory", NOW - 6 * HOUR) }), "scale-up-memory").state, "recommended");
  const afterDown = one(run(pressured(), { state: applied("scale-down-memory", NOW - HOUR) }), "scale-up-memory");
  assert.equal(afterDown.state, "cooldown");
  assert.match(afterDown.reason, /resized at 2026-11-02T21:00Z; waiting until 2026-11-03T03:00Z/);
  // An extend does not hold back a resize.
  assert.equal(one(run(pressured(), { state: applied("extend-volume", NOW - HOUR) }), "scale-up-memory").state, "recommended");
});

test("a failed attempt is shown as failed until the backoff ends", () => {
  const failedAt = (at) => history({ failed: { "scale-up-memory": { at, error: "HTTP 500" } } });
  const failed = one(run(pressured(), { state: failedAt(NOW - 10 * MINUTE) }), "scale-up-memory");
  assert.equal(failed.state, "failed");
  assert.match(failed.reason, /HTTP 500/);
  assert.equal(one(run(pressured(), { state: failedAt(NOW - DEFAULT_CONFIG.failureBackoffMs) }), "scale-up-memory").state, "recommended");
});

test("scale-down-memory at its boundaries", () => {
  const decision = one(run(underused({ usedRatioMax24h: 0.39, memoryFullMax24h: 0.009 })), "scale-down-memory");
  assert.equal(decision.state, "recommended");
  assert.deepEqual(decision.target, { memoryMb: 16384 });
  assert.match(decision.reason, /at most 39% used/);
  assert.deepEqual(one(run(underused({ memoryTotalBytes: MEM32 })), "scale-down-memory").target, { memoryMb: 24576 });
  assert.equal(find(run(underused({ usedRatioMax24h: 0.4 })), "scale-down-memory").length, 0);
  assert.equal(find(run(underused({ memoryFullMax24h: 0.01 })), "scale-down-memory").length, 0);
  assert.equal(find(run(underused({ memoryTotalBytes: MEM16 })), "scale-down-memory").length, 0);
  assert.equal(find(run(underused({ usedRatioMax24h: null })), "scale-down-memory").length, 0);
  assert.equal(find(run(underused({ memoryFullMax24h: null })), "scale-down-memory").length, 0);
  // Not on an hour of history, and not while memory is short right now.
  assert.equal(find(run(underused({ memoryHistorySeconds: 3600 })), "scale-down-memory").length, 0);
  assert.equal(find(run(underused({ memoryAvailableBytes: 1 * GiB })), "scale-down-memory").length, 0);
  // Hand-sized homes off the ladder are left alone.
  assert.equal(find(run(underused({ memoryTotalBytes: 20_000 * MiB })), "scale-down-memory").length, 0);
});

test("scale-down-memory is in cooldown for 24 hours after any applied resize", () => {
  const applied = (at) => history({ applied: { "scale-up-memory": { at } } });
  assert.equal(one(run(underused(), { state: applied(NOW - 24 * HOUR + 1) }), "scale-down-memory").state, "cooldown");
  assert.equal(one(run(underused(), { state: applied(NOW - 24 * HOUR) }), "scale-down-memory").state, "recommended");
});

test("up beats down in the same evaluation", () => {
  const decisions = run(underused({ memoryFullAvg60Over30m: 0.15 }));
  assert.equal(one(decisions, "scale-up-memory").target.memoryMb, 32768);
  assert.equal(find(decisions, "scale-down-memory").length, 0);
});

test("extend-volume at its boundaries, with the 500 GB cap and a 12 hour cooldown", () => {
  // 10% free of 100 GiB is 10 GiB, above the 8 GiB floor.
  assert.equal(find(run(home({ diskWorkspaceTotalBytes: 100 * GiB, diskWorkspaceFreeBytes: 10 * GiB })), "extend-volume").length, 0);
  const ratio = one(run(home({ diskWorkspaceTotalBytes: 100 * GiB, diskWorkspaceFreeBytes: 10 * GiB - 1 })), "extend-volume");
  assert.deepEqual(ratio.target, { sizeGb: 150 });
  assert.equal(ratio.state, "recommended");
  assert.equal(find(run(home({ diskWorkspaceTotalBytes: 50 * GiB, diskWorkspaceFreeBytes: 8 * GiB })), "extend-volume").length, 0);
  assert.deepEqual(one(run(home({ diskWorkspaceTotalBytes: 50 * GiB, diskWorkspaceFreeBytes: 8 * GiB - 1 })), "extend-volume").target, { sizeGb: 75 });
  // ceil(49.2 * 1.5) = 74
  assert.deepEqual(one(run(home({ diskWorkspaceTotalBytes: 49.2 * GiB, diskWorkspaceFreeBytes: GiB })), "extend-volume").target, { sizeGb: 74 });
  assert.deepEqual(one(run(home({ diskWorkspaceTotalBytes: 400 * GiB, diskWorkspaceFreeBytes: GiB })), "extend-volume").target, { sizeGb: 500 });
  const capped = one(run(home({ diskWorkspaceTotalBytes: 490 * GiB, diskWorkspaceFreeBytes: GiB })), "extend-volume");
  assert.equal(capped.target, undefined);
  assert.match(capped.reason, /500 GB cap/);
  // No restart, so busy agents do not hold it back.
  assert.equal(one(run(home({ busyMax15m: 5, diskWorkspaceFreeBytes: GiB })), "extend-volume").state, "recommended");
  const applied = (at) => history({ applied: { "extend-volume": { at } } });
  assert.equal(one(run(home({ diskWorkspaceFreeBytes: GiB }), { state: applied(NOW - 12 * HOUR + 1) }), "extend-volume").state, "cooldown");
  assert.equal(one(run(home({ diskWorkspaceFreeBytes: GiB }), { state: applied(NOW - 12 * HOUR) }), "extend-volume").state, "recommended");
});

test("investigate-silent after 10 minutes without a report, and nothing else for that home", () => {
  assert.deepEqual(run(home({ lastSeenSeconds: 600 })), []);
  const decisions = run(pressured({ lastSeenSeconds: 601, memoryFullAvg60: 0.5, controlConnected: 0, clients: 3 }));
  assert.deepEqual(decisions.map((decision) => [decision.action, decision.state]), [["investigate-silent", "recommended"]]);
  assert.match(decisions[0].reason, /10 minutes/);
  assert.match(decisions[0].reason, /disconnected/);
  assert.match(decisions[0].reason, /3 clients attached/);
  assert.deepEqual(run(home({ lastSeenSeconds: null })), []);
});

test("a silent home the control plane stopped or paused gets no decision at all", () => {
  const silent = { lastSeenSeconds: 900, memoryFullAvg60: 0.5, diskWorkspaceFreeBytes: GiB };
  for (const controlState of ["stopped", "paused", "failed"]) {
    assert.deepEqual(run(pressured({ ...silent, controlState })), [], controlState);
  }
  for (const controlState of ["running", "queued", null, undefined, ""]) {
    const decisions = run(pressured({ ...silent, controlState }));
    assert.deepEqual(decisions.map((decision) => decision.action), ["investigate-silent"], String(controlState));
  }
  assert.match(one(run(home({ ...silent, controlState: "running" })), "investigate-silent").reason, /lists it as running/);
  // The state only matters for a silent home; a reporting one is judged as before.
  assert.equal(one(run(pressured({ controlState: "stopped" })), "scale-up-memory").state, "recommended");
});

test("every decision has the documented shape and plain copy", () => {
  const decisions = evaluate([
    pressured({ homeId: "a", memoryFullAvg60: 0.5, swapTotalBytes: 0, swapFreeBytes: 0, diskWorkspaceFreeBytes: GiB }),
    underused({ homeId: "b" }),
    home({ homeId: "c", lastSeenSeconds: 4000 }),
  ], { now: NOW });
  assert.deepEqual(new Set(decisions.map((d) => d.action)), new Set(ACTIONS));
  for (const decision of decisions) {
    assert.ok(ACTIONS.includes(decision.action));
    assert.ok(STATES.includes(decision.state));
    assert.equal(typeof decision.reason, "string");
    assert.doesNotMatch(decision.reason, /[\u2013\u2014]/);
    assert.ok("machineId" in decision && "homeId" in decision);
  }
});

// ---------------------------------------------------------------------------
// prometheus.mjs
// ---------------------------------------------------------------------------

const reply = (status, body) => new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
const sample = (metric, value) => ({ metric, value: [NOW / 1000, String(value)] });

function fakePrometheus(byKey, { selector = "", fail = null } = {}) {
  const keyOf = new Map(Object.entries(homeQueries(selector)).map(([key, promql]) => [promql, key]));
  const seen = [];
  const fetch = async (url) => {
    const target = new URL(url);
    assert.equal(target.pathname, "/api/v1/query");
    const promql = target.searchParams.get("query");
    seen.push({ promql, time: target.searchParams.get("time") });
    const key = keyOf.get(promql);
    if (!key || key === fail) return reply(422, { status: "error", errorType: "execution", error: `cannot run ${key ?? promql}` });
    return reply(200, { status: "success", data: { resultType: "vector", result: byKey[key] ?? [] } });
  };
  return { fetch, seen };
}

// Prometheus answers that describe `homes` (policy-shaped rows).
function prometheusFor(homes) {
  const byKey = {};
  const add = (key, metric, value) => {
    if (value === null || value === undefined) return;
    (byKey[key] ??= []).push(sample(metric, value));
  };
  for (const h of homes) {
    const labels = { rudder_home_id: h.homeId, fly_machine_id: h.machineId };
    for (const [key, value] of Object.entries(h)) {
      if (["homeId", "machineId", "region", "clients", "controlConnected", "controlState"].includes(key) || key.startsWith("agents")) continue;
      add(key, key === "lastSeenSeconds" ? { ...labels, fly_region: h.region } : labels, value);
    }
    for (const [state, field] of Object.entries({ working: "agentsWorking", waiting: "agentsWaiting", idle: "agentsIdle", asleep: "agentsAsleep" })) {
      add("agents", { ...labels, state }, h[field]);
    }
    add("clients", { rudder_home_id: h.homeId }, h.clients);
    add("controlConnected", { rudder_home_id: h.homeId }, h.controlConnected);
    if (h.controlState) add("controlState", { rudder_home_id: h.homeId, state: h.controlState }, NOW / 1000);
  }
  return byKey;
}

test("collectHomes folds the queries into one row per home", async () => {
  const oldMachine = { rudder_home_id: "home-1", fly_machine_id: "m-old" };
  const h1 = { rudder_home_id: "home-1", fly_machine_id: "m-new" };
  const h2 = { rudder_home_id: "home-2", fly_machine_id: "m-2" };
  const { fetch, seen } = fakePrometheus({
    lastSeenSeconds: [sample({ ...oldMachine, fly_region: "ord" }, 900), sample({ ...h1, fly_region: "iad" }, 12.5), sample({ ...h2, fly_region: "fra" }, -3)],
    memoryTotalBytes: [sample(oldMachine, MEM16), sample(h1, MEM24), sample(h2, MEM16)],
    memoryAvailableBytes: [sample(oldMachine, GiB), sample(h1, 10 * GiB), sample(h2, "NaN")],
    swapTotalBytes: [sample(h1, 0)],
    memoryFullAvg60: [sample(h1, 0.25)],
    usedRatioMax24h: [sample(h2, "+Inf")],
    diskWorkspaceTotalBytes: [sample(h1, 50 * GiB)],
    diskWorkspaceFreeBytes: [sample(h1, 20 * GiB)],
    agents: [sample({ ...h1, state: "working" }, 2), sample({ ...h1, state: "idle" }, 5), sample({ ...h1, state: "stopped" }, 9)],
    busyMax15m: [sample(h1, 2)],
    cleanupRunsLast1h: [sample(h1, 1.0344)],
    cleanupRunsLast15m: [sample(h1, 0)],
    clients: [sample({ rudder_home_id: "home-1" }, 2)],
    controlConnected: [sample({ rudder_home_id: "home-1" }, 1), sample({ rudder_home_id: "home-9" }, 1)],
    // home-1 was running, then stopped; the running series still lingers.
    controlState: [
      sample({ rudder_home_id: "home-1", state: "running" }, NOW / 1000 - 200),
      sample({ rudder_home_id: "home-1", state: "stopped" }, NOW / 1000 - 20),
      sample({ rudder_home_id: "home-9", state: "running" }, NOW / 1000),
    ],
  });
  const homes = await collectHomes(createPrometheusClient({ url: "http://prom.test:9090", fetch }), { time: NOW / 1000 });

  assert.deepEqual(homes.map((h) => h.homeId), ["home-1", "home-2"]);
  const [one1, two] = homes;
  assert.equal(one1.machineId, "m-new");
  assert.equal(one1.region, "iad");
  assert.equal(one1.lastSeenSeconds, 12.5);
  assert.equal(one1.memoryTotalBytes, MEM24);
  assert.equal(one1.memoryAvailableBytes, 10 * GiB);
  assert.equal(one1.swapTotalBytes, 0);
  assert.equal(one1.memoryFullAvg60, 0.25);
  assert.equal(one1.diskWorkspaceFreeBytes, 20 * GiB);
  assert.deepEqual([one1.agentsWorking, one1.agentsWaiting, one1.agentsIdle, one1.agentsAsleep], [2, 0, 5, 0]);
  assert.equal(one1.busyMax15m, 2);
  assert.equal(one1.cleanupRunsLast1h, 1);
  assert.equal(one1.cleanupRunsLast15m, 0);
  assert.equal(one1.clients, 2);
  assert.equal(one1.controlConnected, 1);
  assert.equal(one1.controlState, "stopped");
  assert.equal(two.controlState, null);
  assert.equal(two.memoryAvailableBytes, null);
  assert.equal(two.usedRatioMax24h, null);
  assert.equal(two.lastSeenSeconds, 0);
  assert.equal(two.agentsWorking, null);
  assert.equal(two.cleanupRunsLast1h, null);
  assert.equal(two.clients, null);
  assert.deepEqual(Object.keys(two).sort(), Object.keys(blankHome("x")).sort());

  // Every query ran once, at the same evaluation time.
  assert.equal(seen.length, Object.keys(homeQueries()).length);
  assert.deepEqual(new Set(seen.map((s) => s.time)), new Set([(NOW / 1000).toFixed(3)]));
  assert.ok(seen.some((s) => s.promql.includes('rudder_home_agents{state=~"working|waiting"}')));
  assert.ok(seen.some((s) => s.promql.includes("rudder_control_home_clients")));
});

test("collectHomes reads a tie between control-plane states as the one that expects the home to run", async () => {
  const labels = { rudder_home_id: "home-1", fly_machine_id: "m-1" };
  const { fetch } = fakePrometheus({
    lastSeenSeconds: [sample(labels, 900)],
    controlState: [
      sample({ rudder_home_id: "home-1", state: "paused" }, NOW / 1000),
      sample({ rudder_home_id: "home-1", state: "running" }, NOW / 1000),
    ],
  });
  const [only] = await collectHomes(createPrometheusClient({ url: "http://prom.test:9090", fetch }));
  assert.equal(only.controlState, "running");
});

test("collectHomes fails as a whole when any query fails or Prometheus is unreachable", async () => {
  const { fetch } = fakePrometheus({}, { fail: "cleanupRunsLast1h" });
  await assert.rejects(collectHomes(createPrometheusClient({ url: "http://prom.test:9090", fetch })), /Prometheus query failed \(execution\)/);
  const unreachable = async () => {
    throw new TypeError("fetch failed", { cause: { code: "ECONNREFUSED" } });
  };
  await assert.rejects(collectHomes(createPrometheusClient({ url: "http://prom.test:9090", fetch: unreachable })), /unreachable: ECONNREFUSED/);
});

test("the selector reaches every query", () => {
  const selector = 'deployment_environment_name="production"';
  for (const [key, promql] of Object.entries(homeQueries(selector))) {
    // Every metric name (rudder_home_id is a label) is followed by matchers
    // that end with the selector.
    const metrics = [...promql.matchAll(/\brudder_(?:home|control)_[a-z_]+(\{[^}]*\})?/g)].filter((match) => !match[0].startsWith("rudder_home_id"));
    assert.ok(metrics.length > 0, key);
    for (const match of metrics) assert.ok(match[1]?.endsWith(`${selector}}`), `${key}: ${match[0]}`);
  }
  assert.doesNotMatch(Object.values(homeQueries()).join("\n"), /\{\}/, "no empty matchers without a selector");
});

// ---------------------------------------------------------------------------
// fly.mjs
// ---------------------------------------------------------------------------

function flyMachine(overrides = {}) {
  return {
    id: "m-1",
    instance_id: "inst-1",
    state: "started",
    region: "iad",
    config: {
      image: "registry.fly.io/rudder-workers-exla:v1",
      env: { RUDDER_HOME: "1", RUDDER_WORKSPACE_ID: "home-1", RUDDER_OTHER: "kept" },
      guest: { cpu_kind: "performance", cpus: 4, memory_mb: 16384 },
      mounts: [{ volume: "vol-cache", path: "/cache" }, { volume: "vol-ws", path: "/workspace" }],
      services: [{ internal_port: 8080, ports: [{ port: 443 }] }],
      metadata: { rudder: "home" },
      restart: { policy: "always" },
    },
    ...overrides,
  };
}

function fakeFly({ machine = flyMachine(), volume = { id: "vol-ws", size_gb: 50, attached_machine_id: "m-1" }, leaseStatus = 201 } = {}) {
  const calls = [];
  const base = "/v1/apps/rudder-workers-exla";
  const fetch = async (url, init = {}) => {
    const target = new URL(url);
    const method = init.method ?? "GET";
    const body = init.body ? JSON.parse(init.body) : undefined;
    calls.push({ method, path: target.pathname, body, headers: init.headers ?? {} });
    const machinePath = `${base}/machines/${machine.id}`;
    if (target.pathname === machinePath && method === "GET") return reply(200, machine);
    if (target.pathname === `${machinePath}/lease` && method === "POST") {
      return reply(leaseStatus, leaseStatus < 300 ? { status: "success", data: { nonce: "nonce-1", expires_at: NOW / 1000 + 120, owner: "scaler" } } : { error: "lease held" });
    }
    if (target.pathname === `${machinePath}/lease` && method === "DELETE") return reply(200, { status: "success" });
    if (target.pathname === machinePath && method === "POST") return reply(200, { ...machine, config: body.config, instance_id: "inst-2", state: "replacing" });
    if (target.pathname === `${base}/volumes/${volume.id}` && method === "GET") return reply(200, volume);
    if (target.pathname === `${base}/volumes/${volume.id}/extend` && method === "PUT") return reply(200, { needs_restart: true, volume: { ...volume, size_gb: body.size_gb } });
    return reply(404, { error: "not found" });
  };
  return { fetch, calls };
}

const upDecision = (overrides = {}) => ({ homeId: "home-1", machineId: "m-1", action: "scale-up-memory", state: "recommended", reason: "x", target: { memoryMb: 24576 }, ...overrides });
const extendDecision = (overrides = {}) => ({ homeId: "home-1", machineId: "m-1", action: "extend-volume", state: "recommended", reason: "x", target: { sizeGb: 75 }, ...overrides });
const executor = (fly, apply) => createFlyExecutor({ token: "tok", app: "rudder-workers-exla", fetch: fly.fetch, apply });
const mutating = (calls) => calls.filter((call) => call.method !== "GET");

test("the executor refuses a machine Fly does not show as that home", async () => {
  for (const env of [{ RUDDER_HOME: "1", RUDDER_WORKSPACE_ID: "home-2" }, { RUDDER_WORKSPACE_ID: "home-1" }, {}]) {
    const machine = flyMachine();
    machine.config.env = env;
    for (const decision of [upDecision(), extendDecision()]) {
      const fly = fakeFly({ machine });
      const result = await executor(fly, true).execute(decision);
      assert.equal(result.ok, false);
      assert.match(result.refused, /home/);
      assert.deepEqual(mutating(fly.calls), [], "nothing is leased or changed");
    }
  }
  const missing = fakeFly({ machine: flyMachine({ id: "m-other" }) });
  assert.match((await executor(missing, true).execute(upDecision())).refused, /no machine m-1/);
});

test("a dry run reads Fly at most and changes nothing", async () => {
  const fly = fakeFly();
  const ex = executor(fly, false);
  assert.equal(ex.apply, false);
  const resize = await ex.execute(upDecision());
  assert.equal(resize.dryRun, true);
  assert.equal(resize.verified, true);
  assert.equal(resize.wouldDo.change, "guest.memory_mb 16384 -> 24576");
  const grow = await ex.execute(extendDecision());
  assert.equal(grow.dryRun, true);
  assert.equal(grow.wouldDo.request, "PUT /apps/rudder-workers-exla/volumes/vol-ws/extend");
  assert.equal(grow.wouldDo.change, "size_gb 50 -> 75");
  assert.ok(fly.calls.length > 0);
  assert.deepEqual(mutating(fly.calls), []);

  // apply without a token is a dry run that sends nothing at all.
  const silent = fakeFly();
  const tokenless = createFlyExecutor({ token: null, app: "rudder-workers-exla", fetch: silent.fetch, apply: true });
  assert.equal(tokenless.apply, false);
  const result = await tokenless.execute(upDecision());
  assert.equal(result.dryRun, true);
  assert.equal(result.verified, false);
  assert.deepEqual(silent.calls, []);
});

test("applying a resize sends the full config with only the memory changed", async () => {
  const machine = flyMachine();
  const fly = fakeFly({ machine });
  const result = await executor(fly, true).execute(upDecision());
  assert.equal(result.ok, true);
  assert.equal(result.applied, true);
  assert.deepEqual(result.memoryMb, { from: 16384, to: 24576 });
  assert.deepEqual(fly.calls.map((call) => `${call.method} ${call.path}`), [
    "GET /v1/apps/rudder-workers-exla/machines/m-1",
    "POST /v1/apps/rudder-workers-exla/machines/m-1/lease",
    "GET /v1/apps/rudder-workers-exla/machines/m-1",
    "POST /v1/apps/rudder-workers-exla/machines/m-1",
    "DELETE /v1/apps/rudder-workers-exla/machines/m-1/lease",
  ]);
  const update = fly.calls[3];
  const expected = structuredClone(machine.config);
  expected.guest.memory_mb = 24576;
  assert.deepEqual(update.body, { config: expected, current_version: "inst-1" });
  assert.equal(update.headers["fly-machine-lease-nonce"], "nonce-1");
  assert.equal(update.headers.Authorization, "Bearer tok");
  assert.equal(fly.calls[4].headers["fly-machine-lease-nonce"], "nonce-1");
  // The machine read from Fly was not modified in place.
  assert.equal(machine.config.guest.memory_mb, 16384);
});

test("the executor resizes only recommended decisions in the right direction", async () => {
  for (const state of ["waiting-for-idle", "cooldown", "failed", "applied"]) {
    const fly = fakeFly();
    const result = await executor(fly, true).execute(upDecision({ state }));
    assert.match(result.refused, new RegExp(state));
    assert.deepEqual(fly.calls, []);
  }
  const bigger = flyMachine();
  bigger.config.guest.memory_mb = 32768;
  const fly = fakeFly({ machine: bigger });
  assert.match((await executor(fly, true).execute(upDecision())).refused, /already has 32768 MiB/);
  assert.deepEqual(mutating(fly.calls), []);

  const done = flyMachine();
  done.config.guest.memory_mb = 24576;
  const reconciled = await executor(fakeFly({ machine: done }), true).execute(upDecision());
  assert.equal(reconciled.applied, true);
  assert.equal(reconciled.reconciled, true);

  const stopped = fakeFly({ machine: flyMachine({ state: "stopped" }) });
  assert.match((await executor(stopped, true).execute(upDecision())).refused, /stopped/);

  const leased = fakeFly({ leaseStatus: 409 });
  const result = await executor(leased, true).execute(upDecision());
  assert.match(result.refused, /lease/);
  assert.equal(leased.calls.filter((call) => call.method === "POST" && !call.path.endsWith("/lease")).length, 0);
});

test("applying an extend grows the /workspace volume and never restarts", async () => {
  const fly = fakeFly();
  const result = await executor(fly, true).execute(extendDecision());
  assert.equal(result.applied, true);
  assert.equal(result.volumeId, "vol-ws");
  assert.deepEqual(result.sizeGb, { from: 50, to: 75 });
  assert.equal(result.needsRestart, true);
  assert.deepEqual(mutating(fly.calls).map((call) => [call.method, call.path, call.body]), [
    ["PUT", "/v1/apps/rudder-workers-exla/volumes/vol-ws/extend", { size_gb: 75 }],
  ]);

  const grown = await executor(fakeFly({ volume: { id: "vol-ws", size_gb: 80, attached_machine_id: "m-1" } }), true).execute(extendDecision());
  assert.equal(grown.reconciled, true);
  const elsewhere = await executor(fakeFly({ volume: { id: "vol-ws", size_gb: 50, attached_machine_id: "m-9" } }), true).execute(extendDecision());
  assert.match(elsewhere.refused, /attached to another machine/);
  const noMount = flyMachine();
  noMount.config.mounts = [{ volume: "vol-cache", path: "/cache" }];
  assert.match((await executor(fakeFly({ machine: noMount }), true).execute(extendDecision())).refused, /no volume mounted at \/workspace/);
});

// ---------------------------------------------------------------------------
// state.mjs and main.mjs
// ---------------------------------------------------------------------------

function tempDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), "rudder-scaler-"));
}

test("the state file round-trips and survives a corrupt copy", () => {
  const dir = tempDir();
  const file = path.join(dir, "nested", "state.json");
  assert.deepEqual(loadState(file), emptyState());
  let state = recordEvaluation(emptyState(), { homes: [home()], decisions: [{ homeId: "home-1", action: "run-cleanup" }], now: NOW });
  state = recordOutcome(state, upDecision(), { ok: true, applied: true }, NOW);
  state = recordOutcome(state, extendDecision(), { ok: false, refused: "no" }, NOW);
  saveState(file, state);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ["state.json"]);
  const loaded = loadState(file);
  assert.equal(loaded.homes["home-1"].applied["scale-up-memory"].at, NOW);
  assert.equal(loaded.homes["home-1"].failed["extend-volume"].error, "refused: no");
  assert.equal(loaded.homes["home-1"].lastSeenAt, NOW - 30_000);
  assert.equal(loaded.decisions.length, 1);
  // A home unseen for a week is forgotten.
  assert.deepEqual(recordEvaluation(loaded, { now: NOW + 8 * 24 * HOUR }).homes, {});

  fs.writeFileSync(file, "{ not json");
  const logs = [];
  assert.deepEqual(loadState(file, { log: (line) => logs.push(line) }), emptyState());
  assert.match(logs[0], /unreadable/);
  assert.ok(fs.readdirSync(path.dirname(file)).some((name) => name.startsWith("state.json.corrupt-")));
});

test("thresholds can be overridden from the environment", () => {
  const config = configFromEnv({
    RUDDER_SCALER_SHORT_AVAILABLE_RATIO: "0.15",
    RUDDER_SCALER_MEMORY_STEPS_MB: "8192, 16384",
    RUDDER_SCALER_SCALE_UP_COOLDOWN_MS: "60000",
  });
  assert.equal(config.shortAvailableRatio, 0.15);
  assert.deepEqual(config.memoryStepsMb, [8192, 16384]);
  assert.equal(config.scaleUpCooldownMs, 60000);
  assert.equal(config.volumeMaxGb, DEFAULT_CONFIG.volumeMaxGb);
  assert.throws(() => configFromEnv({ RUDDER_SCALER_MEMORY_STEPS_MB: "16384,8192" }), /ascending/);
  assert.throws(() => configFromEnv({ RUDDER_SCALER_SILENT_AFTER_SECONDS: "soon" }), /number/);
  const options = scalerOptions({ RUDDER_SCALER_APPLY: "1" });
  assert.equal(options.apply, false, "apply needs a Fly token too");
  assert.equal(options.port, 9464);
  assert.equal(options.flyApp, "rudder-workers-exla");
  assert.equal(scalerOptions({ RUDDER_SCALER_APPLY: "1", FLY_API_TOKEN: "t" }).apply, true);
});

test("decision changes are logged once", () => {
  const a = { homeId: "h", action: "scale-up-memory", state: "waiting-for-idle", reason: "r", target: { memoryMb: 24576 } };
  assert.equal(decisionChanges([], [a]).length, 1);
  assert.deepEqual(decisionChanges([a], [{ ...a, reason: "numbers moved" }]), []);
  assert.match(decisionChanges([a], [{ ...a, state: "recommended" }])[0], /h scale-up-memory recommended \(target 24576 MiB\)/);
  assert.deepEqual(decisionChanges([a], []), ["h scale-up-memory cleared"]);
});

// One fetch for both fakes, routed by host.
function fakeWorld(homes, fly = fakeFly()) {
  const prometheus = { byKey: prometheusFor(homes), down: false, queries: 0 };
  const keyOf = new Map(Object.entries(homeQueries()).map(([key, promql]) => [promql, key]));
  const fetch = async (url, init) => {
    const target = new URL(url);
    if (target.host === "api.machines.dev") return fly.fetch(url, init);
    if (prometheus.down) throw new TypeError("fetch failed", { cause: { code: "ECONNREFUSED" } });
    prometheus.queries += 1;
    const key = keyOf.get(target.searchParams.get("query"));
    return reply(200, { status: "success", data: { resultType: "vector", result: prometheus.byKey[key] ?? [] } });
  };
  return { fetch, prometheus, fly };
}

function scalerFor(world, env = {}) {
  const dir = tempDir();
  const options = scalerOptions({ PROMETHEUS_URL: "http://prom.test:9090", RUDDER_SCALER_STATE: path.join(dir, "state.json"), ...env });
  const logs = [];
  let clock = NOW;
  const scaler = createScaler({ options, fetch: world.fetch, log: (line) => logs.push(line), now: () => clock });
  return { scaler, logs, options, advance: (ms) => { clock += ms; } };
}

async function listen(server) {
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return `http://127.0.0.1:${server.address().port}`;
}

test("/metrics serves the scaler metrics, and an unreachable Prometheus counts as an error", async () => {
  const world = fakeWorld([pressured(), home({ homeId: "home-2", machineId: "m-2" })]);
  const { scaler, logs } = scalerFor(world);
  assert.equal((await scaler.evaluateOnce()).ok, true);
  assert.equal(logs.length, 1);
  assert.match(logs[0], /home-1 scale-up-memory recommended \(target 24576 MiB\)/);

  const server = createServer(scaler);
  const base = await listen(server);
  try {
    const response = await fetch(`${base}/metrics`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /^text\/plain; version=0\.0\.4/);
    const text = await response.text();
    for (const [name, type] of [
      ["rudder_scaler_decision", "gauge"],
      ["rudder_scaler_target_memory_bytes", "gauge"],
      ["rudder_scaler_evaluations_total", "counter"],
      ["rudder_scaler_last_evaluation_timestamp_seconds", "gauge"],
      ["rudder_scaler_apply_enabled", "gauge"],
    ]) {
      assert.match(text, new RegExp(`^# HELP ${name} \\S`, "m"));
      assert.match(text, new RegExp(`^# TYPE ${name} ${type}$`, "m"));
    }
    const samples = text.split("\n").filter((line) => line && !line.startsWith("#"));
    for (const line of samples) assert.match(line, /^[a-z_]+(\{[a-z_]+="[^"]*"(,[a-z_]+="[^"]*")*\})? -?[\d.e+]+$/, line);
    assert.ok(samples.includes('rudder_scaler_decision{rudder_home_id="home-1",action="scale-up-memory",state="recommended"} 1'));
    assert.ok(samples.includes(`rudder_scaler_target_memory_bytes{rudder_home_id="home-1"} ${24576 * MiB}`));
    assert.ok(samples.includes('rudder_scaler_evaluations_total{outcome="ok"} 1'));
    assert.ok(samples.includes('rudder_scaler_evaluations_total{outcome="error"} 0'));
    assert.ok(samples.includes(`rudder_scaler_last_evaluation_timestamp_seconds ${NOW / 1000}`));
    assert.ok(samples.includes("rudder_scaler_apply_enabled 0"));
    assert.ok(!samples.some((line) => line.includes("home-2")));

    const decisions = await (await fetch(`${base}/decisions`)).json();
    assert.equal(decisions.apply, false);
    assert.equal(decisions.decisions[0].action, "scale-up-memory");
    assert.equal(decisions.decisions[0].executor.dryRun, true);
    assert.equal((await (await fetch(`${base}/healthz`)).json()).ok, true);
    assert.equal((await fetch(`${base}/nope`)).status, 404);

    world.prometheus.down = true;
    assert.equal((await scaler.evaluateOnce()).ok, false);
    assert.equal((await scaler.evaluateOnce()).ok, false);
    assert.equal(logs.filter((line) => /evaluation failed: .*ECONNREFUSED/.test(line)).length, 1, "a repeated error is logged once");
    const after = await (await fetch(`${base}/metrics`)).text();
    assert.match(after, /^rudder_scaler_evaluations_total\{outcome="error"\} 2$/m);
    assert.match(after, /^rudder_scaler_evaluations_total\{outcome="ok"\} 1$/m);
    assert.equal((await fetch(`${base}/healthz`)).status, 200);
  } finally {
    server.close();
  }
});

test("end to end, a silent home is reported unless the control plane stopped it", async () => {
  const world = fakeWorld([
    home({ homeId: "home-1", machineId: "m-1", lastSeenSeconds: 900, controlState: "stopped" }),
    home({ homeId: "home-2", machineId: "m-2", lastSeenSeconds: 900, controlState: "running" }),
    home({ homeId: "home-3", machineId: "m-3", lastSeenSeconds: 900 }),
  ]);
  const { scaler } = scalerFor(world);
  await scaler.evaluateOnce();
  assert.deepEqual(scaler.snapshot().decisions.map((d) => [d.homeId, d.action]), [["home-2", "investigate-silent"], ["home-3", "investigate-silent"]]);
});

test("a dry run with a Fly token only reads, and asks once per decision", async () => {
  const world = fakeWorld([pressured(), home({ homeId: "home-2", machineId: "m-2", diskWorkspaceFreeBytes: GiB })]);
  const { scaler } = scalerFor(world, { FLY_API_TOKEN: "tok" });
  await scaler.evaluateOnce();
  await scaler.evaluateOnce();
  assert.ok(world.fly.calls.length > 0);
  assert.deepEqual(mutating(world.fly.calls), []);
  const reads = world.fly.calls.length;
  await scaler.evaluateOnce();
  assert.equal(world.fly.calls.length, reads, "an unchanged decision does not ask Fly again");
  assert.equal(scaler.snapshot().decisions.find((d) => d.action === "scale-up-memory").state, "recommended");
  assert.match(scaler.metrics(), /^rudder_scaler_apply_enabled 0$/m);
});

test("with apply on, a resize is applied once and then in cooldown", async () => {
  const world = fakeWorld([pressured()]);
  const { scaler, options, advance } = scalerFor(world, { RUDDER_SCALER_APPLY: "1", FLY_API_TOKEN: "tok" });
  await scaler.evaluateOnce();
  const applied = scaler.snapshot().decisions.find((d) => d.action === "scale-up-memory");
  assert.equal(applied.state, "applied");
  assert.match(applied.reason, /Applied: memory 16384 -> 24576 MiB/);
  assert.match(scaler.metrics(), /^rudder_scaler_apply_enabled 1$/m);
  assert.doesNotMatch(scaler.metrics(), /rudder_scaler_target_memory_bytes\{/);
  const saved = JSON.parse(fs.readFileSync(options.statePath, "utf8"));
  assert.equal(saved.homes["home-1"].applied["scale-up-memory"].at, NOW);

  advance(MINUTE);
  await scaler.evaluateOnce();
  assert.equal(scaler.snapshot().decisions.find((d) => d.action === "scale-up-memory").state, "cooldown");
  assert.equal(world.fly.calls.filter((call) => call.method === "POST" && !call.path.endsWith("/lease")).length, 1);

  // A restarted scaler remembers the cooldown.
  const again = createScaler({ options, fetch: world.fetch, log: () => {}, now: () => NOW + 2 * MINUTE });
  await again.evaluateOnce();
  assert.equal(again.snapshot().decisions.find((d) => d.action === "scale-up-memory").state, "cooldown");
});

test("with apply on, a refusal is recorded as failed and backs off", async () => {
  const machine = flyMachine();
  machine.config.env.RUDDER_WORKSPACE_ID = "someone-else";
  const world = fakeWorld([pressured()], fakeFly({ machine }));
  const { scaler, advance } = scalerFor(world, { RUDDER_SCALER_APPLY: "1", FLY_API_TOKEN: "tok" });
  await scaler.evaluateOnce();
  const failed = scaler.snapshot().decisions.find((d) => d.action === "scale-up-memory");
  assert.equal(failed.state, "failed");
  assert.match(failed.reason, /refused/);
  assert.match(scaler.metrics(), /rudder_scaler_target_memory_bytes\{rudder_home_id="home-1"\} /);
  const calls = world.fly.calls.length;
  advance(MINUTE);
  await scaler.evaluateOnce();
  assert.equal(scaler.snapshot().decisions.find((d) => d.action === "scale-up-memory").state, "failed");
  assert.equal(world.fly.calls.length, calls, "no retry inside the backoff");
  assert.deepEqual(mutating(world.fly.calls), []);
});
