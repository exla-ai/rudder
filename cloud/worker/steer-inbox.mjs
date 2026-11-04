// A dashboard's steer inbox (<repo>/.rudder/steer): how anything outside the
// dashboard asks it to act on an agent. The dashboard is the single writer of
// its agents, so requests are files it consumes on its next poll, and each one
// gets a receipt (<repo>/.rudder/steer-receipts/<requestId>.json) saying
// whether it was delivered. Shared by the supervisor (home.mjs) and the
// cleanup job (cleanup.mjs).

import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

/** Drop one request into the inbox; returns its request id. */
export function queueSteer(repoDir, { kind, taskId, instruction }) {
  const requestId = randomUUID();
  const inbox = path.join(repoDir, ".rudder", "steer");
  fs.mkdirSync(inbox, { recursive: true });
  const safeId = (taskId || "conductor").replace(/[^A-Za-z0-9_.-]/g, "-").slice(0, 64) || "conductor";
  const name = `${String(Date.now()).padStart(13, "0")}-${String(process.hrtime.bigint()).padStart(20, "0")}-${safeId}-${requestId}.json`;
  const file = path.join(inbox, name);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ requestId, kind, taskId, instruction, ts: new Date().toISOString() }));
  fs.renameSync(tmp, file);
  return requestId;
}

/**
 * The dashboard's answer to one request: `{status: "delivered"}`,
 * `{status: "failed", error}`, or null while it has not answered yet
 * ("processing" is a tombstone written before it acts, not an answer).
 */
export function readSteerReceipt(repoDir, requestId) {
  try {
    const receipt = JSON.parse(fs.readFileSync(path.join(repoDir, ".rudder", "steer-receipts", `${requestId}.json`), "utf8"));
    return receipt?.status === "delivered" || receipt?.status === "failed" ? receipt : null;
  } catch {
    return null;
  }
}
