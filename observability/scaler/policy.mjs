// The scaler's rules: what a home machine needs so it does not run out of
// memory or disk. On 2026-11-02 a 16 GB home with no swap filled up with idle
// agents, thrashed (memory full avg60 75%, load 235) and every terminal
// attached to it showed "Cloud machine reconnecting". The home now cleans up
// after itself; these rules cover what the cleanup cannot: a home that needs
// swap, more memory, a bigger volume, or a person to look at it.
//
// Pure: Prometheus readings and the scaler's remembered history in, decisions
// out. Nothing here talks to Fly. The order of preference is the cheap fix
// first (cleanup), then the one that needs no restart (swap from the new
// image, volume extend), and a memory resize only when pressure stayed high
// after a cleanup, because a resize restarts the machine.
//
// A missing reading is null and never triggers anything by itself: no data is
// not evidence that a home is in trouble.

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
export const MiB = 1024 ** 2;
export const GiB = 1024 ** 3;

export const ACTIONS = Object.freeze([
  "investigate-silent",
  "run-cleanup",
  "add-swap",
  "scale-up-memory",
  "scale-down-memory",
  "extend-volume",
]);
export const STATES = Object.freeze(["recommended", "waiting-for-idle", "cooldown", "applied", "failed"]);
// What the Fly executor can carry out. run-cleanup waits for a control-plane
// call; add-swap and investigate-silent are for people.
export const EXECUTABLE_ACTIONS = Object.freeze(new Set(["scale-up-memory", "scale-down-memory", "extend-volume"]));
export const RESIZE_ACTIONS = Object.freeze(new Set(["scale-up-memory", "scale-down-memory"]));
// Control-plane states (rudder_control_home_state) in which a home should be
// reporting. The others (paused, stopped, failed) are quiet on purpose.
const EXPECTED_RUNNING = new Set(["running", "queued"]);

export const DEFAULT_CONFIG = Object.freeze({
  // Memory is short right now: the same line the home's own cleanup uses
  // (cloud/worker/cleanup.mjs memoryIsShort).
  shortAvailableRatio: 0.12,
  shortMemoryFull: 0.20,
  // Memory stayed short for the last 30 minutes.
  sustainedMemoryFull: 0.10,
  sustainedAvailableRatio: 0.10,
  sustainedSwapUsedRatio: 0.5,
  // Memory sizes for a performance 4-CPU machine, smallest first.
  memoryStepsMb: Object.freeze([16384, 24576, 32768]),
  // The kernel's MemTotal is the machine's memory minus what firmware and the
  // kernel keep (about 400 MiB of 16 GiB), so a reading within this share
  // below a step counts as that step.
  stepTolerance: 0.08,
  // Memory has been underused for a day.
  scaleDownUsedRatio: 0.40,
  scaleDownMemoryFull: 0.01,
  // A 24 h maximum computed over an hour of data says nothing about the day.
  scaleDownMinHistorySeconds: 20 * 3600,
  scaleUpCooldownMs: 6 * HOUR,
  scaleDownCooldownMs: 24 * HOUR,
  diskFreeRatio: 0.10,
  diskFreeMinBytes: 8 * GiB,
  volumeGrowth: 1.5,
  volumeMaxGb: 500,
  extendCooldownMs: 12 * HOUR,
  silentAfterSeconds: 600,
  // After a failed or refused attempt, wait before trying the same thing again.
  failureBackoffMs: 30 * MINUTE,
  // Restarts per evaluation loop, so a bad reading cannot restart the fleet.
  maxResizesPerEvaluation: 1,
});

/**
 * Decide what each home needs. `homes` are collectHomes() rows; `state` is the
 * scaler's state file (applied and failed attempts per home and action).
 * Returns [{ homeId, machineId, region, action, state, reason, target? }].
 */
export function evaluate(homes, { now = Date.now(), state = {}, config = {} } = {}) {
  const cfg = { ...DEFAULT_CONFIG, ...config };
  const decisions = [];
  for (const home of homes ?? []) {
    if (!home || typeof home.homeId !== "string" || !home.homeId) continue;
    decisions.push(...evaluateHome(home, { now, history: state?.homes?.[home.homeId] ?? {}, cfg }));
  }
  return decisions;
}

function evaluateHome(home, { now, history, cfg }) {
  const decide = (action, fields) => ({
    homeId: home.homeId,
    machineId: home.machineId ?? null,
    region: home.region ?? null,
    action,
    ...fields,
  });

  // A home that stopped reporting is judged on nothing else: its other
  // readings are stale or gone. One the control plane stopped or paused is
  // quiet on purpose and gets no decision at all; without a control-plane
  // state the scaler cannot tell, so it asks a person to look.
  if (isNum(home.lastSeenSeconds) && home.lastSeenSeconds > cfg.silentAfterSeconds) {
    if (typeof home.controlState === "string" && home.controlState && !EXPECTED_RUNNING.has(home.controlState)) return [];
    return [decide("investigate-silent", { state: "recommended", reason: silentReason(home) })];
  }

  const decisions = [];
  const total = home.memoryTotalBytes;
  const available = home.memoryAvailableBytes;
  const availableRatio = isNum(total) && total > 0 && isNum(available) ? available / total : null;
  const lowAvailable = availableRatio !== null && availableRatio < cfg.shortAvailableRatio;
  const highPressure = isNum(home.memoryFullAvg60) && home.memoryFullAvg60 >= cfg.shortMemoryFull;
  const short = lowAvailable || highPressure;
  const memoryNow = `${gib(available)} of ${gib(total)} available (${pct(availableRatio)}), memory full avg60 ${pct(home.memoryFullAvg60)}`;

  // The cheap fix: the cleanup parks idle agents and stops leftovers. The home
  // runs it itself when memory is short; this catches the homes that did not.
  if (short && !(isNum(home.cleanupRunsLast15m) && home.cleanupRunsLast15m >= 1)) {
    const runs = isNum(home.cleanupRunsLast15m)
      ? "No cleanup ran in the last 15 minutes."
      : "This home reports no cleanup runs, so it may be on an image without the cleanup.";
    decisions.push(decide("run-cleanup", {
      state: "recommended",
      reason: `Memory is short: ${memoryNow}. ${runs} Run the cleanup now.`,
    }));
  }

  if (short && home.swapTotalBytes === 0) {
    decisions.push(decide("add-swap", {
      state: "recommended",
      reason: `The home has no swap and memory is short: ${memoryNow}. The current worker image adds a swap file at boot, so the home gets one when it next restarts while idle.`,
    }));
  }

  const step = memoryStep(total, cfg);
  const swapUsedRatio = isNum(home.swapTotalBytes) && home.swapTotalBytes > 0 && isNum(home.swapFreeBytes)
    ? (home.swapTotalBytes - home.swapFreeBytes) / home.swapTotalBytes
    : null;
  const pressureSustained = isNum(home.memoryFullAvg60Over30m) && home.memoryFullAvg60Over30m >= cfg.sustainedMemoryFull;
  const availableSustained = isNum(home.availableRatioOver30mAvg)
    && home.availableRatioOver30mAvg < cfg.sustainedAvailableRatio
    && swapUsedRatio !== null && swapUsedRatio > cfg.sustainedSwapUsedRatio;
  // A home without cleanup metrics cannot show the cleanup ran; resizing it is
  // still better than letting it fall over, and the reason says so.
  const cleanupKnown = isNum(home.cleanupRunsLast1h);
  const cleanupTried = !cleanupKnown || home.cleanupRunsLast1h >= 1;

  if ((pressureSustained || availableSustained) && cleanupTried && step) {
    const targetMb = stepUp(step, cfg);
    if (targetMb) {
      const why = pressureSustained
        ? `memory full avg60 averaged ${pct(home.memoryFullAvg60Over30m)} over 30 minutes (limit ${pct(cfg.sustainedMemoryFull)})`
        : `available memory averaged ${pct(home.availableRatioOver30mAvg)} over 30 minutes with ${gib(home.swapTotalBytes - home.swapFreeBytes)} of ${gib(home.swapTotalBytes)} swap used`;
      const tried = cleanupKnown
        ? `the cleanup ran ${plural(home.cleanupRunsLast1h, "time")} in the last hour`
        : "this home reports no cleanup metrics, so it is not known whether the cleanup ran";
      decisions.push(gate(decide("scale-up-memory", {
        target: { memoryMb: targetMb },
        reason: `Memory stayed short: ${why}, and ${tried}. Resize from ${stepText(step)} to ${targetMb} MiB.`,
      }), { now, history, cfg, home, cooldownMs: cfg.scaleUpCooldownMs, cooldownActions: RESIZE_ACTIONS, restarts: true }));
    }
  }

  // Up wins: never shrink a home that is or was just short of memory.
  const upDecided = decisions.some((decision) => decision.action === "scale-up-memory");
  const historyShort = isNum(home.memoryHistorySeconds) && home.memoryHistorySeconds < cfg.scaleDownMinHistorySeconds;
  if (!upDecided && !short && !pressureSustained && !availableSustained && !historyShort
    && step && step.index >= 1
    && isNum(home.usedRatioMax24h) && home.usedRatioMax24h < cfg.scaleDownUsedRatio
    && isNum(home.memoryFullMax24h) && home.memoryFullMax24h < cfg.scaleDownMemoryFull) {
    const targetMb = cfg.memoryStepsMb[step.index - 1];
    decisions.push(gate(decide("scale-down-memory", {
      target: { memoryMb: targetMb },
      reason: `Memory is underused: at most ${pct(home.usedRatioMax24h)} used and memory full avg60 at most ${pct(home.memoryFullMax24h)} over 24 hours. Resize from ${stepText(step)} to ${targetMb} MiB.`,
    }), { now, history, cfg, home, cooldownMs: cfg.scaleDownCooldownMs, cooldownActions: RESIZE_ACTIONS, restarts: true }));
  }

  const diskTotal = home.diskWorkspaceTotalBytes;
  const diskFree = home.diskWorkspaceFreeBytes;
  if (isNum(diskTotal) && diskTotal > 0 && isNum(diskFree)
    && (diskFree < diskTotal * cfg.diskFreeRatio || diskFree < cfg.diskFreeMinBytes)) {
    const sizeGb = diskTotal / GiB;
    const diskNow = `/workspace has ${gib(diskFree)} free of ${gib(diskTotal)} (${pct(diskFree / diskTotal)})`;
    // The filesystem is a few percent smaller than the volume under it, so a
    // volume at the cap reads a little below it.
    if (sizeGb >= cfg.volumeMaxGb * 0.95) {
      decisions.push(decide("extend-volume", {
        state: "recommended",
        reason: `${diskNow} and the volume is at the ${cfg.volumeMaxGb} GB cap. Free space on it by hand.`,
      }));
    } else {
      const targetGb = Math.min(Math.ceil(sizeGb * cfg.volumeGrowth), cfg.volumeMaxGb);
      decisions.push(gate(decide("extend-volume", {
        target: { sizeGb: targetGb },
        reason: `${diskNow}. Extend the volume to ${targetGb} GB; extending does not restart the machine.`,
      }), { now, history, cfg, home, cooldownMs: cfg.extendCooldownMs, cooldownActions: new Set(["extend-volume"]), restarts: false }));
    }
  }

  return decisions;
}

// The state a decision is in before anyone acts on it: in cooldown after a
// recent change, failed after a recent bad attempt, waiting while agents work
// (a resize restarts the machine), or recommended.
function gate(decision, { now, history, cfg, home, cooldownMs, cooldownActions, restarts }) {
  const last = latestApplied(history, cooldownActions);
  if (last && now - last.at < cooldownMs) {
    const what = decision.action === "extend-volume" ? "The volume was extended" : "The home was resized";
    return {
      ...decision,
      state: "cooldown",
      reason: `${decision.reason} ${what} at ${isoMinute(last.at)}; waiting until ${isoMinute(last.at + cooldownMs)} before changing it again.`,
    };
  }
  const failed = history?.failed?.[decision.action];
  if (failed && isNum(failed.at) && now - failed.at < cfg.failureBackoffMs) {
    return {
      ...decision,
      state: "failed",
      reason: `${decision.reason} The last attempt at ${isoMinute(failed.at)} failed (${failed.error || "unknown error"}); retrying after ${isoMinute(failed.at + cfg.failureBackoffMs)}.`,
    };
  }
  if (!restarts) return { ...decision, state: "recommended" };
  const idle = idleFor15m(home);
  const clients = isNum(home.clients) && home.clients > 0
    ? ` ${plural(home.clients, "attached client")} will see it reconnect.`
    : "";
  if (!idle.idle) {
    return {
      ...decision,
      state: "waiting-for-idle",
      reason: `${decision.reason} A resize restarts the machine, so it waits until no agent has worked or waited for 15 minutes (${idle.why}).`,
    };
  }
  return {
    ...decision,
    state: "recommended",
    reason: `${decision.reason} No agent worked or waited in the last 15 minutes, so the restart interrupts no one.${clients}`,
  };
}

function idleFor15m(home) {
  if (!isNum(home.busyMax15m)) return { idle: false, why: "agent activity over the last 15 minutes is unknown" };
  const busyNow = (isNum(home.agentsWorking) ? home.agentsWorking : 0) + (isNum(home.agentsWaiting) ? home.agentsWaiting : 0);
  const busy = Math.max(home.busyMax15m, busyNow);
  if (busy > 0) return { idle: false, why: `up to ${plural(busy, "agent")} working or waiting` };
  return { idle: true };
}

function latestApplied(history, actions) {
  let latest = null;
  for (const [action, entry] of Object.entries(history?.applied ?? {})) {
    if (!actions.has(action) || !isNum(entry?.at)) continue;
    if (!latest || entry.at > latest.at) latest = entry;
  }
  return latest;
}

function silentReason(home) {
  const minutes = Math.round(home.lastSeenSeconds / 60);
  const parts = [`The home has not reported metrics for ${minutes} minutes.`];
  if (home.controlState) parts.push(`The control plane lists it as ${home.controlState}.`);
  if (home.controlConnected === 1) {
    parts.push("The control plane still has its control socket open, so its telemetry may have stopped.");
  } else if (home.controlConnected === 0) {
    parts.push("The control plane shows it disconnected: it may be asleep, restarting or out of memory.");
  }
  if (isNum(home.clients) && home.clients > 0) parts.push(`${plural(home.clients, "client")} attached.`);
  if (isNum(home.memoryFullAvg60Over30m)) parts.push(`Before it went quiet, memory full avg60 averaged ${pct(home.memoryFullAvg60Over30m)}.`);
  parts.push("Look at the machine; the scaler changes nothing it cannot see.");
  return parts.join(" ");
}

/**
 * Where a home sits on the memory ladder: { mb, index, stepMb }, with index -1
 * and stepMb null when its memory is not one of the steps (sized by hand).
 * Null without a reading.
 */
export function memoryStep(totalBytes, cfg = DEFAULT_CONFIG) {
  if (!isNum(totalBytes) || totalBytes <= 0) return null;
  const mb = totalBytes / MiB;
  const index = cfg.memoryStepsMb.findIndex((step) => step >= mb && step - mb <= step * cfg.stepTolerance);
  return { mb, index, stepMb: index >= 0 ? cfg.memoryStepsMb[index] : null };
}

function stepUp(step, cfg) {
  if (step.index >= 0) return cfg.memoryStepsMb[step.index + 1] ?? null;
  return cfg.memoryStepsMb.find((candidate) => candidate > step.mb) ?? null;
}

function stepText(step) {
  return step.stepMb ? `${step.stepMb} MiB` : `about ${Math.round(step.mb)} MiB`;
}

// ---------------------------------------------------------------------------

export function isNum(value) {
  return typeof value === "number" && Number.isFinite(value);
}

function gib(bytes) {
  return isNum(bytes) ? `${(bytes / GiB).toFixed(1)} GiB` : "unknown";
}

function pct(ratio) {
  return isNum(ratio) ? `${Number((ratio * 100).toPrecision(3))}%` : "unknown";
}

function plural(count, noun) {
  const n = Number.isInteger(count) ? count : Math.round(count * 10) / 10;
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

function isoMinute(ms) {
  return `${new Date(ms).toISOString().slice(0, 16)}Z`;
}
