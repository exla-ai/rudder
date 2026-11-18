// Build tracking for a project's vibeprint canvas (vibeprint-host.mjs): when an
// agent with tracking on finishes a turn, its diff goes to the canvas's
// `POST /api/track`, where one read-only model pass sets build statuses and
// proposes changes the code made to the drawing. The agent itself never sees
// vibeprint. One pass per agent at most every 10 minutes, and none when its
// diff is the same as the last one checked; a turn that ends inside the 10
// minutes is checked when they are up, with the diff as it is then.
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { phoneStatus } from "./agent-status.mjs";

export const TRACK_GAP_MS = 10 * 60_000;
const DIFF_MAX_BYTES = 2 * 1024 * 1024;
export const RUN_ID = /^[A-Za-z0-9._-]{1,128}$/;

/**
 * The run's record, or null when the agent is gone. A record that cannot be
 * read is an error, not a gone agent: that would stop its tracking for good.
 */
function readRunRecord(projectDir, runId) {
  const file = path.join(projectDir, ".rudder", "runs", runId, "run.json");
  let body;
  try {
    body = fs.readFileSync(file, "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
  let record;
  try {
    record = JSON.parse(body);
  } catch (error) {
    throw new Error(`could not read agent ${runId}: ${error.message}`);
  }
  if (record?.id !== runId) throw new Error(`${file} is not agent ${runId}'s record`);
  return record;
}

/** Where the agent's edits are: its own workspace, or the checkout for a main agent. */
function runWorkspace(projectDir, record) {
  const ws = record?.workspace;
  if (ws?.enabled !== true) return projectDir;
  // Never the checkout instead: that diff is the user's, not this agent's.
  if (typeof ws.path !== "string" || !ws.path || !fs.existsSync(ws.path)) {
    throw new Error(`the agent's workspace ${ws.path || "(none)"} is gone`);
  }
  return ws.path;
}

/** What one turn's end looks like: it was working and now is not, and is not just asking to run a command. */
function turnEnded(before, record) {
  const now = phoneStatus(record);
  return before === "working" && now !== "working" && !(now === "waiting" && record?.attention === "permission");
}

function jj(cwd, args, env) {
  return new Promise((resolve) => {
    execFile("jj", args, { cwd, env, timeout: 30_000, maxBuffer: DIFF_MAX_BYTES * 2 }, (error, stdout, stderr) => {
      resolve({ ok: !error, stdout: String(stdout ?? ""), stderr: String(stderr ?? "") || error?.message || "" });
    });
  });
}

/**
 * The agent's working-copy diff, as Rudder's verifier reads it (`jj diff --git`
 * in its workspace). A stale working copy is the agent's to update (its next
 * jj command does it); the tracker never changes the agent's version control,
 * so it reports the error and reads again on its next pass.
 */
export async function readAgentDiff(cwd, env) {
  const r = await jj(cwd, ["diff", "--git", "--no-pager"], env);
  if (!r.ok) throw new Error(`could not read the agent's changes: ${r.stderr.trim().split("\n").at(-1)?.slice(0, 200) || "jj diff failed"}`);
  return Buffer.byteLength(r.stdout) > DIFF_MAX_BYTES ? Buffer.from(r.stdout).subarray(0, DIFF_MAX_BYTES).toString("utf8") : r.stdout;
}

const hashOf = (text) => createHash("sha256").update(text).digest("hex");

const authorOf = (record) => String(record?.taskSummary || record?.task || record?.id || "agent").replace(/\s+/g, " ").trim().slice(0, 80);

/**
 * @param {object} deps
 * @param {(project: object, body: object) => Promise<void>} deps.send  POST /api/track on the project's canvas; throws `{ busy: true }` when it is reading already
 * @param {(cwd: string) => Promise<string>} deps.diff
 * @param {(message: string) => void} deps.log
 * @param {() => number} [deps.now]
 */
export function createBuildTracker({ send, diff, log, now = Date.now }) {
  /** `${workspaceId}/${runId}` -> last status seen, so a turn's end is a change between two looks */
  const seen = new Map();
  const running = new Set();

  /**
   * Check one agent: its diff goes to the canvas unless it is empty or the same
   * as last time. Returns the entry to keep.
   */
  async function check(project, runId, record, entry) {
    const text = await diff(runWorkspace(project.dir, record));
    const hash = hashOf(text);
    if (!text.trim() || hash === entry.lastHash) return { ...entry, pending: false };
    await send(project, { author: authorOf(record), task: String(record.task ?? "").slice(0, 8000), diff: text });
    log(`vibeprint ${project.name}: checking ${runId}'s changes against the canvas`);
    return { ...entry, pending: false, lastHash: hash, lastAt: now(), error: null };
  }

  return {
    /**
     * One look at a project's tracked agents (`tracked`: runId -> entry). Calls
     * `save` for each entry that changed. Agents that are gone stop being tracked.
     */
    async tick(project, tracked, save) {
      for (const [runId, entry] of Object.entries(tracked)) {
        const key = `${project.workspaceId}/${runId}`;
        const record = readRunRecord(project.dir, runId);
        if (!record) {
          seen.delete(key);
          save(runId, null);
          continue;
        }
        const status = phoneStatus(record);
        const ended = turnEnded(seen.get(key), record);
        seen.set(key, status);
        const pending = entry.pending || ended;
        if (!pending || status === "working" || running.has(key)) {
          if (pending !== entry.pending) save(runId, { ...entry, pending });
          continue;
        }
        // lastAt 0: the user just turned tracking on and asked for a check now.
        if (entry.lastAt && now() - entry.lastAt < TRACK_GAP_MS) {
          if (!entry.pending) save(runId, { ...entry, pending: true });
          continue;
        }
        running.add(key);
        try {
          save(runId, await check(project, runId, record, { ...entry, pending: true }));
        } catch (error) {
          // The canvas is reading something else: try again on the next look.
          if (error?.busy) save(runId, { ...entry, pending: true });
          else save(runId, { ...entry, pending: false, error: String(error?.message || error).slice(0, 300) });
        } finally {
          running.delete(key);
        }
      }
    },
  };
}
