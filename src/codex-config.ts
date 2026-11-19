// Rudder workers run Codex as a child process, so do not inherit desktop-app
// notification hooks that expect the official signed app launch chain.
// Must match CODEX_COMMON_CONFIG in native/src/launch.rs; a test compares them.
// Compaction and the output cap come from the v4 session measurement (PLAN.md
// section 20): context ran to about 451K of a 475K window before compacting.
// No update check: Codex's "Update available" screen stops an unattended agent.
export const CODEX_RUDDER_COMMON_CONFIG_ARGS = [
  "-c",
  "notify=[]",
  "-c",
  'model_reasoning_summary="detailed"',
  "-c",
  "model_auto_compact_token_limit=150000",
  "-c",
  "tool_output_token_limit=6000",
  "-c",
  "check_for_update_on_startup=false",
];

/** Full-capability profile for implementation, review, and shipping agents. */
export const CODEX_RUDDER_WORKER_CONFIG_ARGS = [
  ...CODEX_RUDDER_COMMON_CONFIG_ARGS,
  "-c",
  "features.plugins=true",
  "-c",
  "features.computer_use=true",
];

/** Restricted profile for read-only planners and the orchestration conductor. */
export const CODEX_RUDDER_PLANNER_CONFIG_ARGS = [
  ...CODEX_RUDDER_COMMON_CONFIG_ARGS,
  "-c",
  "features.plugins=false",
  "-c",
  "features.computer_use=false",
];

/**
 * `-c shell_environment_policy.set.<NAME>=<value>` for each variable. Codex
 * builds its own environment for shell commands, and a policy such as
 * `inherit = "core"` drops every other variable, so values an agent's
 * commands need (the shared tool cache, src/tool-cache.ts) go into the policy
 * itself. Claude passes its environment through unchanged.
 */
export function codexShellEnvConfigArgs(vars: Record<string, string>): string[] {
  return Object.entries(vars)
    .filter(([name]) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(name))
    .flatMap(([name, value]) => ["-c", `shell_environment_policy.set.${name}=${JSON.stringify(value)}`]);
}
