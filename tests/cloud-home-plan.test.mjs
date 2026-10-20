// The plan the home machine reports for each project (cloud/worker/plan.mjs):
// graph.json nodes and edges plus each plan's approval gate from plan-queue.json.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { planFromGraph, plansFromQueue, readProjectPlan } from "../cloud/worker/plan.mjs";

function repo() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-home-plan-"));
  fs.mkdirSync(path.join(dir, ".rudder"), { recursive: true });
  return dir;
}

function write(dir, name, value) {
  const file = path.join(dir, ".rudder", name);
  fs.writeFileSync(file, typeof value === "string" ? value : JSON.stringify(value));
  // Make the mtime move even within one clock tick so the cache re-reads.
  const at = new Date(Date.now() + Math.floor(Math.random() * 100000));
  fs.utimesSync(file, at, at);
}

const graph = {
  version: 1,
  repoRoot: "/x",
  updatedAt: "2026-10-20T10:00:00.000Z",
  nodes: {
    n1: { id: "n1", title: "API", status: "running", runId: "run-1", backend: "codex", model: "gpt-6.1-sol", prompt: "p", createdAt: "2026-10-20T09:00:00Z" },
    n0: { id: "n0", title: "Schema", status: "merged", backend: "claude", prompt: "p", createdAt: "2026-10-20T09:00:00Z" },
    n2: { id: "n2", title: "UI", status: "weird", backend: "claude", prompt: "p", createdAt: "2026-10-20T09:00:00Z" },
  },
  edges: {
    a: { id: "a", from: "n0", to: "n1", type: "hard" },
    b: { id: "b", from: "n1", to: "n2", type: "soft" },
    c: { id: "c", from: "n0", to: "n2", type: "hard" },
    bad: { id: "bad", from: "", to: "n2", type: "hard" },
  },
};

test("graph.json maps to plan nodes with deps, in planner order", () => {
  const plan = planFromGraph(graph);
  assert.equal(plan.updatedAt, "2026-10-20T10:00:00.000Z");
  assert.deepEqual(plan.nodes.map((node) => node.id), ["n0", "n1", "n2"]);
  assert.deepEqual(plan.nodes[1], {
    id: "n1",
    title: "API",
    status: "running",
    runId: "run-1",
    backend: "codex",
    model: "gpt-6.1-sol",
    deps: [{ on: "n0", type: "hard" }],
  });
  assert.equal(plan.nodes[2].status, "planned", "unknown statuses read as planned");
  assert.deepEqual(plan.nodes[2].deps, [{ on: "n0", type: "hard" }, { on: "n1", type: "soft" }]);
  assert.equal(plan.nodes[0].runId, null);
  assert.equal(plan.nodes[0].model, null);
});

test("malformed graphs read as no plan", () => {
  assert.equal(planFromGraph(null), null);
  assert.equal(planFromGraph({ version: 2, nodes: {}, edges: {} }), null);
  assert.equal(planFromGraph({ version: 1, nodes: [], edges: {} }), null);
  assert.deepEqual(planFromGraph({ version: 1, nodes: { x: null, y: { title: "no id" } }, edges: null }).nodes, []);
});

test("plan-queue.json gives each plan's approval gate, old single-plan files too", () => {
  assert.deepEqual(plansFromQueue({ plans: [{ id: "plan-1", awaiting_approval: true, plan_request: "build it" }, { id: "plan-2" }] }), [
    { id: "plan-1", awaitingApproval: true, request: "build it" },
    { id: "plan-2", awaitingApproval: false, request: null },
  ]);
  assert.deepEqual(plansFromQueue({ awaiting_approval: true, planned_nodes: [] }), [{ id: "", awaitingApproval: true, request: null }]);
  assert.deepEqual(plansFromQueue({ plans: [] }), []);
  assert.deepEqual(plansFromQueue("nope"), []);
});

test("readProjectPlan combines both files, re-reads on change, and never throws", () => {
  const dir = repo();
  try {
    assert.equal(readProjectPlan(dir), null, "no files, no plan");
    write(dir, "graph.json", graph);
    const first = readProjectPlan(dir);
    assert.equal(first.nodes.length, 3);
    assert.equal(first.awaitingApproval, false);
    assert.deepEqual(first.plans, []);

    write(dir, "plan-queue.json", { plans: [{ id: "plan-1", awaiting_approval: true }] });
    const gated = readProjectPlan(dir);
    assert.equal(gated.awaitingApproval, true);
    assert.deepEqual(gated.plans, [{ id: "plan-1", awaitingApproval: true, request: null }]);

    write(dir, "graph.json", "{ not json");
    const broken = readProjectPlan(dir);
    assert.deepEqual(broken.nodes, [], "a broken graph with a plan at the gate still shows the gate");
    assert.equal(broken.awaitingApproval, true);

    write(dir, "plan-queue.json", "garbage");
    assert.equal(readProjectPlan(dir), null);

    write(dir, "graph.json", { ...graph, nodes: { n9: { id: "n9", title: "Later", status: "review" } }, edges: {} });
    assert.deepEqual(readProjectPlan(dir).nodes.map((node) => [node.id, node.status]), [["n9", "review"]]);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
