import path from "node:path";
import type { EffortLevel } from "./types.js";
import { rudderHome } from "./util.js";

/**
 * OpenCode runs a model at an effort through the model's variant of that
 * name ("high", "max"; `opencode models --verbose` lists each model's). The
 * TUI has no flag for it and sends the variant it last used with the model in
 * any session, whatever the config says, so Rudder's plugin
 * (native/src/opencode_plugin.mjs, which the dashboard installs in the
 * signals dir) sets it on every message on the run's model. The environment
 * that loads the plugin and names both, as `NAME=value` pairs; none without a
 * model or an effort. The dashboard builds the same (native/src/launch.rs
 * opencode_tui).
 */
export function opencodeEffortEnv(model: string | undefined, effort: EffortLevel | undefined): string[] {
  if (!model || !effort) {
    return [];
  }
  const plugin = path.join(rudderHome(), "signals", "opencode-plugin.mjs");
  return [
    `OPENCODE_CONFIG_CONTENT=${JSON.stringify({ plugin: [plugin] })}`,
    `RUDDER_OPENCODE_MODEL=${model}`,
    `RUDDER_OPENCODE_VARIANT=${effort}`,
  ];
}
