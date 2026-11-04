// The scaler's memory between evaluations: when each home was last changed and
// how (the cooldowns are measured from these), recent failed attempts (the
// retry backoff), and the last decisions (to log only what changed). Without
// it a restarted scaler would resize a home it resized a minute ago.
//
// One JSON file, written whole to a temporary file and renamed into place, so
// a crash mid-write leaves the previous version, never half of one.

import fs from "node:fs";
import path from "node:path";

export const DEFAULT_STATE_PATH = "observability/scaler/.state.json";
const VERSION = 1;
// A home not seen for a week is gone (deleted or moved); its cooldowns ended long ago.
const FORGET_AFTER_MS = 7 * 24 * 60 * 60_000;

export function statePath(env = process.env, cwd = process.cwd()) {
  return path.resolve(cwd, env.RUDDER_SCALER_STATE || DEFAULT_STATE_PATH);
}

export function emptyState() {
  return { version: VERSION, homes: {}, decisions: [], updatedAt: null };
}

/**
 * Read the state file. Missing means a first run. Unreadable means the file is
 * moved aside (kept for a person to look at) and the scaler starts empty.
 */
export function loadState(file, { log = () => {} } = {}) {
  let text;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (error) {
    if (error.code !== "ENOENT") log(`state: cannot read ${file} (${error.code || error.message}); starting empty`);
    return emptyState();
  }
  try {
    const parsed = JSON.parse(text);
    if (!parsed || typeof parsed !== "object" || parsed.version !== VERSION) throw new Error(`unknown version ${parsed?.version}`);
    return {
      version: VERSION,
      homes: parsed.homes && typeof parsed.homes === "object" ? parsed.homes : {},
      decisions: Array.isArray(parsed.decisions) ? parsed.decisions : [],
      updatedAt: parsed.updatedAt ?? null,
    };
  } catch (error) {
    const aside = `${file}.corrupt-${Date.now()}`;
    try {
      fs.renameSync(file, aside);
    } catch {
      // leave it; the next save replaces it
    }
    log(`state: ${file} is unreadable (${error.message}); moved to ${aside}, starting empty, so cooldowns are forgotten`);
    return emptyState();
  }
}

export function saveState(file, state) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  const fd = fs.openSync(tmp, "w");
  try {
    fs.writeSync(fd, `${JSON.stringify(state, null, 2)}\n`);
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
  fs.renameSync(tmp, file);
}

/** Note the homes seen and the decisions made in one evaluation. */
export function recordEvaluation(state, { homes = [], decisions = [], now = Date.now() } = {}) {
  const next = { ...emptyState(), ...state, homes: { ...state?.homes } };
  for (const home of homes) {
    const entry = { ...next.homes[home.homeId] };
    entry.machineId = home.machineId ?? entry.machineId ?? null;
    entry.region = home.region ?? entry.region ?? null;
    if (typeof home.lastSeenSeconds === "number") entry.lastSeenAt = now - home.lastSeenSeconds * 1000;
    next.homes[home.homeId] = entry;
  }
  for (const [homeId, entry] of Object.entries(next.homes)) {
    const lastSeenAt = entry.lastSeenAt ?? 0;
    const lastApplied = Math.max(0, ...Object.values(entry.applied ?? {}).map((applied) => applied?.at ?? 0));
    if (now - Math.max(lastSeenAt, lastApplied) > FORGET_AFTER_MS) delete next.homes[homeId];
  }
  next.decisions = decisions;
  next.updatedAt = now;
  return next;
}

/** Note what happened when the executor acted on a decision. */
export function recordOutcome(state, decision, result, now = Date.now()) {
  const homes = { ...state.homes };
  const entry = { applied: {}, failed: {}, ...homes[decision.homeId] };
  entry.machineId = decision.machineId ?? entry.machineId ?? null;
  if (result?.applied) {
    entry.applied = { ...entry.applied, [decision.action]: { at: now, time: new Date(now).toISOString(), target: decision.target ?? null, result } };
    const failed = { ...entry.failed };
    delete failed[decision.action];
    entry.failed = failed;
  } else {
    const error = result?.refused ? `refused: ${result.refused}` : (result?.error || "unknown error");
    entry.failed = { ...entry.failed, [decision.action]: { at: now, time: new Date(now).toISOString(), target: decision.target ?? null, error } };
  }
  homes[decision.homeId] = entry;
  return { ...state, homes };
}
