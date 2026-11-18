import type { BackendId, EffortLevel } from "./types.js";

/** Rudder's one default reasoning tier. Matches `DEFAULT_EFFORT` in native/src/models.rs. */
export const DEFAULT_EFFORT: EffortLevel = "high";

/**
 * Effort config for every Codex launch. Always explicit: unset would fall
 * through to the user's global Codex config, which is how the measured v4
 * session ran every step at its highest tier (PLAN.md section 20).
 */
export function codexEffortConfigArgs(effort: EffortLevel | undefined): string[] {
  return ["-c", `model_reasoning_effort="${normalizeEffortForBackend("codex", effort ?? DEFAULT_EFFORT)}"`];
}

export function normalizeEffortForBackend(backend: BackendId, effort: EffortLevel | undefined): EffortLevel | undefined {
  if (!effort) {
    return undefined;
  }
  // opencode exposes no reasoning-effort flag at all, so a stored effort is
  // dropped rather than passed to a CLI that would reject it.
  if (backend === "opencode") {
    return undefined;
  }
  if (backend === "codex" && effort === "max") {
    return "xhigh";
  }
  return effort;
}
