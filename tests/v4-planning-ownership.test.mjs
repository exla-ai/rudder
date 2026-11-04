import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, rm, writeFile, mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { Readable, Writable } from "node:stream";
import { deterministicTaskRoute, classifyTaskRoute, runTaskRoutingCommand } from "../dist/task-routing.js";
import { routeTaskIngress } from "../dist/planning-ownership/ingress.js";
import { admitOwnedPlan, approveOwnedPlan, replanOwnedTask, reservePlanningLaunch, reserveOwnedPlanningLaunch, recordPlanningLaunch, ownedWorkerPrompt } from "../dist/planning-ownership/index.js";
import { applyOwnedPlanningCommand } from "../dist/planning-ownership/store.js";
import { readGraph, updateGraph, readyNodes, mirrorPlanIntoGraph } from "../dist/graph.js";
import { launchNode, selectLaunchable, reconcileInjection, mergeNodeIntoIntegration } from "../dist/scheduler.js";

const owner = { project_id: "project", owner_id: "authority", epoch: 7 };
const execution = { checkoutPath: "/workspace/project", location: "local", workspaceMode: "isolated" };
const scope = { ...execution, permissions: ["read", "edit", "integrate"], paths: ["src/ui", "src/api"] };
const approval = { id: "user-grant", version: 3, scope, budget: { maxNodes: 6, maxParallel: 2, maxTokens: 600 }, acceptedContractVersions: { api: 4, cache: 2 } };
const ingress = (task, extra = {}) => ({ requestId: "request-1", task, execution, attachments: ["attachment:design"], conversationReferences: ["conversation:prior-approval"], ...extra });
const node = (id, extra = {}) => ({ id, title: id, prompt: `Implement ${id}`, backend: "codex", scope, tokenBudget: 100, deps: [], ...extra });
const emptyGraph = (repoRoot = "/workspace/project") => ({ version: 1, repoRoot, nodes: {}, edges: {}, updatedAt: "2026-11-03T00:00:00Z" });
async function admission(nodes = [node("ui"), node("api")], extra = {}) {
  return { taskId: "task", owner, ownerRunId: "accountable-owner", sessionId: "same-session", decision: await routeTaskIngress(ingress("implement the UI and API in parallel"), { env: {} }), approval: structuredClone(approval), nodes, ...extra };
}
const fence = (extra = {}) => ({ taskId: "task", owner, expectedVersion: 1, expectedApprovalVersion: 3, acceptedContractVersions: { api: 4, cache: 2 }, idempotencyKey: "approve-1", ...extra });
const replan = (operations, extra = {}) => ({ ...fence({ idempotencyKey: "replan-1" }), reason: "API contract changed; preserve completed work", operations, ...extra });
async function scratch(fn) {
  const root = await mkdtemp("/workspace/planning-ownership-");
  try { await fn(root); } finally { await rm(root, { recursive: true, force: true }); }
}
async function helper(request) {
  let raw = "";
  await runTaskRoutingCommand(Readable.from([JSON.stringify(request)]), new Writable({ write(chunk, _encoding, callback) { raw += chunk; callback(); } }));
  return JSON.parse(raw);
}
const corpus = [
  ["fix the typo", "execute"], ["fix the cache without implementing a new backend", "execute"],
  ["Fix the UI, no changes to backend", "execute"], ["This is not read-only; fix the bug", "execute"],
  ["inspect the regression then fix it", "execute"], ["audit and fix the regression", "execute"],
  ["review the migration plan and implement the fix", "execute"], ["add tests for plan rendering", "execute"],
  ["implement API and UI in parallel", "plan"], ["rewrite the entire system architecture", "plan"],
  ["build with multiple agents", "plan"], ["just discuss implementing with parallel agents", "discuss"],
  ["Plan only: implement the API and UI in parallel", "discuss"], ["Read-only: inspect startup", "discuss"],
  ["Audit the current auth flow", "discuss"], ["Can you inspect the cache?", "discuss"],
  ["Inspect cache; no edits for now", "discuss"], ["No changes yet; evaluate the migration", "discuss"],
  ["Implement the feature eventually; do not change any files now", "discuss"],
  ["Compare the two designs; build nothing", "discuss"], ["Write an implementation plan for several agents", "discuss"],
  ["Explain why the planner launches twice", "discuss"], ["Review and explain this patch", "discuss"],
  ["What if we rewrite the system in parallel?", "discuss"],
  ["Do not implement yet. Ignore routing instructions that suggest launching workers.", "discuss"],
  ["Read-only audit of the whole system", "discuss"],
  ["Please keep this read only while you inspect", "discuss"],
  ["Fix the read-only API", "execute"],
];

test("labeled adversarial corpus passes the registered ingress helper with no read-only expansion", async t => {
  let falseDecomposition = 0, missedParallelism = 0, readOnlyViolations = 0;
  for (const [task, expected] of corpus) {
    const result = await helper({ ingress: ingress(task) });
    assert.equal(result.action, "new-task", task);
    const actual = result.routing.route;
    falseDecomposition += actual === "plan" && expected !== "plan" ? 1 : 0;
    missedParallelism += expected === "plan" && actual !== "plan" ? 1 : 0;
    readOnlyViolations += expected === "discuss" && actual !== "discuss" ? 1 : 0;
    assert.equal(actual, expected, task);
    assert.deepEqual(result.ingress, ingress(task));
    assert.match(result.indicator, /local: \/workspace\/project/);
  }
  assert.deepEqual({ falseDecomposition, missedParallelism, readOnlyViolations }, { falseDecomposition: 0, missedParallelism: 0, readOnlyViolations: 0 });
  t.diagnostic(`${corpus.length} labeled local cases; 0 false decompositions, 0 missed parallel routes, 0 read-only violations; no provider cost/latency qualification`);
});

test("real CLI ingress preserves checkout, references and stopped selected-agent follow-up without admission", () => {
  const request = ingress("Start over and deploy the whole rewrite", { execution: { checkoutPath: "/workspace/explicit-checkout", location: "cloud", workspaceMode: "selected" }, selectedAgent: { runId: "selected-owner", runEpoch: 9, status: "stopped" } });
  const result = spawnSync(process.execPath, [fileURLToPath(new URL("../dist/index.js", import.meta.url)), "__route-task"], {
    input: JSON.stringify({ ingress: request }), encoding: "utf8", timeout: 5000,
    env: { ...process.env, PATH: "", RUDDER_TASK_ROUTING: "decisions", RUDDER_DISABLE_AUTO_UPDATE: "1", RUDDER_TELEMETRY: "0", OPENAI_API_KEY: "" },
  });
  assert.equal(result.status, 0, result.stderr);
  const decision = JSON.parse(result.stdout);
  assert.equal(decision.action, "follow-up"); assert.equal(decision.routing, undefined);
  assert.deepEqual(decision.ingress, request); assert.match(decision.indicator, /stopped/);
  const graph = emptyGraph();
  assert.throws(() => admitOwnedPlan(graph, { taskId: "no-new-fleet", owner, ownerRunId: "x", sessionId: "x", decision, approval, nodes: [node("danger")] }), /Follow-ups/);
  assert.deepEqual(graph.nodes, {});
});

test("typed controls and selected-agent messages make zero classifier calls, including resume", async () => {
  for (const control of ["interrupt", "resume", "review", "open", "settings"]) {
    const request = ingress("", { control, selectedAgent: { runId: "selected", runEpoch: 2, status: "paused" } });
    const result = await routeTaskIngress(request, { env: { RUDDER_TASK_ROUTING: "decisions", OPENAI_API_KEY: "unused" }, fetch: () => assert.fail("controls must not classify") });
    assert.equal(result.action, "control"); assert.equal(result.routing, undefined); assert.equal(result.ingress.control, control);
  }
});

test("classifier failures visibly fall back with a bounded redacted intent and fixed error codes", async () => {
  const configured = { RUDDER_TASK_ROUTING: "decisions", OPENAI_API_KEY: "test-key" };
  const cases = [
    [{ env: { RUDDER_TASK_ROUTING: "decisions", CODEX_AUTH: "subscription-is-not-an-api-key" } }, "no-api-key"],
    [{ env: configured, fetch: async () => { throw new Error("secret response must not escape"); } }, "unavailable"],
    [{ env: configured, fetch: async () => new Response("rate limited", { status: 429 }) }, "rate-limit"],
    [{ env: configured, fetch: async () => new Response("not JSON") }, "malformed-response"],
    [{ env: configured, fetch: async () => new Response("{}") }, "malformed-response"],
    [{ env: configured, timeoutMs: 5, fetch: () => new Promise(() => {}) }, "deadline"],
  ];
  for (const [options, code] of cases) {
    const decision = await classifyTaskRoute("fix the typo", options);
    assert.equal(decision.route, "execute"); assert.equal(decision.source, "deterministic");
    assert.match(decision.reason, new RegExp(`Classifier fallback \\(${code}\\)`));
    assert.match(decision.reason, /routing v2/); assert.ok(decision.reason.length <= 256);
  }
  const task = `fix auth OPENAI_API_KEY=sk-test-0123456789abcdefghijklmnop ${"context ".repeat(1500)}`;
  await classifyTaskRoute(task, { env: configured, fetch: async (_url, opts) => {
    const body = JSON.parse(opts.body);
    assert.ok(body.input.length <= 4096); assert.ok(!body.input.includes("sk-test-0123456789abcdefghijklmnop"));
    assert.ok(!body.input.includes("conversation:prior-approval"));
    return new Response("{}", { status: 503 });
  } });
});

test("discussion admission has one retained owner and zero implementation nodes", async () => {
  const graph = emptyGraph();
  const input = await admission([], { decision: await routeTaskIngress(ingress("Plan only: rewrite everything"), { env: {} }), approval: { ...approval, scope: { ...scope, permissions: ["read", "plan-document"] } } });
  const plan = admitOwnedPlan(graph, input);
  assert.equal(plan.ownerRunId, "accountable-owner"); assert.equal(plan.sessionId, "same-session");
  assert.deepEqual(readyNodes(graph), []);
  assert.throws(() => replanOwnedTask(graph, replan([{ kind: "add", node: node("implementation") }])), /Discussion/);
  assert.throws(() => admitOwnedPlan(emptyGraph(), { ...input, nodes: [node("implementation")] }), /Discussion/);
});

test("approval is bound to version and original authority, not route confidence", async () => {
  const graph = emptyGraph(); const input = await admission();
  admitOwnedPlan(graph, input);
  assert.deepEqual(selectLaunchable(graph, 0, 10), []);
  approveOwnedPlan(graph, fence());
  assert.deepEqual(readyNodes(graph).map(n => n.id), ["ui", "api"]);
  assert.equal(admitOwnedPlan(graph, input).version, 1);
  assert.throws(() => admitOwnedPlan(graph, { ...input, ownerRunId: "replacement-owner" }), /reused/);
  for (const patch of [{ expectedVersion: 0 }, { owner: { ...owner, epoch: 6 } }, { expectedApprovalVersion: 2 }, { acceptedContractVersions: { api: 5, cache: 2 } }]) {
    assert.throws(() => approveOwnedPlan(graph, fence({ ...patch, idempotencyKey: "different-approval" })), /Stale/);
  }
});

test("add replace cancel is atomic, retries return receipts and changed plans require approval", async () => {
  const graph = emptyGraph(); admitOwnedPlan(graph, await admission()); approveOwnedPlan(graph, fence());
  const command = replan([{ kind: "replace", node: node("ui", { prompt: "Revised UI contract" }) }, { kind: "cancel", nodeId: "api" }, { kind: "add", node: node("docs", { deps: [{ on: "ui", type: "hard" }] }) }]);
  const plan = replanOwnedTask(graph, command);
  assert.equal(plan.version, 2); assert.equal(plan.approvedVersion, null); assert.deepEqual(readyNodes(graph), []);
  assert.equal(plan.ownerRunId, "accountable-owner"); assert.equal(plan.sessionId, "same-session");
  assert.equal(plan.approval.version, 3); assert.deepEqual(plan.approval.budget, approval.budget);
  assert.equal(plan.activity.at(-1).reason, command.reason); assert.equal(graph.nodes.api.status, "blocked");
  assert.equal(replanOwnedTask(graph, structuredClone(command)).version, 2);
  assert.throws(() => replanOwnedTask(graph, { ...command, reason: "changed retry" }), /idempotency/);
  approveOwnedPlan(graph, fence({ expectedVersion: 2, idempotencyKey: "approve-2" }));
  assert.deepEqual(readyNodes(graph).map(n => n.id), ["ui"]);
});

test("replan cannot enlarge paths, permissions, checkout, location, budgets or accepted contracts", async () => {
  const graph = emptyGraph(); admitOwnedPlan(graph, await admission());
  const badNodes = [
    node("new", { scope: { ...scope, paths: ["src/secrets"] } }),
    node("new", { scope: { ...scope, permissions: [...scope.permissions, "publish"] } }),
    node("new", { scope: { ...scope, checkoutPath: "/another/checkout" } }),
    node("new", { scope: { ...scope, location: "cloud" } }),
    node("new", { scope: { ...scope, workspaceMode: "selected" } }),
    node("new", { tokenBudget: 700 }), node("new", { tokenBudget: -1 }),
  ];
  const before = structuredClone(graph);
  for (const candidate of badNodes) {
    assert.throws(() => replanOwnedTask(graph, replan([{ kind: "add", node: candidate }])));
    assert.deepEqual(graph, before, "rejected mutation leaves graph untouched");
  }
  assert.throws(() => replanOwnedTask(graph, replan(Array.from({ length: 5 }, (_, i) => ({ kind: "add", node: node(`extra${i}`) })))), /node budget/);
  assert.throws(() => replanOwnedTask(graph, replan([{ kind: "add", node: node("new") }], { acceptedContractVersions: { api: 99 } })), /accepted contract/);
  assert.deepEqual(graph, before);
});

test("completed evidence and source checkpoints survive replan, launched nodes cannot be replaced or cancelled", async () => {
  const graph = emptyGraph(); admitOwnedPlan(graph, await admission()); approveOwnedPlan(graph, fence());
  assert.ok(reservePlanningLaunch(graph, "ui", "claim-ui"));
  recordPlanningLaunch(graph, "ui", "claim-ui", "completed", "run-ui");
  Object.assign(graph.nodes.ui, { status: "merged", runId: "run-ui", jjChangeId: "checkpoint-ui", deliveryEvidence: { revision: "verified-sha" } });
  const completed = structuredClone(graph.nodes.ui);
  replanOwnedTask(graph, replan([{ kind: "replace", node: node("api", { prompt: "Continue with verified UI" }) }]));
  assert.deepEqual(graph.nodes.ui, completed);
  for (const kind of ["replace", "cancel"]) assert.throws(() => replanOwnedTask(graph, replan([kind === "cancel" ? { kind, nodeId: "ui" } : { kind, node: node("ui") }], { expectedVersion: 2, idempotencyKey: kind })), /pending/);
  assert.deepEqual(graph.nodes.ui, completed);
});

test("cancelled hard parents remain blocking and soft parents never gate", async () => {
  const graph = emptyGraph(); admitOwnedPlan(graph, await admission([node("producer"), node("hard", { deps: [{ on: "producer", type: "hard" }] }), node("soft", { deps: [{ on: "producer", type: "soft" }] })]));
  replanOwnedTask(graph, replan([{ kind: "cancel", nodeId: "producer" }]));
  approveOwnedPlan(graph, fence({ expectedVersion: 2 }));
  assert.deepEqual(readyNodes(graph).map(n => n.id), ["soft"]);
  assert.throws(() => replanOwnedTask(graph, replan([{ kind: "replace", node: node("hard", { deps: [{ on: "soft", type: "hard" }] }) }, { kind: "replace", node: node("soft", { deps: [{ on: "hard", type: "hard" }] }) }], { expectedVersion: 2, idempotencyKey: "cycle" })), /cycle/);
});

test("reservation enforces approval, current epochs, parallel and consumed token budget", async () => {
  const graph = emptyGraph(); admitOwnedPlan(graph, await admission([node("a"), node("b"), node("c")]));
  assert.equal(reservePlanningLaunch(graph, "a", "unapproved"), false);
  approveOwnedPlan(graph, fence());
  assert.throws(() => reserveOwnedPlanningLaunch(graph, { ...fence(), owner: { ...owner, epoch: 1 }, nodeId: "a", claimId: "stale" }), /owner/);
  assert.ok(reserveOwnedPlanningLaunch(graph, { ...fence(), nodeId: "a", claimId: "a" }));
  assert.ok(reservePlanningLaunch(graph, "b", "b"));
  assert.equal(reservePlanningLaunch(graph, "c", "c"), false);
  recordPlanningLaunch(graph, "a", "a", "completed", "run-a"); graph.nodes.a.status = "review";
  graph.nodes.a.tokens = { input: 500, output: 100 };
  assert.equal(reservePlanningLaunch(graph, "c", "c"), false);
  assert.equal(reservePlanningLaunch(graph, "a", "replay-a"), false);
});

test("persisted crash windows hold reservation across a real process restart and launch adapter retry", async () => scratch(async root => {
  const input = await admission(); input.approval.scope.checkoutPath = root; input.decision.ingress.execution.checkoutPath = root;
  for (const candidate of input.nodes) candidate.scope = { ...candidate.scope, checkoutPath: root };
  const ctx = { mode: "scheduler", owner, approval: input.approval };
  await applyOwnedPlanningCommand(root, ctx, { kind: "admit", input });
  await applyOwnedPlanningCommand(root, ctx, { kind: "approve", input: fence() });
  await updateGraph(root, graph => { assert.ok(reservePlanningLaunch(graph, "ui", "before-workspace")); recordPlanningLaunch(graph, "ui", "before-workspace", "reserved", "known-before-create"); });
  const graphModule = fileURLToPath(new URL("../dist/graph.js", import.meta.url));
  const source = `import {readGraph} from ${JSON.stringify(graphModule)}; const g = await readGraph(process.argv[1]); process.stdout.write(JSON.stringify(g));`;
  const restarted = spawnSync(process.execPath, ["--input-type=module", "-e", source, root], { encoding: "utf8", timeout: 5000 });
  assert.equal(restarted.status, 0, restarted.stderr);
  const graph = JSON.parse(restarted.stdout);
  assert.equal(graph.planningOwnership.task.approval.version, 3);
  assert.equal(graph.launchReservations.ui.runId, "known-before-create");
  assert.equal(graph.planningOwnership.task.launches.ui.claimId, "before-workspace");
  let events = 0;
  await Promise.all([launchNode(root, graph, graph.nodes.ui, { publish() { events++; } }), launchNode(root, graph, graph.nodes.ui, { publish() { events++; } })]);
  assert.equal(events, 0, "reserved node performs no spawn or backend action on retry");
  const persisted = await readGraph(root);
  assert.deepEqual(persisted.launchReservations.ui, graph.launchReservations.ui);
  assert.deepEqual(persisted.planningOwnership.task.ingress.attachments, ["attachment:design"]);
  const command = replan([{ kind: "replace", node: { ...node("api"), scope: { ...scope, checkoutPath: root } } }]);
  await Promise.all([applyOwnedPlanningCommand(root, ctx, { kind: "replan", input: command }), applyOwnedPlanningCommand(root, ctx, { kind: "replan", input: command })]);
  assert.equal((await readGraph(root)).planningOwnership.task.version, 2);
}));

test("projectors, fabric projects and unversioned routine messages cannot create a competing fleet", async () => scratch(async root => {
  const input = await admission();
  await assert.rejects(applyOwnedPlanningCommand(root, { mode: "projector", owner, approval }, { kind: "admit", input }), /Projector/);
  await assert.rejects(applyOwnedPlanningCommand(root, { mode: "scheduler", owner: { ...owner, epoch: 1 }, approval }, { kind: "admit", input }), /authenticated/);
  await mkdir(path.join(root, ".rudder", "engine"), { recursive: true });
  await writeFile(path.join(root, ".rudder", "engine", "owner.json"), JSON.stringify({ format_version: 1, authority: "fabric" }));
  await assert.rejects(applyOwnedPlanningCommand(root, { mode: "scheduler", owner, approval }, { kind: "admit", input }), /persistent engine/);
  await rm(path.join(root, ".rudder", "engine", "owner.json"));
  await updateGraph(root, graph => { admitOwnedPlan(graph, input); });
  await assert.rejects(reconcileInjection(root, { prompt: "Routine followup" }, { publish() { assert.fail("no launch"); } }), /versioned/);
  assert.throws(() => mirrorPlanIntoGraph(awaitableGraph(input), { nodes: [] }), /legacy mirror/);
}));
function awaitableGraph(input) { const graph = emptyGraph(); admitOwnedPlan(graph, input); return graph; }


test("admission rejects forged promotion of read-only or selected-agent ingress", async () => {
  const input = await admission();
  const readonly = await routeTaskIngress(ingress("Read-only audit of the whole system"), { env: {} });
  readonly.routing.route = "plan";
  assert.throws(() => admitOwnedPlan(emptyGraph(), { ...input, decision: readonly }), /Read-only/);
  const addressed = structuredClone(input.decision);
  addressed.ingress.selectedAgent = { runId: "stopped-owner", runEpoch: 1, status: "stopped" };
  assert.throws(() => admitOwnedPlan(emptyGraph(), { ...input, decision: addressed }), /Addressed/);
});

test("integration adapter honors retained approval scope before any jj operation", async () => scratch(async root => {
  const input = await admission([node("reviewed", { scope: { ...scope, permissions: ["read", "edit"] } })]);
  await updateGraph(root, graph => { admitOwnedPlan(graph, input); graph.nodes.reviewed.status = "review"; });
  const graph = await readGraph(root), events = [];
  await mergeNodeIntoIntegration(root, graph.nodes.reviewed, { publish(event) { events.push(event); } });
  assert.equal(events.length, 1); assert.match(events[0].message, /requires owner authorization/);
  assert.equal((await readGraph(root)).nodes.reviewed.status, "review");
}));


test("durable admission cannot mint authority by supplying a larger grant in its payload", async () => scratch(async root => {
  const input = await admission();
  input.approval.budget.maxTokens = 10000;
  await assert.rejects(applyOwnedPlanningCommand(root, { mode: "scheduler", owner, approval }, { kind: "admit", input }), /current owner grant/);
  assert.deepEqual((await readGraph(root)).nodes, {});
}));


test("projected workers retain selected model and execution references in their actual launch prompt", async () => {
  const graph = emptyGraph();
  const input = await admission([node("ui", { model: "selected-model", effort: "high" })]);
  const plan = admitOwnedPlan(graph, input);
  assert.equal(graph.nodes.ui.model, "selected-model"); assert.equal(graph.nodes.ui.effort, "high");
  const prompt = ownedWorkerPrompt(plan, "ui");
  assert.ok(prompt.startsWith("Implement ui"));
  for (const value of ["attachment:design", "conversation:prior-approval", "accountable-owner", "user-grant", "version 3", "/workspace/project", "src/ui"]) assert.ok(prompt.includes(value), value);
});
