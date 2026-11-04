// A project's plan, as the web and phone apps draw it.
//
// The dashboard mirrors every live plan into `.rudder/graph.json` (src/graph.ts,
// one flat node list: node ids are unique across plans) and keeps each plan's
// approval gate in `.rudder/plan-queue.json` (native PlanQueueFile). This reads
// both into one small shape. Read every state tick, so a file is parsed again
// only when its mtime or size moved, and a malformed file reads as "no plan"
// rather than throwing.

import fs from "node:fs";
import path from "node:path";

const STATUSES = new Set(["planned", "ready", "running", "review", "merged", "failed", "blocked"]);
const MAX_NODES = 200;

const fileCache = new Map(); // file -> { key, value }

function readCached(file, parse) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    fileCache.delete(file);
    return null;
  }
  const key = `${stat.mtimeMs}:${stat.size}`;
  const cached = fileCache.get(file);
  if (cached && cached.key === key) {
    return cached.value;
  }
  let value = null;
  try {
    value = parse(JSON.parse(fs.readFileSync(file, "utf8")));
  } catch {
    value = null;
  }
  fileCache.set(file, { key, value });
  return value;
}

function text(value, max) {
  return typeof value === "string" && value ? value.slice(0, max) : null;
}

/** graph.json -> { nodes, updatedAt } or null. */
export function planFromGraph(graph) {
  if (!graph || typeof graph !== "object" || graph.version !== 1) {
    return null;
  }
  const nodes = graph.nodes && typeof graph.nodes === "object" && !Array.isArray(graph.nodes) ? graph.nodes : null;
  const edges = graph.edges && typeof graph.edges === "object" && !Array.isArray(graph.edges) ? graph.edges : {};
  if (!nodes) {
    return null;
  }
  const depsByNode = new Map();
  for (const edge of Object.values(edges)) {
    if (!edge || typeof edge.from !== "string" || typeof edge.to !== "string" || !edge.from || !edge.to) {
      continue;
    }
    const type = edge.type === "soft" || edge.type === "judge" ? edge.type : "hard";
    const list = depsByNode.get(edge.to) ?? [];
    list.push({ on: edge.from, type });
    depsByNode.set(edge.to, list);
  }
  const out = [];
  for (const node of Object.values(nodes)) {
    if (!node || typeof node.id !== "string" || !node.id) {
      continue;
    }
    out.push({
      id: node.id,
      title: text(node.title, 200) ?? node.id,
      status: STATUSES.has(node.status) ? node.status : "planned",
      runId: text(node.runId, 200),
      backend: text(node.backend, 40),
      model: text(node.model, 200),
      deps: (depsByNode.get(node.id) ?? []).sort((a, b) => a.on.localeCompare(b.on)),
      createdAt: typeof node.createdAt === "string" ? node.createdAt : "",
    });
  }
  // Planner order (n0, n1, ...) reads best; creation time breaks ties for
  // content-hashed ids the board created.
  out.sort((a, b) => a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id, undefined, { numeric: true }));
  const list = out.slice(0, MAX_NODES).map(({ createdAt: _createdAt, ...node }) => node);
  return { nodes: list, updatedAt: typeof graph.updatedAt === "string" ? graph.updatedAt : null };
}

/** plan-queue.json -> [{ id, awaitingApproval, request }]. Tolerates the older single-plan file. */
export function plansFromQueue(queue) {
  if (!queue || typeof queue !== "object") {
    return [];
  }
  const entries = Array.isArray(queue.plans) && queue.plans.length > 0 ? queue.plans : Array.isArray(queue.plans) ? [] : [queue];
  const out = [];
  for (const entry of entries) {
    if (!entry || typeof entry !== "object") {
      continue;
    }
    out.push({
      id: text(entry.id, 120) ?? "",
      awaitingApproval: entry.awaiting_approval === true,
      request: text(entry.plan_request, 500),
    });
  }
  return out;
}

/**
 * The project's plan: graph nodes plus each plan's approval gate, or null when
 * there is nothing to draw (no graph, an empty graph, or a malformed file).
 */
export function readProjectPlan(repoDir) {
  const dir = path.join(repoDir, ".rudder");
  const graph = readCached(path.join(dir, "graph.json"), planFromGraph);
  const plans = readCached(path.join(dir, "plan-queue.json"), plansFromQueue) ?? [];
  const awaitingApproval = plans.some((plan) => plan.awaitingApproval);
  if ((!graph || graph.nodes.length === 0) && !awaitingApproval) {
    return null;
  }
  return {
    nodes: graph?.nodes ?? [],
    updatedAt: graph?.updatedAt ?? null,
    awaitingApproval,
    plans,
  };
}
