import { createHash } from "node:crypto";
import type { RudderGraph, TaskNode } from "../types.js";
import { validateIngress, type IngressDecision, type TaskIngress } from "./ingress.js";
import { deterministicTaskRoute } from "../task-routing.js";

/** Planner-private projection, not an engine ownership lease. The live engine
 * integration must authenticate this existing OwnerFence and commit the reducer's
 * result with its command receipt. Legacy headless uses updateGraph under its writer gate. */
export interface PlanningOwner { project_id: string; owner_id: string; epoch: number }
export type Permission = "read" | "plan-document" | "edit" | "integrate" | "publish" | "deploy";
export interface PlanningScope {
  checkoutPath: string;
  location: "local" | "cloud";
  workspaceMode?: "isolated" | "selected";
  permissions: Permission[];
  paths: string[];
}
export interface PlanningBudget { maxNodes: number; maxParallel: number; maxTokens: number }
export interface PlanningApproval {
  id: string;
  version: number;
  scope: PlanningScope;
  budget: PlanningBudget;
  acceptedContractVersions: Record<string, number>;
}
export interface OwnedNode {
  id: string;
  title: string;
  prompt: string;
  backend: TaskNode["backend"];
  model?: string;
  effort?: TaskNode["effort"];
  scope: PlanningScope;
  tokenBudget: number;
  deps: Array<{ on: string; type: "hard" | "soft" }>;
}
export interface LaunchReservation {
  claimId: string;
  planVersion: number;
  ownerEpoch: number;
  state: "reserved" | "started" | "uncertain" | "completed";
  runId?: string;
}
export interface OwnedPlan {
  formatVersion: 1;
  taskId: string;
  owner: PlanningOwner;
  ownerRunId: string;
  sessionId: string;
  ingress: TaskIngress;
  routing: NonNullable<IngressDecision["routing"]>;
  version: number;
  approvedVersion: number | null;
  approval: PlanningApproval;
  nodes: Record<string, OwnedNode>;
  cancelled: string[];
  launches: Record<string, LaunchReservation>;
  receipts: Record<string, { digest: string; version: number }>;
  activity: Array<{ version: number; reason: string; operation: string; nodeIds: string[] }>;
}
export type PlanningGraph = RudderGraph & { planningOwnership?: Record<string, OwnedPlan>; launchReservations?: Record<string, LaunchReservation> };
export interface PlanFence {
  taskId: string;
  owner: PlanningOwner;
  expectedVersion: number;
  expectedApprovalVersion: number;
  acceptedContractVersions: Record<string, number>;
  idempotencyKey: string;
}
export type ReplanOperation = { kind: "add"; node: OwnedNode } | { kind: "replace"; node: OwnedNode } | { kind: "cancel"; nodeId: string };
export interface ReplanCommand extends PlanFence { reason: string; operations: ReplanOperation[] }

function insist(condition: unknown, message: string): asserts condition { if (!condition) throw new Error(message); }
function positive(value: number): boolean { return Number.isSafeInteger(value) && value > 0; }
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical((value as Record<string, unknown>)[key])}`).join(",")}}`;
  return JSON.stringify(value);
}
function digest(value: unknown): string { return createHash("sha256").update(canonical(value)).digest("hex"); }
function same(a: unknown, b: unknown): boolean { return canonical(a) === canonical(b); }
function validOwner(owner: PlanningOwner): void {
  insist(owner && typeof owner.project_id === "string" && owner.project_id.trim()
    && typeof owner.owner_id === "string" && owner.owner_id.trim() && positive(owner.epoch), "Invalid planning owner fence");
}
function validScope(scope: PlanningScope): void {
  insist(scope && typeof scope.checkoutPath === "string" && scope.checkoutPath.startsWith("/") && !scope.checkoutPath.includes("\0")
    && (!scope.workspaceMode || ["isolated", "selected"].includes(scope.workspaceMode)) && ["local", "cloud"].includes(scope.location) && Array.isArray(scope.permissions) && Array.isArray(scope.paths), "Invalid planning scope");
  insist(scope.permissions.every(p => ["read", "plan-document", "edit", "integrate", "publish", "deploy"].includes(p))
    && scope.paths.every(p => typeof p === "string" && p.trim() && !p.includes("\0")), "Invalid planning permissions or paths");
}
function within(scope: PlanningScope, approved: PlanningScope): boolean {
  // Exact path labels, not prefix matching: ../ and sibling checkout tricks do
  // not acquire additional authority. The owner expands labels into file scopes.
  return scope.checkoutPath === approved.checkoutPath && scope.location === approved.location
    && (scope.workspaceMode ?? "isolated") === (approved.workspaceMode ?? "isolated")
    && scope.permissions.every(p => approved.permissions.includes(p)) && scope.paths.every(p => approved.paths.includes(p));
}
function validApproval(approval: PlanningApproval): void {
  insist(approval && typeof approval.id === "string" && approval.id.trim() && positive(approval.version), "Invalid approval version");
  validScope(approval.scope);
  insist(approval.budget && positive(approval.budget.maxNodes) && positive(approval.budget.maxParallel)
    && positive(approval.budget.maxTokens) && approval.budget.maxParallel <= approval.budget.maxNodes, "Invalid granted budget");
  insist(approval.acceptedContractVersions && typeof approval.acceptedContractVersions === "object" && !Array.isArray(approval.acceptedContractVersions) && Object.entries(approval.acceptedContractVersions).every(([key, version]) => key.trim() && positive(version)), "Invalid accepted contract versions");
}
function validateNodes(plan: OwnedPlan): void {
  const nodes = Object.values(plan.nodes);
  // Cancelled or completed allocations are retained. Replanning cannot recycle
  // the same grant into unbounded additional launches or token reservations.
  insist(nodes.length <= plan.approval.budget.maxNodes, "Replan exceeds granted node budget");
  let tokens = 0;
  for (const node of nodes) {
    insist(node && typeof node.id === "string" && /^[A-Za-z0-9_-]{1,100}$/u.test(node.id)
      && typeof node.title === "string" && node.title.trim() && typeof node.prompt === "string" && node.prompt.trim()
      && ["claude", "codex", "acpx", "opencode"].includes(node.backend), "Invalid owned node");
    validScope(node.scope);
    insist(node.scope.permissions.includes("edit"), "Implementation workers require explicit edit scope");
    insist(within(node.scope, plan.approval.scope), "Replan enlarges approved scope or execution location");
    insist(positive(node.tokenBudget), "A positive node token budget is required");
    tokens += node.tokenBudget;
    insist(Number.isSafeInteger(tokens) && tokens <= plan.approval.budget.maxTokens, "Replan exceeds granted token budget");
    insist(Array.isArray(node.deps) && node.deps.every(dep => dep && typeof dep.on === "string" && ["hard", "soft"].includes(dep.type)), "Invalid plan dependencies");
  }
  // Match the existing permissive missing-parent rule; soft edges never gate.
  const visiting = new Set<string>(), visited = new Set<string>();
  const visit = (id: string): void => {
    if (visited.has(id) || !plan.nodes[id] || plan.cancelled.includes(id)) return;
    insist(!visiting.has(id), "Replan contains a hard dependency cycle");
    visiting.add(id);
    for (const dep of plan.nodes[id].deps) if (dep.type === "hard") visit(dep.on);
    visiting.delete(id); visited.add(id);
  };
  for (const node of nodes) visit(node.id);
}

/** Admission is idempotent for the original ingress and authenticated owner.
 * A selected-agent message/control never becomes admission for a new fleet. */
export function admitOwnedPlan(graph: PlanningGraph, input: {
  taskId: string; owner: PlanningOwner; ownerRunId: string; sessionId: string;
  decision: IngressDecision; approval: PlanningApproval; nodes: OwnedNode[];
}): OwnedPlan {
  insist(input.decision.action === "new-task" && input.decision.routing, "Follow-ups and controls cannot create a plan");
  validateIngress(input.decision.ingress);
  insist(!input.decision.ingress.selectedAgent && !input.decision.ingress.control, "Addressed ingress cannot create another fleet");
  insist(["discuss", "execute", "plan"].includes(input.decision.routing.route), "Invalid task route");
  insist(deterministicTaskRoute(input.decision.ingress.task).route !== "discuss" || input.decision.routing.route === "discuss", "Read-only ingress cannot be promoted by a route proposal");
  validOwner(input.owner); validApproval(input.approval);
  insist(/^[A-Za-z0-9_-]{1,160}$/u.test(input.taskId) && !["__proto__", "constructor", "prototype"].includes(input.taskId) && input.ownerRunId && input.sessionId, "Task, owner run and session identity are required");
  const existing = graph.planningOwnership && Object.hasOwn(graph.planningOwnership, input.taskId) ? graph.planningOwnership[input.taskId] : undefined;
  const body = digest(input);
  if (existing) {
    insist(existing.receipts.admission?.digest === body, "Task admission identity was reused with different input");
    return existing;
  }
  insist(input.approval.scope.checkoutPath === input.decision.ingress.execution.checkoutPath
    && input.approval.scope.location === input.decision.ingress.execution.location
    && (input.approval.scope.workspaceMode ?? "isolated") === (input.decision.ingress.execution.workspaceMode ?? "isolated"), "Approval cannot change the selected execution checkout");
  if (input.decision.routing.route === "discuss") {
    insist(input.nodes.length === 0 && input.approval.scope.permissions.every(p => p === "read" || p === "plan-document"), "Discussion cannot authorize implementation workers");
  }
  if (input.decision.routing.route === "execute") insist(input.nodes.length <= 1, "Direct execution has one owner");
  const plan: OwnedPlan = structuredClone({
    formatVersion: 1, taskId: input.taskId, owner: input.owner, ownerRunId: input.ownerRunId, sessionId: input.sessionId,
    ingress: input.decision.ingress, routing: input.decision.routing, version: 1, approvedVersion: null,
    approval: input.approval, nodes: {}, cancelled: [], launches: {}, receipts: {}, activity: [],
  });
  for (const node of input.nodes) {
    insist(!Object.hasOwn(plan.nodes, node.id) && !Object.hasOwn(graph.nodes, node.id), "Plan node id is already in use");
    insist(!["__proto__", "constructor", "prototype"].includes(node.id), "Reserved node id");
    plan.nodes[node.id] = structuredClone(node);
  }
  validateNodes(plan);
  plan.receipts.admission = { digest: body, version: 1 };
  plan.activity.push({ version: 1, reason: input.decision.indicator, operation: "proposed", nodeIds: Object.keys(plan.nodes) });
  (graph.planningOwnership ??= {})[input.taskId] = plan;
  projectNodes(graph, plan, Object.keys(plan.nodes));
  return plan;
}

function fencedPlan(graph: PlanningGraph, command: PlanFence): OwnedPlan {
  const plan = graph.planningOwnership?.[command.taskId];
  insist(plan && Object.hasOwn(graph.planningOwnership!, command.taskId), "Unknown owned task"); validOwner(command.owner);
  insist(same(plan.owner, command.owner), "Stale planning owner epoch");
  insist(plan.approval.version === command.expectedApprovalVersion
    && same(plan.approval.acceptedContractVersions, command.acceptedContractVersions), "Stale approval or accepted contract versions");
  insist(typeof command.idempotencyKey === "string" && /^[A-Za-z0-9_-]{1,160}$/u.test(command.idempotencyKey)
    && !["admission", "__proto__", "constructor", "prototype"].includes(command.idempotencyKey), "Invalid planning idempotency key");
  return plan;
}
function retry(plan: OwnedPlan, key: string, body: unknown): boolean {
  const prior = Object.hasOwn(plan.receipts, key) ? plan.receipts[key] : undefined;
  if (!prior) return false;
  insist(prior.digest === digest(body), "Planning idempotency key reused with different input");
  return true;
}
function version(plan: OwnedPlan, expected: number): void { insist(plan.version === expected, "Stale plan version"); }

/** Commits add/replace/cancel atomically to the same graph as the launch gate.
 * Running/completed nodes are immutable; send/steer/interrupt use engine commands
 * and their own receipts. An interrupt request never cancels a launch ledger. */
export function replanOwnedTask(graph: PlanningGraph, command: ReplanCommand): OwnedPlan {
  const current = fencedPlan(graph, command);
  if (retry(current, command.idempotencyKey, command)) return current;
  version(current, command.expectedVersion);
  insist(command.reason?.trim() && command.reason.length <= 2000 && Array.isArray(command.operations)
    && command.operations.length > 0 && command.operations.length <= 256, "A bounded replan and visible reason are required");
  insist(current.routing.route !== "discuss", "Discussion cannot become an implementation plan without new authorization");
  const next = structuredClone(current), changed: string[] = [];
  for (const op of command.operations) {
    insist(["add", "replace", "cancel"].includes(op.kind), "Unknown replan operation");
    const id = op.kind === "cancel" ? op.nodeId : op.node.id;
    insist(!changed.includes(id), "Duplicate node operation"); changed.push(id);
    insist(typeof id === "string" && !["__proto__", "constructor", "prototype"].includes(id), "Invalid node id");
    const old = Object.hasOwn(next.nodes, id) ? next.nodes[id] : undefined;
    if (op.kind === "add") {
      insist(!Object.hasOwn(next.nodes, id) && !Object.hasOwn(graph.nodes, id), "Cannot reuse a node id");
      insist(!["__proto__", "constructor", "prototype"].includes(id), "Reserved node id");
      next.nodes[id] = structuredClone(op.node);
    } else {
      insist(old && !next.cancelled.includes(id) && !next.launches[id] && !graph.launchReservations?.[id]
        && graph.nodes[id]?.status === "planned" && !graph.nodes[id]?.runId, "Only pending, never-launched nodes can be replaced or cancelled");
      if (op.kind === "cancel") next.cancelled.push(id);
      else next.nodes[id] = structuredClone(op.node);
    }
  }
  if (next.routing.route === "execute") insist(Object.keys(next.nodes).length <= 1, "Direct execution cannot expand into a fleet");
  validateNodes(next);
  next.version++;
  next.approvedVersion = null; // Existing DAG semantics: changed plan is visible before approval.
  next.receipts[command.idempotencyKey] = { digest: digest(command), version: next.version };
  next.activity.push({ version: next.version, reason: command.reason, operation: "replanned", nodeIds: changed });
  projectNodes(graph, next, changed);
  graph.planningOwnership![command.taskId] = next;
  return next;
}

export function approveOwnedPlan(graph: PlanningGraph, command: PlanFence): OwnedPlan {
  const plan = fencedPlan(graph, command);
  const body = { operation: "approve", ...command };
  if (retry(plan, command.idempotencyKey, body)) return plan;
  version(plan, command.expectedVersion);
  validateNodes(plan);
  plan.approvedVersion = plan.version;
  plan.receipts[command.idempotencyKey] = { digest: digest(body), version: plan.version };
  plan.activity.push({ version: plan.version, reason: `Approved scope ${plan.approval.id} version ${plan.approval.version}`, operation: "approved", nodeIds: Object.keys(plan.nodes) });
  return plan;
}

function projectNodes(graph: PlanningGraph, plan: OwnedPlan, ids: string[]): void {
  const stamp = new Date().toISOString();
  for (const id of ids) {
    if (plan.cancelled.includes(id)) {
      // Keep the row and its dependencies so cancellation cannot turn a hard
      // parent into a satisfied dangling reference. History stays inspectable.
      const node = graph.nodes[id]; if (node) { node.status = "blocked"; node.updatedAt = stamp; }
      continue;
    }
    const node = plan.nodes[id], old = graph.nodes[id];
    const deps: string[] = [];
    for (const [edgeId, edge] of Object.entries(graph.edges)) if (edge.to === id) delete graph.edges[edgeId];
    for (const dep of node.deps) {
      const edgeId = `owned_${digest([dep.on, dep.type, id])}`;
      graph.edges[edgeId] = { id: edgeId, from: dep.on, to: id, type: dep.type }; deps.push(edgeId);
    }
    graph.nodes[id] = { id, title: node.title, prompt: node.prompt, backend: node.backend, ...(node.model ? { model: node.model } : {}), ...(node.effort ? { effort: node.effort } : {}), status: "planned", deps,
      source: "planner", createdAt: old?.createdAt ?? stamp, updatedAt: stamp };
  }
}

export function planForNode(graph: PlanningGraph, nodeId: string): OwnedPlan | undefined {
  return Object.values(graph.planningOwnership ?? {}).find(plan => Object.hasOwn(plan.nodes, nodeId));
}
export function planningLaunchAllowed(graph: PlanningGraph, nodeId: string): boolean {
  if (graph.launchReservations?.[nodeId]) return false;
  const plan = planForNode(graph, nodeId);
  if (!plan) return true; // Preserve legacy DAG semantics until explicit migration.
  if (plan.routing.route === "discuss" || plan.cancelled.includes(nodeId) || plan.launches[nodeId]
    || plan.approvedVersion !== plan.version) return false;
  const spent = Object.keys(plan.nodes).reduce((sum, id) => sum + (graph.nodes[id]?.tokens?.input ?? 0) + (graph.nodes[id]?.tokens?.output ?? 0), 0);
  if (spent >= plan.approval.budget.maxTokens) return false;
  const active = Object.keys(plan.launches).filter(id => {
    const launch = plan.launches[id];
    return launch.state === "reserved" || launch.state === "uncertain" || graph.nodes[id]?.status === "running";
  }).length;
  return active < plan.approval.budget.maxParallel;
}

/** Reserve before workspace/run/process creation. Uncertain reservations are
 * deliberately held across retry/restart; only observed reconciliation releases
 * capacity, and the same node identity can never launch a second time. */
export function reservePlanningLaunch(graph: PlanningGraph, nodeId: string, claimId: string): boolean {
  if (!graph.nodes[nodeId] || graph.nodes[nodeId].status !== "planned" || graph.nodes[nodeId].runId
    || !planningLaunchAllowed(graph, nodeId)) return false;
  const plan = planForNode(graph, nodeId);
  const reservation: LaunchReservation = { claimId, planVersion: plan?.version ?? 0, ownerEpoch: plan?.owner.epoch ?? 0, state: "reserved" };
  (graph.launchReservations ??= {})[nodeId] = reservation;
  if (plan) plan.launches[nodeId] = reservation;
  return true;
}
export function recordPlanningLaunch(graph: PlanningGraph, nodeId: string, claimId: string, state: LaunchReservation["state"], runId?: string): void {
  const reservation = graph.launchReservations?.[nodeId];
  insist(reservation && reservation.claimId === claimId, "Launch claim is stale");
  insist(!reservation.runId || !runId || reservation.runId === runId, "Launch run identity cannot change");
  insist(reservation.state !== "completed" || state === "completed", "Completed launch cannot restart");
  reservation.state = state;
  if (runId) reservation.runId = runId;
  const plan = planForNode(graph, nodeId);
  if (plan) plan.launches[nodeId] = structuredClone(reservation);
}

/** Engine callers supply the authenticated live fence, never a fence inferred
 * from task text. The engine commits this with its idempotent command receipt. */
export function reserveOwnedPlanningLaunch(graph: PlanningGraph, command: PlanFence & { nodeId: string; claimId: string }): boolean {
  const plan = fencedPlan(graph, command);
  version(plan, command.expectedVersion);
  insist(Object.hasOwn(plan.nodes, command.nodeId), "Node is outside the owned plan");
  return reservePlanningLaunch(graph, command.nodeId, command.claimId);
}

/** Pass references and granted scope into the actual worker contract as well as
 * retaining them in storage. Reference contents stay out of route classification. */
export function ownedWorkerPrompt(plan: OwnedPlan, nodeId: string): string {
  const node = plan.nodes[nodeId];
  insist(node, "Unknown owned worker");
  const references = [
    `Accountable owner: ${plan.ownerRunId}; task: ${plan.taskId}; plan version: ${plan.version}.`,
    `Approval: ${plan.approval.id} version ${plan.approval.version}; owner epoch: ${plan.owner.epoch}.`,
    `Execution: ${node.scope.location}, checkout ${JSON.stringify(node.scope.checkoutPath)}, workspace mode ${node.scope.workspaceMode ?? "isolated"}.`,
    `Granted permissions: ${node.scope.permissions.join(", ")}. Allowed path labels: ${JSON.stringify(node.scope.paths)}.`,
    `Worker token allocation: ${node.tokenBudget}; task grant: ${plan.approval.budget.maxTokens}; maximum parallel workers: ${plan.approval.budget.maxParallel}.`,
    `Accepted contract versions: ${JSON.stringify(plan.approval.acceptedContractVersions)}.`,
    "Preserve scope and prior work. The owner remains responsible for verification, integration and authorized delivery. Do not create another fleet or grant additional permissions.",
    `Attachment references: ${JSON.stringify(plan.ingress.attachments)}.`,
    `Conversation references: ${JSON.stringify(plan.ingress.conversationReferences)}.`,
  ];
  return `${node.prompt}\n\nRudder task ownership:\n${references.join("\n")}`;
}
