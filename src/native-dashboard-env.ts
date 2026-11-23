import { toolCacheEnv } from "./tool-cache.js";

/** The env the native dashboard starts with: from `rudder` (main.ts) and from
 *  a Cloud home, which starts rudder-native itself (cloud/worker/home.mjs). */
export function nativeDashboardEnv(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  return {
    ...env,
    RUDDER_NATIVE_TUI: "1",
    // Native-launched agents get the same shared tool caches as CLI launches.
    RUDDER_AGENT_TOOL_ENV: JSON.stringify(toolCacheEnv("agent", { env })),
  };
}
