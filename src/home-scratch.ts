// On the cloud home each agent gets its own scratch directory,
// <volume>/scratch/<run id>, as RUDDER_SCRATCH, with TMPDIR inside it, so
// temporary files stop piling up at the top of /workspace. The hourly cleanup
// (cloud/worker/cleanup.mjs) deletes it after the run finishes. The native
// dashboard does the same for the agents it launches (signals.rs
// with_home_scratch); this covers the detached `rudder __worker`.

import fs from "node:fs";
import path from "node:path";

export function homeScratchEnv(runId: string, env: NodeJS.ProcessEnv = process.env, platform: NodeJS.Platform = process.platform): Record<string, string> {
  if (platform !== "linux" || env.RUDDER_CLOUD_RUNTIME !== "home" || !/^[A-Za-z0-9_-][A-Za-z0-9._-]*$/.test(runId)) {
    return {};
  }
  const scratch = path.join(env.RUDDER_HOME_VOLUME || "/workspace", "scratch", runId);
  const tmp = path.join(scratch, "tmp");
  try {
    fs.mkdirSync(tmp, { recursive: true });
  } catch {
    return {};
  }
  return { RUDDER_SCRATCH: scratch, TMPDIR: tmp };
}
