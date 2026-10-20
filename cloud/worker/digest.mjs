// The morning digest's raw material: what every agent in one repo did between
// `since` and `until`, read straight from its run.json records (the same files
// the dashboard writes). The control plane groups these by project and keeps
// the result; nothing here is cached, so a digest is never staler than disk.
import fs from "node:fs";
import path from "node:path";
import { userTask } from "./transcripts.mjs";

const MAX_RUNS = 80;
const MAX_TEXT = 600;

export function collectDigestRuns(repoDir, { since, until = Date.now() }) {
  const runsDir = path.join(repoDir, ".rudder", "runs");
  let entries = [];
  try {
    entries = fs.readdirSync(runsDir);
  } catch {
    return [];
  }
  const runs = [];
  for (const id of entries) {
    let record;
    try {
      record = JSON.parse(fs.readFileSync(path.join(runsDir, id, "run.json"), "utf8"));
    } catch {
      continue;
    }
    if (!record || typeof record !== "object" || record.id !== id) {
      continue;
    }
    const run = digestRun(record, { since, until });
    if (run) {
      runs.push(run);
    }
  }
  runs.sort((a, b) => (a.firstAt ?? 0) - (b.firstAt ?? 0));
  return runs.slice(-MAX_RUNS);
}

// One run's activity inside the window, or null when it did nothing there.
export function digestRun(record, { since, until }) {
  const inWindow = (ms) => ms !== null && ms >= since && ms < until;
  const events = [];
  const createdAt = toMillis(record.createdAt);
  if (inWindow(createdAt)) {
    events.push({ at: createdAt, kind: "started" });
  }
  const turns = Array.isArray(record.turns) ? record.turns : [];
  for (const [index, turn] of turns.entries()) {
    const at = toMillis(turn?.ts);
    // The first turn is the task itself, already counted as "started".
    if (!inWindow(at) || (index === 0 && inWindow(createdAt) && Math.abs(at - createdAt) < 60_000)) {
      continue;
    }
    const text = userTask(String(turn?.prompt || "")).trim();
    if (text) {
      events.push({ at, kind: "message", text: clip(text, 200) });
    }
  }
  const status = String(record.status || "");
  const updatedAt = toMillis(record.updatedAt);
  const merge = record.merge && typeof record.merge === "object" ? record.merge : null;
  const delivery = record.delivery && typeof record.delivery === "object" ? record.delivery : null;
  const publish = record.publish && typeof record.publish === "object" ? record.publish : null;
  const verifiedAt = toMillis(delivery?.verifiedAt);
  if (delivery?.status === "deployed" && inWindow(verifiedAt)) {
    events.push({ at: verifiedAt, kind: "deployed", text: clip(String(delivery.target || delivery.url || ""), 200) || null });
  }
  const pushedAt = toMillis(merge?.pushedAt);
  if (merge?.pushed && inWindow(pushedAt)) {
    events.push({ at: pushedAt, kind: "pushed", text: merge.targetBranch ? String(merge.targetBranch) : null });
  }
  if (inWindow(updatedAt)) {
    const end = endEvent(status);
    if (end) {
      events.push({ at: updatedAt, kind: end });
    } else if (events.length === 0) {
      // Still going, or touched without a turn of its own (a reply, a resume).
      events.push({ at: updatedAt, kind: status === "running" ? "working" : "updated" });
    }
  }
  if (events.length === 0) {
    return null;
  }
  events.sort((a, b) => a.at - b.at);
  const tokens = record.tokens && typeof record.tokens === "object" ? record.tokens : null;
  return {
    id: String(record.id),
    title: clip(String(record.taskSummary || userTask(record.task) || record.id), 200),
    task: clip(userTask(String(record.task || "")), MAX_TEXT),
    status: digestStatus(record),
    mode: record.mode ?? null,
    backend: record.backend ?? null,
    model: record.model ?? null,
    nodeId: record.nodeId ?? null,
    summary: typeof record.doneSummary === "string" && record.doneSummary.trim() ? clip(record.doneSummary.trim(), MAX_TEXT) : null,
    merged: status === "merged" || merge?.status === "merged",
    pushed: merge?.pushed === true,
    pr: publish?.url ? { url: String(publish.url), number: Number(publish.number) || null } : null,
    delivery: delivery?.status === "deployed"
      ? { target: delivery.target ?? null, url: delivery.url ?? null, revision: delivery.revision ?? null }
      : null,
    tokens: tokens ? (Number(tokens.input) || 0) + (Number(tokens.output) || 0) : 0,
    firstAt: events[0].at,
    lastAt: events[events.length - 1].at,
    events,
  };
}

function endEvent(status) {
  if (status === "completed") return "finished";
  if (status === "merged") return "merged";
  if (status === "failed") return "failed";
  if (status === "cancelled") return "stopped";
  if (status === "merge-conflict") return "conflict";
  return null;
}

// The phone's vocabulary, the same as home.mjs phoneStatus.
function digestStatus(record) {
  const status = String(record.status || "");
  const phase = String(record.lifecyclePhase || "");
  if (["running", "steering", "verifying", "created"].includes(status)) {
    return record.attention ? "waiting" : "working";
  }
  if (status === "completed" || status === "merge-conflict") {
    if (phase === "deployed") return "deployed";
    if (phase === "review" || status === "merge-conflict" || phase.includes("needs merge") || phase.includes("proof")) {
      return "review";
    }
    return "done";
  }
  if (status === "cancelled") return "stopped";
  if (status === "migrated") return "done";
  return status || "done";
}

function clip(text, max) {
  const value = String(text || "").replace(/\s+/g, " ").trim();
  return value.length > max ? `${value.slice(0, max - 1)}…` : value;
}

function toMillis(value) {
  if (value === null || value === undefined || value === "") {
    return null;
  }
  const number = Number(value);
  if (Number.isFinite(number)) {
    return number;
  }
  const parsed = Date.parse(String(value));
  return Number.isFinite(parsed) ? parsed : null;
}
