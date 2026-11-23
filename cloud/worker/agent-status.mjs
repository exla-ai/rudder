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

/**
 * What a working row's finished turn left running: background shells and
 * Monitors, async subagents and busy teammates (`{shells, agents, teammates}`,
 * dashboard background.rs). The agent is waiting on them, not ready for review;
 * each restarts it when it finishes. Null otherwise.
 */
export function backgroundWork(record) {
  const work = record?.background;
  if (!work || typeof work !== "object") {
    return null;
  }
  return { shells: work.shells, agents: work.agents, teammates: work.teammates };
}

/** The status fields of an agent summary (the rest is title, preview, asks). */
export function statusFields(record, options = {}) {
  const status = phoneStatus(record, options);
  return {
    status,
    phase: record?.lifecyclePhase ?? null,
    attention: record?.attention ?? null,
    background: status === "working" ? backgroundWork(record) : null,
    conflict: hasConflict(record),
    notInTrunk: record?.status === "merged" && notInTrunk(record),
    pr: prInfo(record),
  };
}

// Prompts an agent is blocked on until someone answers: a question (Claude's
// `agent_needs_input` or an MCP elicitation, OpenCode's question) or an
// approval. An interrupted or failed turn has ended.
const OPEN_PROMPT = new Set(["input", "permission"]);

/**
 * Whether restarting the machine now would cut this agent off: a turn in
 * flight or starting (working includes a finished turn still waiting on its
 * background shells, subagents or teammates, `background` above), or a
 * prompt waiting for its answer (a hook holds it, and a restart drops the
 * hook). Takes an agent summary as the control plane
 * gets it. The control plane's idle update (server.ts homeBusyAgents) and
 * the app's Update now (cloud/app/home-update.js busyAgents) count the same
 * agents; the home checks again right before its host restarts it.
 */
export function holdsRestart(agent) {
  const status = agent?.status;
  if (status === "working" || status === "starting") return true;
  if (Array.isArray(agent?.asks) && agent.asks.length > 0) return true;
  return status === "waiting" && OPEN_PROMPT.has(agent?.attention);
}
