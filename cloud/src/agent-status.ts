// The ONE definition of "needs you" (AGENTS.md §9.8.2). The supervisor sends
// each agent's status word plus raw facts (asks, attention, phase, conflict,
// notInTrunk, pr); the control plane stamps `needsYou` + `reason` on every
// agent it serves, and the web app, Mac menu bar, team view, voice and Slack
// all read those two fields instead of re-deriving them.
//
// Also: what a sleeping home's last state means. Agents can only work while
// the supervisor is connected, so a state from a closed socket is served with
// its live rows frozen as "paused" + `stale: true` + `lastSeen`.

export type NeedsReason =
  | "permission"
  | "question"
  | "plan"
  | "input"
  | "interrupted"
  | "error"
  | "conflict"
  | "not-in-trunk"
  | "delivery"
  | "delivery-blocked"
  | "review"
  | "failed";

export type StatusAgent = {
  status?: unknown;
  phase?: unknown;
  attention?: unknown;
  asks?: unknown;
  conflict?: unknown;
  notInTrunk?: unknown;
  pending?: unknown;
  pr?: unknown;
  needsYou?: unknown;
  reason?: unknown;
  [key: string]: unknown;
};

/** Statuses that are only true while the machine is up. */
export const LIVE_STATUSES = new Set(["working", "starting", "waiting"]);

const ASK_KINDS = new Set(["permission", "question", "plan"]);
const ATTENTION = new Set(["permission", "input", "interrupted", "error"]);

const text = (value: unknown): string => (typeof value === "string" ? value : "");

function prState(agent: StatusAgent): string | null {
  const pr = agent.pr;
  if (!pr || typeof pr !== "object") {
    return null;
  }
  const state = (pr as { state?: unknown }).state;
  return typeof state === "string" ? state : null;
}

function firstAskKind(asks: unknown): string | null {
  if (!Array.isArray(asks) || asks.length === 0) {
    return null;
  }
  const kind = (asks[0] as { kind?: unknown } | null)?.kind;
  return typeof kind === "string" && ASK_KINDS.has(kind) ? kind : "permission";
}

/** A status word corrected for supervisors older than agent-status.mjs. */
export function normalizedStatus(agent: StatusAgent): string {
  const status = text(agent.status);
  const phase = text(agent.phase);
  if ((status === "done" || status === "review") && phase === "delivery blocked") {
    return "failed";
  }
  if ((status === "done" || status === "review") && prState(agent) === "merged") {
    return "merged";
  }
  return status;
}

/**
 * Needs you = a prompt is waiting (permission / question / plan), the agent
 * stopped on input / an interrupt / an error, its merge has conflicts, its
 * merged work left main, delivery proof is missing, its turn failed, or
 * finished workspace work awaits review. A row whose PR is open (or a draft)
 * is waiting on GitHub, not on you; a row whose PR merged is merged.
 */
export function agentNeeds(agent: StatusAgent): { needsYou: boolean; reason: NeedsReason | null } {
  const none = { needsYou: false, reason: null };
  if (!agent || agent.pending === true) {
    return none;
  }
  const ask = firstAskKind(agent.asks);
  if (ask) {
    return { needsYou: true, reason: ask as NeedsReason };
  }
  const status = normalizedStatus(agent);
  const phase = text(agent.phase);
  if (status === "waiting") {
    const attention = text(agent.attention);
    return { needsYou: true, reason: (ATTENTION.has(attention) ? attention : "input") as NeedsReason };
  }
  if (status === "failed") {
    return { needsYou: true, reason: phase === "delivery blocked" ? "delivery-blocked" : "failed" };
  }
  if (status === "review") {
    if (agent.conflict === true || phase === "integration-blocked") {
      return { needsYou: true, reason: "conflict" };
    }
    if (agent.notInTrunk === true) {
      return { needsYou: true, reason: "not-in-trunk" };
    }
    if (phase.includes("proof")) {
      return { needsYou: true, reason: "delivery" };
    }
    const pr = prState(agent);
    if (pr === "open" || pr === "draft" || pr === "merged") {
      return none;
    }
    return { needsYou: true, reason: "review" };
  }
  return none;
}

/** Reads the stamped field; derives it for an agent that was never stamped. */
export function isNeedsYou(agent: StatusAgent): boolean {
  return typeof agent?.needsYou === "boolean" ? agent.needsYou : agentNeeds(agent).needsYou;
}

/**
 * Stamps `needsYou` + `reason` on one agent. With `live: false` (the home's
 * control socket is gone), rows that could only be true on a running machine
 * become "paused" with `stale: true` and `lastSeen` (ms), and their prompts
 * are dropped: the hook processes holding them died with the machine.
 */
export function annotateAgent<T extends StatusAgent>(agent: T, options: { live: boolean; lastSeen?: number | null }): T {
  if (!agent || typeof agent !== "object") {
    return agent;
  }
  let next: StatusAgent = { ...agent, status: normalizedStatus(agent) || agent.status };
  if (!options.live) {
    const frozen = LIVE_STATUSES.has(text(next.status)) || (Array.isArray(next.asks) && next.asks.length > 0);
    if (frozen) {
      next = {
        ...next,
        status: LIVE_STATUSES.has(text(next.status)) ? "paused" : next.status,
        asks: [],
        attention: null,
        stale: true,
        lastSeen: options.lastSeen ?? null,
      };
    }
  }
  const needs = agentNeeds(next);
  return { ...next, needsYou: needs.needsYou, reason: needs.reason } as T;
}
