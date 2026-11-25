// A dashboard's steer inbox (<repo>/.rudder/steer): how anything outside the
// dashboard asks it to act on an agent. The dashboard is the single writer of
// its agents, so requests are files it consumes the moment they land, and each
// one gets a receipt (<repo>/.rudder/steer-receipts/<requestId>.json) saying
// whether it was delivered. Shared by the supervisor (home.mjs) and the
// cleanup job (cleanup.mjs).

import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const REQUEST_ID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

/**
 * Drop one request into the inbox; returns its request id. A sender that may
 * ask again (the app, when its send got no answer) names the id and repeats
 * it: a request the inbox still holds, or one the dashboard has claimed or
 * answered (its receipt), is not queued a second time, so one message or new
 * task reaches the dashboard once however often it is sent.
 */
export function queueSteer(repoDir, { kind, taskId, instruction, requestId = randomUUID() }) {
  if (typeof requestId !== "string" || !REQUEST_ID.test(requestId)) throw new Error("bad request id");
  const inbox = path.join(repoDir, ".rudder", "steer");
  fs.mkdirSync(inbox, { recursive: true });
  if (fs.existsSync(path.join(repoDir, ".rudder", "steer-receipts", `${requestId}.json`))
    || fs.readdirSync(inbox).some((name) => name.endsWith(`-${requestId}.json`))) {
    return requestId;
  }
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

/**
 * Resolves true once the agent's run.json says "running", or false after
 * `timeoutMs`. A message typed into an idle agent turns its row Working only
 * when the agent takes it (its turn-start hook, about 200 ms after the
 * receipt), so a state sent with the receipt still shows it idle.
 */
export function waitRunWorking(repoDir, runId, timeoutMs) {
  const dir = path.join(repoDir, ".rudder", "runs", runId);
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
      if (readRunStatus(path.join(dir, "run.json")) === "running") settle(() => resolve(true));
    };
    const watcher = fs.watch(dir, check);
    watcher.on("error", (error) => settle(() => reject(error)));
    const timer = setTimeout(() => settle(() => resolve(false)), timeoutMs);
    check();
  });
}

function readRunStatus(file) {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8")).status;
  } catch {
    // Caught mid-write: the rename that finishes it wakes the watch again.
    return null;
  }
}
/** Read a completed receipt for a send that previously timed out. */
export function readSteerReceipt(repoDir, requestId) {
  if (typeof requestId !== "string" || !REQUEST_ID.test(requestId)) return null;
  return readReceipt(path.join(repoDir, ".rudder", "steer-receipts", `${requestId}.json`));
}

const WITHDRAW_SETTLE_MS = 250;

/**
 * Take back a request the dashboard has not acted on, so a message the app
 * sends again cannot also arrive late. Its receipt is written first, as
 * "withdrawn": the dashboard skips any request that already has a receipt
 * (native poll_steer_inbox, its at-most-once ledger). Returns
 * `{status: "withdrawn"}`, or the dashboard's own answer when it got there
 * first: `{status: "processing"}` while it acts, then the receipt.
 */
export async function withdrawSteer(repoDir, requestId) {
  if (typeof requestId !== "string" || !REQUEST_ID.test(requestId)) throw new Error("bad request id");
  const dir = path.join(repoDir, ".rudder", "steer-receipts");
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `${requestId}.json`);
  const mine = JSON.stringify({ requestId, status: "failed", error: "withdrawn", withdrawn: true });
  try {
    fs.writeFileSync(file, mine, { flag: "wx" });
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
    return answerIn(file);
  }
  const inbox = path.join(repoDir, ".rudder", "steer");
  let names = [];
  try {
    names = fs.readdirSync(inbox);
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  for (const name of names) {
    if (name.endsWith(`-${requestId}.json`)) fs.rmSync(path.join(inbox, name), { force: true });
  }
  // The dashboard checks for a receipt just before it writes "processing"
  // over it: one that had checked already shows up within this wait.
  await new Promise((resolve) => setTimeout(resolve, WITHDRAW_SETTLE_MS));
  return fs.readFileSync(file, "utf8") === mine ? { status: "withdrawn" } : answerIn(file);
}

function answerIn(file) {
  const receipt = readReceipt(file);
  if (receipt?.withdrawn) return { status: "withdrawn" };
  if (receipt) return { status: receipt.status, error: receipt.error, code: receipt.code };
  return { status: "processing" };
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
