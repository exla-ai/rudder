import type { EffortLevel } from "./types.js";

/**
 * OpenCode runs a model at an effort through the model's variant of that
 * name ("high", "max"; `opencode models --verbose` lists each model's). The
 * TUI has no flag for it: an agent's `variant` in its config applies when
 * the agent runs its configured model, so a launch names both through
 * OPENCODE_CONFIG_CONTENT. Null without a model or an effort. The dashboard
 * builds the same config (native/src/launch.rs opencode_variant_config).
 */
export function opencodeVariantConfig(agent: "build" | "plan", model: string | undefined, effort: EffortLevel | undefined): string | null {
  if (!model || !effort) {
    return null;
  }
  return JSON.stringify({ agent: { [agent]: { model, variant: effort } } });
}
