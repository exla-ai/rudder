// A dashboard's steer inbox (<repo>/.rudder/steer): how anything outside the
// dashboard asks it to act on an agent. The dashboard is the single writer of
// its agents, so requests are files it consumes the moment they land, and each
// one gets a receipt (<repo>/.rudder/steer-receipts/<requestId>.json) saying
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
 * The dashboard's answer to one request, the moment its receipt lands:
 * `{status: "delivered"}`, `{status: "failed", error}`, or null if none came
 * within `timeoutMs` ("processing" is a tombstone written before it acts, not
 * an answer).
 */
export function waitSteerReceipt(repoDir, requestId, timeoutMs) {
  const dir = path.join(repoDir, ".rudder", "steer-receipts");
  const name = `${requestId}.json`;
  fs.mkdirSync(dir, { recursive: true });
  return new Promise((resolve, reject) => {
    let settled = false;
    const settle = (finish) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      watcher.close();
      finish();
    };
    const check = () => {
      const receipt = readReceipt(path.join(dir, name));
      if (receipt) settle(() => resolve(receipt));
    };
    const watcher = fs.watch(dir, (_event, changed) => {
      if (!changed || changed === name) check();
    });
    watcher.on("error", (error) => settle(() => reject(error)));
    const timer = setTimeout(() => settle(() => resolve(null)), timeoutMs);
    // The receipt may have landed before the watch started.
    check();
  });
}

function readReceipt(file) {
  try {
    const receipt = JSON.parse(fs.readFileSync(file, "utf8"));
    return receipt?.status === "delivered" || receipt?.status === "failed" ? receipt : null;
  } catch {
    // Not written yet, or caught mid-write by a sender that writes in place.
    return null;
  }
}
