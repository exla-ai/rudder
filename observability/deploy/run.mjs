// Supervises the four processes of the observability machine. If any one
// exits, stop the rest and exit non-zero so Fly restarts the whole machine:
// a half-running stack (Grafana up, Prometheus down) looks healthy from
// outside while silently dropping every sample.
import { spawn } from "node:child_process";
import fs from "node:fs";

const DATA = process.env.RUDDER_OBSERVABILITY_DATA || "/data";
fs.mkdirSync(`${DATA}/prometheus`, { recursive: true });
fs.mkdirSync(`${DATA}/grafana`, { recursive: true });
fs.chownSync(`${DATA}/grafana`, 472, 0);

const processes = [
  ["prometheus", "prometheus", [
    "--config.file=/etc/prometheus/prometheus.yml",
    `--storage.tsdb.path=${DATA}/prometheus`,
    `--storage.tsdb.retention.time=${process.env.PROMETHEUS_RETENTION || "30d"}`,
    "--storage.tsdb.retention.size=8GB",
    "--web.enable-otlp-receiver",
    "--web.listen-address=127.0.0.1:9090",
  ]],
  ["collector", "otelcol-contrib", ["--config=/etc/otelcol/config.yaml"]],
  ["grafana", "su-exec", ["grafana", "grafana", "server", "--homepath=/usr/share/grafana", "--config=/etc/grafana/grafana.ini"]],
  ["scaler", "node", ["/opt/rudder-scaler/main.mjs"]],
];

const children = [];
let stopping = false;
function stopAll(code) {
  if (stopping) return;
  stopping = true;
  for (const child of children) {
    try { child.kill("SIGTERM"); } catch { /* gone */ }
  }
  setTimeout(() => process.exit(code), 8000).unref();
}

for (const [name, command, args] of processes) {
  const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"], env: process.env });
  const prefix = (chunk) => chunk.toString().split("\n").filter(Boolean).map((line) => `[${name}] ${line}`).join("\n") + "\n";
  child.stdout.on("data", (chunk) => process.stdout.write(prefix(chunk)));
  child.stderr.on("data", (chunk) => process.stderr.write(prefix(chunk)));
  child.on("exit", (code, signal) => {
    console.error(`[run] ${name} exited (${signal || code}); stopping the machine`);
    stopAll(1);
  });
  children.push(child);
}
for (const signal of ["SIGTERM", "SIGINT"]) process.on(signal, () => stopAll(0));
