// How one run.json reads on the phone, web, Mac and Slack: the status word
// and the raw facts (attention, conflict, PR) the control plane turns into
// `needsYou` + `reason` (cloud/src/agent-status.ts). Pure, so it is tested
// without starting a supervisor.

/** Whether a process still exists (EPERM: it does, just not ours). */
export function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

const LIVE_RECORD = new Set(["running", "steering", "verifying", "created"]);
const PR_STATES = new Set(["draft", "open", "merged", "closed"]);

/**
 * The PR the dashboard opened for this row and what GitHub last said about
 * it. `observedState` is written by newer dashboards; older ones leave only
 * the identity, and then the state is unknown (null), never guessed.
 */
export function prInfo(record) {
  const publish = record?.publish;
  if (!publish || typeof publish !== "object") {
    return null;
  }
  const number = Number.isFinite(publish.number) ? publish.number : null;
  const url = typeof publish.url === "string" && publish.url ? publish.url : null;
  const state = PR_STATES.has(publish.observedState) ? publish.observedState : null;
  if (number === null && !url && !state) {
    return null;
  }
  return { state, number, url };
}

/** A merge the dashboard recorded but whose change is no longer in trunk. */
export function notInTrunk(record) {
  const phase = String(record?.lifecyclePhase || "");
  return phase === "merged-not-in-trunk" || /not in (trunk|main)/i.test(phase) || record?.integration?.inTrunk === false;
}

/** The merge left conflicts for a person (or the resolver) to settle. */
export function hasConflict(record) {
  const phase = String(record?.lifecyclePhase || "");
  return record?.status === "merge-conflict" || phase === "integration-blocked";
}

/**
 * The phone's vocabulary: working / waiting / review / done / paused / failed /
 * stopped / merged / orphaned.
 *
 * `dashboardRunning`: whether this repo's dashboard (the only thing that can
 * host the agent) is up. A row that says running while it is not, or whose
 * recorded process is gone, is not working, whatever the record says.
 */
export function phoneStatus(record, { dashboardRunning = true, alive = pidAlive } = {}) {
  const status = String(record?.status || "");
  const phase = String(record?.lifecyclePhase || "");
  const pr = prInfo(record);
  if (LIVE_RECORD.has(status)) {
    if (!dashboardRunning) {
      return "paused";
    }
    const pid = record?.process?.pid;
    if (Number.isInteger(pid) && pid > 0 && !alive(pid)) {
      return "orphaned";
    }
    return record.attention ? "waiting" : "working";
  }
  if (status === "merged") {
    // Said merged, but the work left main: not a success to show.
    return notInTrunk(record) ? "review" : "merged";
  }
  if (status === "completed" || status === "merge-conflict") {
    if (phase === "delivery blocked") {
      return "failed";
    }
    if (status === "merge-conflict") {
      return "review";
    }
    if (pr?.state === "merged") {
      return "merged";
    }
    // A finished /swarm experiment is a candidate to compare, not work that owes
    // you a merge: a swarm of 18 would otherwise fill "Needs you".
    if (record?.swarmExperiment) {
      return "done";
    }
    if (phase === "review" || phase.includes("needs merge") || phase.includes("proof") || phase === "integration-blocked") {
      return "review";
    }
    return "done";
  }
  if (status === "cancelled") {
    return "stopped";
  }
  if (["failed", "paused", "orphaned"].includes(status)) {
    return status;
  }
  if (status === "migrated") {
    return "done";
  }
  return status || "done";
}

/** The status fields of an agent summary (the rest is title, preview, asks). */
export function statusFields(record, options = {}) {
  return {
    status: phoneStatus(record, options),
    phase: record?.lifecyclePhase ?? null,
    attention: record?.attention ?? null,
    conflict: hasConflict(record),
    notInTrunk: record?.status === "merged" && notInTrunk(record),
    pr: prInfo(record),
  };
}
