import { randomUUID } from "node:crypto";
import { reservePlanningLaunch, recordPlanningLaunch, planForNode, ownedWorkerPrompt, type PlanningGraph } from "./planning-ownership/index.js";
import type { RudderBus } from "./bus.js";
import { assertLegacyWriterAllowed } from "./engine-authority.js";
import {
  createNodeWorkspace,
  currentJjChangeId,
  integrationTrunk,
  jjDiff,
  listConflicts,
  mergeJjRunIntoCurrentWorkspace,
  squashResolutionIntoMerge,
} from "./jj.js";
import {
  dependents,
  frontier as graphFrontier,
  hardParents,
  judgeParents,
  newEdgeId,
  newNodeId,
  projectNodeStatus,
  readGraph,
  readyNodes,
  isReady,
  updateGraph,
} from "./graph.js";
import { reconcile } from "./planner.js";
import { DEFAULT_SUCCESS, deriveGoal, formatGoalPrompt, normalizeGoalLine } from "./goal.js";
import { configuredBackend, createRunRecord, loadConfig, loadRunRecord, runDir, saveRunRecord } from "./state.js";
import { newAttemptId, spawnWorker } from "./run-manager.js";
import { renderLiveRudderMd } from "./surfaces.js";
import type {
  GraphEdge,
  InferredDep,
  JsonValue,
  PlanNode,
  ResolverContext,
  RudderGraph,
  RunRecord,
  TaskNode,
} from "./types.js";
import { currentBranch } from "./git.js";
import path from "node:path";
import { newRunId, nowIso, writeJson } from "./util.js";

// ===========================================================================
// PURE decision functions. No IO, no spawning: these are the unit-tested core
// of the scheduler so tests never launch a worker.
// ===========================================================================

/**
 * The set of nodes to launch this tick: ready nodes (planned + all hard parents
 * merged) sorted oldest-first, sliced to the remaining parallel capacity.
 */
export function selectLaunchable(graph: RudderGraph, runningCount: number, maxParallel: number): TaskNode[] {
  const capacity = Math.max(0, maxParallel - runningCount);
  if (capacity === 0) {
    return [];
  }
  return readyNodes(graph)
    .slice()
    .sort((a, b) => a.createdAt.localeCompare(b.createdAt))
    .slice(0, capacity);
}

/**
 * When a node fails or is cancelled, the hard dependents (direct + transitive)
 * that can no longer ever start must become blocked. Soft dependents are
 * unaffected (they only ever received the parent's diff as context).
 */
export function computeBlocked(graph: RudderGraph, failedOrCancelledNodeId: string): string[] {
  const blocked = new Set<string>();
  const queue = [failedOrCancelledNodeId];
  while (queue.length) {
    const current = queue.shift() as string;
    for (const childId of dependents(graph, current)) {
      const child = graph.nodes[childId];
      if (!child) {
        continue;
      }
      // Only hard dependents are blocked; the edge from `current` must be hard.
      const isHardChild = hardParents(graph, childId).includes(current);
      if (!isHardChild) {
        continue;
      }
      if (child.status === "merged" || blocked.has(childId)) {
        continue;
      }
      blocked.add(childId);
      queue.push(childId);
    }
  }
  return [...blocked];
}

/**
 * Build the resolver-agent prompt. Pure: given the conflicted merge change, the
 * two merged titles, and the conflicted files, it instructs the worker to open
 * each file, resolve preserving BOTH intents, and remove all conflict markers.
 */
export function buildResolverPrompt(input: {
  mergeChangeId: string;
  intoTitle: string;
  nodeTitle: string;
  conflictedFiles: string[];
}): string {
  const files = input.conflictedFiles.length ? input.conflictedFiles.join(", ") : "(none reported)";
  return formatGoalPrompt({
    goal: `resolve the merge conflict between ${input.intoTitle} and ${input.nodeTitle}, preserving both intents`,
    success: "no conflict markers remain and `jj resolve --list` is empty",
    body: [
      `You are resolving a merge conflict at jj change ${input.mergeChangeId}, a merge of ${input.intoTitle} and ${input.nodeTitle}.`,
      `These files have conflict markers: ${files}.`,
      "Open each, resolve the conflict preserving BOTH intents, and remove all conflict markers.",
      "Do not touch unrelated files.",
      "When done, the merge must have no remaining conflicts.",
    ].join(" "),
  });
}

/**
 * PURE decision: should a completed resolver run finalize its original node?
 * Yes when the run is a resolver (has resolverFor) and no conflicts remain in
 * its workspace. The IO (listConflicts) is supplied by the caller so this stays
 * unit-testable without jj.
 */
export function resolverShouldFinalize(input: {
  resolverFor: string | undefined;
  remainingConflicts: string[];
}): boolean {
  return Boolean(input.resolverFor) && input.remainingConflicts.length === 0;
}

/**
 * Soft edges whose parent has merged but whose diff has not yet been delivered
 * to the child. The scheduler pipes each parent's diff into the child context.
 */
export function undeliveredSoftEdges(graph: RudderGraph): GraphEdge[] {
  return Object.values(graph.edges).filter(
    (edge) => edge.type === "soft" && !edge.delivered && graph.nodes[edge.from]?.status === "merged",
  );
}

/**
 * Judge edges (fan-out-and-judge) whose variant has REACHED review (or merged)
 * but whose diff has not yet been delivered to the judge node. Like
 * undeliveredSoftEdges but gated on review rather than merge, since the judge
 * compares the variants' finished work before any of them lands.
 */
export function undeliveredJudgeEdges(graph: RudderGraph): GraphEdge[] {
  return Object.values(graph.edges).filter((edge) => {
    if (edge.type !== "judge" || edge.delivered) {
      return false;
    }
    const status = graph.nodes[edge.from]?.status;
    return status === "review" || status === "merged";
  });
}

/**
 * Whether a node is a fan-out variant: the `from` of at least one judge edge.
 * Such a node is never merged on its own; the judge node it feeds produces the
 * winning implementation and merges that. Used to skip variants during integration.
 */
export function isJudgedVariant(graph: RudderGraph, id: string): boolean {
  return Object.values(graph.edges).some((edge) => edge.type === "judge" && edge.from === id);
}

/**
 * Sum the node-attributed token usage across the whole graph (running +
 * terminal + merged). This is the number the budget cap compares against.
 */
export function sumNodeTokens(graph: RudderGraph): number {
  let total = 0;
  for (const node of Object.values(graph.nodes)) {
    if (node.tokens) {
      total += node.tokens.input + node.tokens.output;
    }
  }
  return total;
}

/**
 * Pure budget gate: true when a positive maxTokens cap is set and the spent
 * total has reached it. The scheduler HOLDS new launches when this is true.
 */
export function isOverBudget(tokensSpent: number, maxTokens: number | undefined): boolean {
  return typeof maxTokens === "number" && maxTokens > 0 && tokensSpent >= maxTokens;
}

/**
 * Whether `next` token usage is a larger total than `prev` (or prev is unset).
 * Backends report cumulative usage, so the largest value seen is the truth; we
 * only ever raise node.tokens, never lower it.
 */
export function tokensNewer(
  prev: { input: number; output: number } | undefined,
  next: { input: number; output: number },
): boolean {
  if (!prev) {
    return next.input + next.output > 0;
  }
  return next.input + next.output > prev.input + prev.output;
}

// ===========================================================================
// SIDE-EFFECTFUL orchestration. These mutate graph.json (always via updateGraph,
// the daemon-only writer), spawn detached workers, and call jj.
// ===========================================================================

/**
 * Launch a ready node: create its jj workspace off the commit the integration
 * bookmark points at, create the run record, flip status to "running" INSIDE the
 * updateGraph transaction (the double-launch guard), then spawn the detached
 * worker and publish schedule.launched.
 */
export async function launchNode(
  repoRoot: string,
  graph: RudderGraph,
  node: TaskNode,
  bus: RudderBus,
): Promise<void> {
  assertLegacyWriterAllowed(repoRoot);
  // Persist a reservation before the first workspace/run/process side effect.
  // A crash at any later step is uncertain, never permission to replay launch.
  const claimId = randomUUID();
  let reserved = false;
  let currentGraph = graph;
  let currentNode = node;
  let intendedRunId = newRunId(node.prompt);
  await updateGraph(repoRoot, (g) => {
    const fresh = g.nodes[node.id];
    if (!fresh || !isReady(g, fresh)) return;
    reserved = reservePlanningLaunch(g, node.id, claimId);
    if (reserved) {
      const plan = planForNode(g, node.id);
      if (plan?.routing.route === "execute") intendedRunId = plan.ownerRunId;
      recordPlanningLaunch(g, node.id, claimId, "reserved", intendedRunId);
      currentGraph = g; currentNode = structuredClone(fresh);
    }
  });
  if (!reserved) return;
  try {
    await launchReservedNode(repoRoot, currentGraph, currentNode, claimId, intendedRunId, bus);
  } catch (error) {
    await updateGraph(repoRoot, g => { recordPlanningLaunch(g, node.id, claimId, "uncertain"); });
    throw error;
  }
}

async function launchReservedNode(repoRoot: string, graph: RudderGraph, node: TaskNode, claimId: string, intendedRunId: string, bus: RudderBus): Promise<void> {
  // Every worker starts from the commit the integration bookmark points at. A
  // hard dependent is only ready once its parents landed there, so it sees them.
  const baseChange = (await integrationTrunk(repoRoot)).commit;

  const owned = planForNode(graph, node.id);
  const selection = owned?.nodes[node.id]?.scope;
  if (selection && (selection.checkoutPath !== repoRoot || selection.location !== "local")) {
    throw new Error("Owned task must launch on its selected checkout and location");
  }
  const isolated = selection?.workspaceMode !== "selected";
  const workspace = isolated ? await createNodeWorkspace({
    repoRoot,
    nodeId: node.id,
    atChangeId: baseChange,
    task: node.title,
  }) : { path: repoRoot, workspaceName: undefined };
  const jjChangeId = (await currentJjChangeId(workspace.path)) || node.jjChangeId;

  const run = await createRunRecord({
    id: intendedRunId,
    repoRoot,
    task: owned ? ownedWorkerPrompt(owned, node.id) : node.prompt,
    backend: node.backend,
    model: node.model,
    effort: node.effort,
    targetBranch: baseChange,
    baseCommit: baseChange,
    useWorkspace: isolated,
    workspaceName: workspace.workspaceName,
    workspaceChangeId: jjChangeId,
    workspacePath: workspace.path,
  });

  // Flip status -> running in the same transaction so a concurrent tick cannot
  // re-select this node and double-launch it.
  await updateGraph(repoRoot, (g) => {
    const current = g.nodes[node.id];
    if (!current) throw new Error("Reserved node disappeared; reconcile before retrying");
    const reservation = (g as PlanningGraph).launchReservations?.[node.id];
    if (!reservation || reservation.claimId !== claimId || current.status !== "planned" || current.runId) throw new Error("Launch reservation changed; reconcile before retrying");
    recordPlanningLaunch(g, node.id, claimId, "reserved", run.id);
    current.runId = run.id;
    current.status = "running";
    current.workspace = { path: workspace.path, workspaceName: workspace.workspaceName };
    if (jjChangeId) {
      current.jjChangeId = jjChangeId;
    }
    current.updatedAt = nowIso();
    return g;
  });

  const attemptId = newAttemptId();
  const pid = spawnWorker(repoRoot, run.id, attemptId);
  run.status = "running";
  run.process = { pid, controllerPid: pid, attemptId, startedAt: nowIso() };
  await saveRunRecord(run);
  await updateGraph(repoRoot, g => { recordPlanningLaunch(g, node.id, claimId, "started", run.id); });

  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "schedule.launched",
    message: `Launched ${node.id} (${node.title})`,
    data: { workspace: workspace.path },
  });
  bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "node.running" });
}

/**
 * Land a node's run on the integration bookmark (main) through the dashboard's
 * own merge: main gains the node's change, the checkout follows with its own
 * edits still uncommitted, and the node's workspace moves onto a fresh change.
 * A conflicted merge becomes the checkout's `@` and gets a resolver. Anything
 * else that stops the merge holds the node blocked with the error.
 */
export async function mergeNodeIntoIntegration(repoRoot: string, node: TaskNode, bus: RudderBus): Promise<void> {
  assertLegacyWriterAllowed(repoRoot);
  const live = await readGraph(repoRoot);
  const owned = planForNode(live, node.id);
  if (owned && !owned.nodes[node.id].scope.permissions.includes("integrate")) {
    bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.review", message: "Work is ready for review; integration requires owner authorization" });
    return;
  }
  if (!node.runId) {
    throw new Error(`Node ${node.id} has no run to merge.`);
  }
  const run = await loadRunRecord(repoRoot, node.runId);
  if (!run) {
    throw new Error(`Run ${node.runId} for node ${node.id} is missing.`);
  }
  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "merge.attempt",
    message: `Merging ${node.id} into integration`,
  });
  await landNodeRun(repoRoot, node, run, bus);
}

/**
 * Merge the node's run (mergeJjRunIntoCurrentWorkspace takes the cross-process
 * integration lock itself) and record the outcome on the graph. Returns true
 * when the node merged.
 */
async function landNodeRun(repoRoot: string, node: TaskNode, run: RunRecord, bus: RudderBus): Promise<boolean> {
  let landed: RunRecord;
  try {
    landed = await mergeJjRunIntoCurrentWorkspace(run, true, { subject: node.title });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.merge = { status: "failed", error: message };
        current.updatedAt = nowIso();
      }
      return g;
    });
    bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "merge.conflict", message: `Could not merge ${node.id}: ${message}` });
    bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "node.blocked" });
    throw error;
  }
  const merge = landed.merge;
  if (merge?.status === "merged") {
    // If this is a judge node (fan-out-and-judge), its variant parents lose:
    // the judge produced the winning implementation and merged it. The variants
    // are never merged; they end in "review" flagged supersededBy the judge so
    // the board can render them as superseded.
    const supersededVariants = judgeParents(await readGraph(repoRoot), node.id);
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "merged";
        current.reviewState = "approved";
        current.merge = merge;
        current.updatedAt = nowIso();
      }
      for (const variantId of supersededVariants) {
        const variant = g.nodes[variantId];
        if (variant && variant.status !== "merged") {
          variant.supersededBy = node.id;
          variant.updatedAt = nowIso();
        }
      }
      return g;
    });
    bus.publish({
      ts: nowIso(),
      runId: run.id,
      nodeId: node.id,
      type: "merge.merged",
      message: `Merged ${node.id} into ${merge.targetBranch}`,
      data: { mergeChangeId: merge.mergeChangeId ?? "", operationId: merge.operationId ?? "", localCommit: merge.localCommit ?? "" },
    });
    bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "node.merged" });
    return true;
  }
  if (merge?.status !== "conflict" || !merge.mergeChangeId) {
    throw new Error(`Merging ${node.id} ended as ${merge?.status ?? "nothing"} instead of merged or conflicted.`);
  }

  // Conflict-as-data: the checkout now holds the conflicted merge. Hold the node
  // blocked and spawn a resolver on top of that merge, unless we have already
  // auto-retried once (cap at 1).
  const conflictedFiles = merge.conflictedFiles ?? [];
  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "merge.conflict",
    message: `Merge conflict for ${node.id}`,
    data: { mergeChangeId: merge.mergeChangeId, operationId: merge.operationId ?? "", conflictedFiles },
  });
  bus.publish({ ts: nowIso(), runId: run.id, nodeId: node.id, type: "node.blocked" });
  const trunk = await integrationTrunk(repoRoot);
  await spawnResolver(repoRoot, {
    node,
    mergeChangeId: merge.mergeChangeId,
    intoChangeId: trunk.commit,
    intoTitle: trunk.bookmark,
    conflictedFiles,
    bus,
  });
  return false;
}

/**
 * Spawn a resolver-agent node at the conflicted merge change. The resolver's
 * workspace is a new change on top of that merge, so its files carry the
 * conflict markers; its run record carries resolverFor = the blocked node id.
 * The original node keeps status "blocked" with resolverRunId set. The resolver
 * auto-retry is capped: a node that already has a resolverRunId is not respun.
 */
async function spawnResolver(
  repoRoot: string,
  input: {
    node: TaskNode;
    mergeChangeId: string;
    intoChangeId: string;
    intoTitle: string;
    conflictedFiles: string[];
    bus: RudderBus;
  },
): Promise<void> {
  const { node, bus } = input;

  // Cap auto-retry at 1: if this node already spawned a resolver, do not loop.
  if (node.resolverRunId) {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    return;
  }

  const resolverRunId = newRunId(`resolve ${node.title}`);

  let workspace: { workspaceName: string; path: string };
  try {
    workspace = await createNodeWorkspace({
      repoRoot,
      runId: resolverRunId,
      atChangeId: input.mergeChangeId,
      task: `resolve ${node.title}`,
    });
  } catch {
    // jj could not materialize the merge change; hold blocked without a resolver.
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    return;
  }

  const prompt = buildResolverPrompt({
    mergeChangeId: input.mergeChangeId,
    intoTitle: input.intoTitle,
    nodeTitle: node.title,
    conflictedFiles: input.conflictedFiles,
  });

  const run = await createRunRecord({
    id: resolverRunId,
    repoRoot,
    task: prompt,
    backend: node.backend,
    model: node.model,
    effort: node.effort,
    targetBranch: input.mergeChangeId,
    baseCommit: input.mergeChangeId,
    resolverFor: node.id,
    useWorkspace: true,
    workspaceName: workspace.workspaceName,
    workspaceChangeId: input.mergeChangeId,
    workspacePath: workspace.path,
  });

  // Persist the resolver context for the worker (and any UI). Best-effort.
  const context: ResolverContext = {
    mergeChangeId: input.mergeChangeId,
    parentChangeIds: [input.intoChangeId, node.jjChangeId ?? ""].filter(Boolean),
    conflictedFiles: input.conflictedFiles,
    nodeTitle: node.title,
    intoTitle: input.intoTitle,
    workspacePath: workspace.path,
  };
  await writeJson(
    path.join(runDir(repoRoot, resolverRunId), "resolver.json"),
    context as unknown as JsonValue,
  ).catch(() => undefined);

  // Spawn the resolver worker FIRST. With detached + ignored stdio a failed spawn
  // returns undefined; if we back-pointed the node at resolverRunId before knowing
  // that, the node would stay blocked forever (the auto-retry cap skips any node that
  // already has a resolverRunId). So only persist the back-pointer once the worker is
  // actually live; on a failed spawn, leave the node blocked WITHOUT a resolverRunId so
  // a later merge attempt can retry, and surface the failure.
  const attemptId = newAttemptId();
  const pid = spawnWorker(repoRoot, run.id, attemptId);
  if (pid === undefined) {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "blocked";
        current.merge = {
          ...(current.merge ?? { status: "conflict" }),
          status: "conflict",
          mergeChangeId: input.mergeChangeId,
          conflictedFiles: input.conflictedFiles,
        };
        current.updatedAt = nowIso();
      }
      return g;
    });
    throw new Error(`failed to spawn conflict resolver ${run.id} for node ${node.id}`);
  }
  run.status = "running";
  run.process = { pid, controllerPid: pid, attemptId, startedAt: nowIso() };
  await saveRunRecord(run);

  // Resolver is live: hold the original node blocked and back-point at its resolver.
  await updateGraph(repoRoot, (g) => {
    const current = g.nodes[node.id];
    if (current) {
      current.status = "blocked";
      current.resolverRunId = resolverRunId;
      current.merge = {
        ...(current.merge ?? { status: "conflict" }),
        status: "conflict",
        mergeChangeId: input.mergeChangeId,
        conflictedFiles: input.conflictedFiles,
      };
      current.updatedAt = nowIso();
    }
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: run.id,
    nodeId: node.id,
    type: "resolver.spawned",
    message: `Spawned resolver ${run.id} for ${node.id}`,
    data: { resolverRunId: run.id, mergeChangeId: input.mergeChangeId, conflictedFiles: input.conflictedFiles },
  });
}

/**
 * Deliver a parent's diff to its child as additional context, then mark the
 * edge delivered. Used for both soft edges (parent merged) and judge edges (the
 * variant reached review). Minimal acceptable form: record the parent diff on
 * the child node (the worker re-reads it). Never blocks. Publishes
 * schedule.softDelivered.
 */
export async function deliverSoftDiff(repoRoot: string, edge: GraphEdge, bus: RudderBus): Promise<void> {
  const graph = await readGraph(repoRoot);
  const parent = graph.nodes[edge.from];
  const child = graph.nodes[edge.to];
  if (!parent || !child) {
    return;
  }
  const parentWorkspace = parent.workspace?.path ?? repoRoot;
  const diff = await jjDiff(parentWorkspace).catch(() => "");
  const isJudge = edge.type === "judge";

  await updateGraph(repoRoot, (g) => {
    const target = g.edges[edge.id];
    if (target) {
      target.delivered = true;
    }
    const childNode = g.nodes[edge.to];
    if (childNode && diff.trim()) {
      // Stash the delivered context on the child's prompt so the worker reads it
      // when the run is created/continued. Append, do not replace, and never
      // block on it. Judge nodes receive each variant's diff to compare.
      const header = isJudge
        ? `\n\n--- Variant ${parent.id} (${parent.title}) diff to evaluate ---\n`
        : `\n\n--- Context from merged sibling ${parent.id} (${parent.title}) ---\n`;
      if (!childNode.prompt.includes(header)) {
        // Real repo diffs commonly exceed 8k chars; a too-small slice silently starves a
        // judge of comparison context (each variant is truncated independently). 16k keeps
        // it bounded while carrying typical diffs whole.
        childNode.prompt = `${childNode.prompt}${header}${diff.slice(0, 16000)}`;
      }
      childNode.updatedAt = nowIso();
    }
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: child.runId ?? child.id,
    nodeId: child.id,
    type: "schedule.softDelivered",
    message: isJudge
      ? `Delivered variant ${parent.id} diff to judge ${child.id}`
      : `Delivered ${parent.id} diff to ${child.id}`,
    data: { from: edge.from, to: edge.to, kind: edge.type },
  });
}

// Per-repo serialization for every scheduler operation that reads or moves the
// graph and the integration bookmark. The daemon fires scheduleTick (1s
// interval) AND onRunTransition (per-run fs.watch) AND the board's manual merge
// endpoints, all in ONE process as un-awaited `void` calls. Chaining every op per
// repo onto a single promise keeps a tick from launching or merging on a graph
// another op is midway through changing. Merges also take the cross-process
// integration lock inside mergeJjRunIntoCurrentWorkspace.
const schedulerChains = new Map<string, Promise<unknown>>();

export function withSchedulerLock<T>(repoRoot: string, fn: () => Promise<T>): Promise<T> {
  const prev = schedulerChains.get(repoRoot) ?? Promise.resolve();
  // Run fn after the previous op settles (success OR failure), so one rejected op
  // never wedges the chain. The caller still sees fn's own result/rejection.
  const result = prev.then(fn, fn);
  schedulerChains.set(
    repoRoot,
    result.catch(() => undefined),
  );
  return result;
}

/**
 * One scheduler tick. Recompute each scheduled node's status from its run.json,
 * read orchestrator config, hold if at capacity or over budget, else launch the
 * selectable ready nodes and deliver undelivered soft diffs. A node that just
 * reached completed (review) integrates automatically.
 *
 * Public entry point: serialized against all other scheduler ops for this repo.
 * Internal callers that already hold the lock use `scheduleTickCore` directly.
 */
export function scheduleTick(repoRoot: string, bus: RudderBus): Promise<void> {
  return withSchedulerLock(repoRoot, () => scheduleTickCore(repoRoot, bus));
}

async function scheduleTickCore(repoRoot: string, bus: RudderBus): Promise<void> {
  assertLegacyWriterAllowed(repoRoot);
  const config = await loadConfig();
  const orchestrator = config.orchestrator ?? { maxParallel: 1000 };
  const maxParallel = orchestrator.maxParallel ?? 1000;
  const budgetTokens = orchestrator.budget?.maxTokens;

  // Recompute statuses from run.json and surface review/integration transitions.
  const graph = await readGraph(repoRoot);
  let runningCount = 0;
  let tokensSpent = 0;
  const justReached: Array<{ node: TaskNode; status: TaskNode["status"] }> = [];
  // Token usage the backend reported on run.json that is newer than what the
  // node currently carries; flushed into the graph so the budget sum is real.
  const tokenUpdates = new Map<string, { input: number; output: number }>();

  for (const node of Object.values(graph.nodes)) {
    let status = node.status;
    let nodeTokens = node.tokens;
    // A node held blocked with a resolver in flight is owned by that resolver,
    // not by its original (already-completed) run. Do not re-project it back to
    // review from the original run.json while the resolver works.
    const resolverInFlight = node.status === "blocked" && Boolean(node.resolverRunId);
    if (node.runId && !resolverInFlight) {
      const run = await loadRunRecord(repoRoot, node.runId).catch(() => null);
      status = projectNodeStatus(node, run ?? undefined);
      // Budget accounting: copy the backend-reported run.tokens onto the node so
      // scheduleTick's budget sum runs on real numbers (the worker captures them
      // from claude/codex stream output). Only update when newer/larger.
      if (run?.tokens && tokensNewer(node.tokens, run.tokens)) {
        nodeTokens = { input: run.tokens.input, output: run.tokens.output };
        tokenUpdates.set(node.id, nodeTokens);
      }
    }
    // Budget is a soft cap on node-attributed tokens summed across every node
    // that has reported usage (running + terminal + merged alike).
    if (nodeTokens) {
      tokensSpent += nodeTokens.input + nodeTokens.output;
    }
    if (status !== node.status) {
      justReached.push({ node, status });
    }
    if (status === "running") {
      runningCount += 1;
    }
  }

  // A failure detected by the tick must block hard dependents exactly like the
  // edge-triggered onRunTransition path — otherwise a dependent whose parent failed
  // (but was only noticed via the 1s tick) would still be scheduled. Compute the
  // blocked set from the pre-update graph for every justReached "failed".
  const blockedByFailure: string[] = [];
  for (const { node, status } of justReached) {
    if (status === "failed") {
      for (const id of computeBlocked(graph, node.id)) {
        if (!blockedByFailure.includes(id)) {
          blockedByFailure.push(id);
        }
      }
    }
  }

  // Persist any projected status changes (and capture review transitions) plus
  // any freshened token counts.
  if (justReached.length || tokenUpdates.size) {
    await updateGraph(repoRoot, (g) => {
      for (const { node, status } of justReached) {
        const current = g.nodes[node.id];
        if (current && current.status !== status) {
          current.status = status;
          if (status === "review") {
            current.reviewState = current.reviewState ?? "pending";
          }
          current.updatedAt = nowIso();
        }
      }
      for (const id of blockedByFailure) {
        const target = g.nodes[id];
        if (target && target.status !== "merged" && target.status !== "blocked") {
          target.status = "blocked";
          target.updatedAt = nowIso();
        }
      }
      for (const [nodeId, tokens] of tokenUpdates) {
        const current = g.nodes[nodeId];
        if (current) {
          current.tokens = tokens;
          current.updatedAt = nowIso();
        }
      }
      return g;
    });
    for (const { node, status } of justReached) {
      if (status === "review") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.review" });
      } else if (status === "failed") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.failed" });
      } else if (status === "merged") {
        bus.publish({ ts: nowIso(), runId: node.runId ?? node.id, nodeId: node.id, type: "node.merged" });
      }
    }
    for (const id of blockedByFailure) {
      bus.publish({ ts: nowIso(), runId: id, nodeId: id, type: "node.blocked" });
    }
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }

  for (const [id, reservation] of Object.entries((graph as PlanningGraph).launchReservations ?? {})) {
    if ((reservation.state === "reserved" || reservation.state === "uncertain") && graph.nodes[id]?.status !== "running") runningCount++;
  }

  // Integrate review nodes. Skip fan-out variants (nodes
  // that feed a judge node via a judge edge): only the judge node merges; the
  // variants end in review, superseded by the judge's choice.
  const fresh = await readGraph(repoRoot);
  for (const node of Object.values(fresh.nodes)) {
    // reviewState is the board's lever: "changes-requested" holds the node out
    // of auto-integration until the revised work lands back in review. Without
    // this check the flag was write-only and the board's review controls were
    // cosmetic — the tick merged every review node regardless.
    if (
      node.status === "review" &&
      !node.supersededBy &&
      node.reviewState !== "changes-requested" &&
      !isJudgedVariant(fresh, node.id) &&
      (!planForNode(fresh, node.id) || planForNode(fresh, node.id)!.nodes[node.id].scope.permissions.includes("integrate"))
    ) {
      await mergeNodeIntoIntegration(repoRoot, node, bus);
    }
  }

  const overBudget = isOverBudget(tokensSpent, budgetTokens);
  if (runningCount >= maxParallel || overBudget) {
    bus.publish({
      ts: nowIso(),
      runId: "scheduler",
      type: "schedule.tick",
      message: overBudget
        ? `holding: budget exceeded (${tokensSpent}/${budgetTokens} tokens)`
        : "holding: at parallel capacity",
      data: { runningCount, maxParallel, tokensSpent, ...(budgetTokens ? { maxTokens: budgetTokens } : {}), overBudget },
    });
    return;
  }

  // Deliver pending judge diffs BEFORE launching so a judge node that is about
  // to become ready has every variant diff already stitched into its prompt.
  const beforeLaunch = await readGraph(repoRoot);
  for (const edge of undeliveredJudgeEdges(beforeLaunch)) {
    await deliverSoftDiff(repoRoot, edge, bus);
  }

  const latest = await readGraph(repoRoot);
  const launchable = selectLaunchable(latest, runningCount, maxParallel);
  for (const node of launchable) {
    await launchNode(repoRoot, latest, node, bus);
  }

  const afterLaunch = launchable.length ? await readGraph(repoRoot) : latest;
  for (const edge of undeliveredSoftEdges(afterLaunch)) {
    await deliverSoftDiff(repoRoot, edge, bus);
  }

  bus.publish({
    ts: nowIso(),
    runId: "scheduler",
    type: "schedule.tick",
    message: `tick: launched ${launchable.length}`,
    data: { runningCount, maxParallel, launched: launchable.length },
  });

  if (launchable.length) {
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }
}

/**
 * Called by the daemon when a run.json changes. Projects the owning node's
 * status: completed -> review then integration; failed/cancelled ->
 * node.failed + propagate blocked to hard dependents. Then ticks the scheduler.
 */
export function onRunTransition(repoRoot: string, runId: string, bus: RudderBus): Promise<void> {
  return withSchedulerLock(repoRoot, () => onRunTransitionCore(repoRoot, runId, bus));
}

async function onRunTransitionCore(repoRoot: string, runId: string, bus: RudderBus): Promise<void> {
  assertLegacyWriterAllowed(repoRoot);
  const run = await loadRunRecord(repoRoot, runId).catch(() => null);

  // A resolver run finishing is handled specially: it owns no graph node of its
  // own; instead it points (resolverFor) at the originally-blocked node.
  if (run?.resolverFor && (run.status === "completed" || run.status === "merged")) {
    await onResolverTransition(repoRoot, run.resolverFor, run, bus);
    return;
  }

  const graph = await readGraph(repoRoot);
  const node = Object.values(graph.nodes).find((candidate) => candidate.runId === runId);
  if (!node) {
    // Not a graph-managed run (e.g. an ad-hoc TUI run); just tick.
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  // If this node is held blocked with a resolver in flight, the resolver (not
  // the original completed run) drives it. Ignore the original run's transition.
  if (node.status === "blocked" && node.resolverRunId) {
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  const projected = projectNodeStatus(node, run ?? undefined);

  if (projected === "review" && node.status !== "review") {
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "review";
        current.reviewState = current.reviewState ?? "pending";
        current.updatedAt = nowIso();
      }
      return g;
    });
    bus.publish({ ts: nowIso(), runId, nodeId: node.id, type: "node.review" });
    await renderLiveRudderMd(repoRoot).catch(() => undefined);

    // A fan-out variant never integrates directly: it only feeds the judge node, which
    // is what eventually merges. Let scheduleTick deliver the variant diff and
    // launch the judge once all variants have reached review.
    if (!isJudgedVariant(graph, node.id)) {
      const fresh = await readGraph(repoRoot);
      const refreshed = fresh.nodes[node.id];
      // Same changes-requested hold as the tick-path integrator.
      if (refreshed && refreshed.reviewState !== "changes-requested") {
        await mergeNodeIntoIntegration(repoRoot, refreshed, bus);
      }
    }
  } else if (projected === "failed" && node.status !== "failed") {
    const blocked = computeBlocked(graph, node.id);
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[node.id];
      if (current) {
        current.status = "failed";
        current.updatedAt = nowIso();
      }
      for (const id of blocked) {
        const target = g.nodes[id];
        if (target && target.status !== "merged") {
          target.status = "blocked";
          target.updatedAt = nowIso();
        }
      }
      return g;
    });
    bus.publish({ ts: nowIso(), runId, nodeId: node.id, type: "node.failed" });
    for (const id of blocked) {
      bus.publish({ ts: nowIso(), runId, nodeId: id, type: "node.blocked" });
    }
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
  }

  await scheduleTickCore(repoRoot, bus);
}

/**
 * A resolver run finished. Its workspace is a change on top of the conflicted
 * merge the main checkout holds. With no conflicts left there, its resolution
 * moves into that merge and the original node lands through the same merge call,
 * which finalizes a merge whose conflicts are gone, then tick (to unblock
 * dependents). If conflicts remain, hold the node blocked and re-publish
 * merge.conflict. Auto-retry is capped at 1 (the original node already has
 * resolverRunId set, so spawnResolver does not respin).
 */
async function onResolverTransition(
  repoRoot: string,
  originalNodeId: string,
  resolverRun: RunRecord,
  bus: RudderBus,
): Promise<void> {
  const workspacePath = resolverRun.workspace?.path || repoRoot;
  const remaining = await listConflicts(workspacePath).catch(() => [] as string[]);
  const node = (await readGraph(repoRoot)).nodes[originalNodeId];
  if (node?.status === "merged") {
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  if (!resolverShouldFinalize({ resolverFor: resolverRun.resolverFor, remainingConflicts: remaining })) {
    // Conflicts remain: keep the node blocked. We do not respin (cap at 1).
    await updateGraph(repoRoot, (g) => {
      const current = g.nodes[originalNodeId];
      if (current && current.status !== "merged") {
        current.status = "blocked";
        current.updatedAt = nowIso();
      }
      return g;
    });
    bus.publish({
      ts: nowIso(),
      runId: resolverRun.id,
      nodeId: originalNodeId,
      type: "merge.conflict",
      message: `Resolver ${resolverRun.id} left ${remaining.length} conflict(s) for ${originalNodeId}`,
      data: { conflictedFiles: remaining, mergeChangeId: resolverRun.workspace?.jjChangeId ?? "" },
    });
    await renderLiveRudderMd(repoRoot).catch(() => undefined);
    await scheduleTickCore(repoRoot, bus);
    return;
  }

  if (!node?.runId) {
    throw new Error(`Resolver ${resolverRun.id} finished for node ${originalNodeId}, which has no run to merge.`);
  }
  const run = await loadRunRecord(repoRoot, node.runId);
  const mergeChangeId = run?.merge?.status === "conflict" ? run.merge.mergeChangeId : undefined;
  if (!run || !mergeChangeId) {
    throw new Error(`Node ${originalNodeId} has no conflicted merge for resolver ${resolverRun.id} to finish.`);
  }
  await squashResolutionIntoMerge(workspacePath, mergeChangeId);
  if (await landNodeRun(repoRoot, node, run, bus)) {
    bus.publish({
      ts: nowIso(),
      runId: resolverRun.id,
      nodeId: originalNodeId,
      type: "resolver.resolved",
      message: `Resolver ${resolverRun.id} resolved ${originalNodeId}`,
      data: { mergeChangeId },
    });
  }

  await renderLiveRudderMd(repoRoot).catch(() => undefined);
  await scheduleTickCore(repoRoot, bus);
}

/**
 * The single injection chokepoint. A typed task (terminal pane or board
 * composer) becomes a NEW node reconciled against the frontier, never blindly
 * appended. Adds the node (status "planned", source "injection") + inferred
 * edges, publishes plan.reconciled, then ticks.
 */
export function reconcileInjection(
  repoRoot: string,
  input: { prompt: string; title?: string; backend?: TaskNode["backend"]; model?: string; effort?: TaskNode["effort"] },
  bus: RudderBus,
): Promise<{ nodeId: string }> {
  return withSchedulerLock(repoRoot, () => reconcileInjectionCore(repoRoot, input, bus));
}

async function reconcileInjectionCore(
  repoRoot: string,
  input: { prompt: string; title?: string; backend?: TaskNode["backend"]; model?: string; effort?: TaskNode["effort"] },
  bus: RudderBus,
): Promise<{ nodeId: string }> {
  assertLegacyWriterAllowed(repoRoot);
  const graph = await readGraph(repoRoot);
  if (Object.keys((graph as PlanningGraph).planningOwnership ?? {}).length) {
    throw new Error("Owned tasks require a versioned add, replace or cancel command; a routine message cannot start another planner");
  }
  const backend = input.backend ?? configuredBackend(await loadConfig());
  const frontierNodes: PlanNode[] = graphFrontier(graph).map((node) => ({
    id: node.id,
    title: node.title,
    prompt: node.prompt,
    deps: [],
  }));

  const branch = await currentBranch(repoRoot).catch(() => "main");
  const result = await reconcile(input.prompt, frontierNodes, { root: repoRoot, branch }).catch(() => ({
    node: {
      id: "new",
      title: input.prompt.slice(0, 72),
      prompt: input.prompt,
      goal: deriveGoal(input.prompt),
      success: DEFAULT_SUCCESS,
      deps: [],
    } as PlanNode,
    inferredDeps: [] as InferredDep[],
  }));

  const title = input.title?.trim() || result.node.title || input.prompt.slice(0, 72);
  const nodeId = newNodeId(title);

  const createdAt = nowIso();
  await updateGraph(repoRoot, (g) => {
    const incomingEdgeIds: string[] = [];
    for (const dep of result.inferredDeps) {
      if (!g.nodes[dep.node]) {
        continue;
      }
      const edgeId = newEdgeId(dep.node, nodeId);
      incomingEdgeIds.push(edgeId);
      g.edges[edgeId] = {
        id: edgeId,
        from: dep.node,
        to: nodeId,
        type: dep.type,
      };
    }
    const goal = normalizeGoalLine(result.node.goal ?? deriveGoal(title || input.prompt), title || input.prompt);
    const success = normalizeGoalLine(result.node.success ?? DEFAULT_SUCCESS, DEFAULT_SUCCESS);
    g.nodes[nodeId] = {
      id: nodeId,
      title,
      // Reconciled (injected) nodes lead with the objective-format header too.
      prompt: formatGoalPrompt({ goal, success, body: result.node.prompt || input.prompt }),
      goal,
      success,
      backend,
      ...(input.model ? { model: input.model } : {}),
      ...(input.effort ? { effort: input.effort } : {}),
      status: "planned",
      deps: incomingEdgeIds,
      source: "injection",
      createdAt,
      updatedAt: createdAt,
    };
    return g;
  });

  bus.publish({
    ts: nowIso(),
    runId: nodeId,
    nodeId,
    type: "plan.reconciled",
    message: `Reconciled injection ${nodeId} against ${frontierNodes.length} frontier node(s)`,
    data: { inferredDeps: result.inferredDeps },
  });
  bus.publish({ ts: nowIso(), runId: nodeId, nodeId, type: "node.created" });

  await renderLiveRudderMd(repoRoot).catch(() => undefined);
  await scheduleTickCore(repoRoot, bus);
  return { nodeId };
}
