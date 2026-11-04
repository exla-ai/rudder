// Runs the REAL home telemetry (cloud/worker/telemetry.mjs) inside a Linux
// container, against that container's own /proc and statfs, with a couple of
// fake agent rows so the agent panels have something to count. Proves the
// production code path end to end: SDK -> OTLP -> collector -> Prometheus.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const { startHomeTelemetry } = await import("/opt/rudder-worker/telemetry.mjs");

// Node is PID 1 in the container, which the agent reader rightly skips, so
// the "live" agents point at a real child process instead.
const agent = spawn("sleep", ["infinity"], { stdio: "ignore" });
const root = fs.mkdtempSync(path.join(os.tmpdir(), "home-sim-"));
const codeDir = path.join(root, "code");
const rows = [
  ["rudder", "run-working", { status: "running", process: { pid: agent.pid } }],
  ["libra", "run-idle", { status: "completed", process: { pid: agent.pid } }],
  ["libra", "run-asleep", { status: "completed", parked: { at: new Date().toISOString(), reason: "idle 3 days" } }],
  ["offstage", "run-done", { status: "merged" }],
];
for (const [project, id, record] of rows) {
  const dir = path.join(codeDir, project, ".rudder", "runs", id);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "run.json"), JSON.stringify({ id, createdAt: String(Date.now()), ...record }));
}

let connected = true;
const telemetry = await startHomeTelemetry({
  homeId: process.env.RUDDER_WORKSPACE_ID || "home-local",
  paths: { codeDir, home: root, rudderHome: path.join(root, ".rudder") },
  controlConnected: () => connected,
  projectCount: () => 3,
  log: (line) => console.log(line),
});
if (!telemetry.enabled) {
  console.error("home-sim: telemetry did not start (is RUDDER_OTEL_ENDPOINT set and the SDK installed?)");
  process.exit(1);
}

// Now and then the control socket drops and comes back, like a real home.
setInterval(() => {
  connected = false;
  setTimeout(() => {
    connected = true;
    telemetry.recordReconnect();
  }, 20_000);
}, 10 * 60_000);
// An hourly cleanup that parks one agent.
setInterval(() => telemetry.recordCleanupRun({ trigger: "hourly", outcome: "ok", actions: { park: 1, stop: 0 } }), 60 * 60_000);
telemetry.recordCleanupRun({ trigger: "boot", outcome: "ok", actions: { park: 0, stop: 0 } });

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.on(signal, () => void telemetry.shutdown().finally(() => process.exit(0)));
}
console.log("home-sim: running");
