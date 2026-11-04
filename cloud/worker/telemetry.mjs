// Home machine metrics for the observability stack (observability/metrics.md).
// On 2026-11-02 a home ran out of memory and every attached terminal showed
// "reconnecting" before anyone knew; these are the numbers that would have
// shown it coming: memory, swap, pressure stall (PSI), disk, what is holding
// the memory, and what the agents are doing.
//
// Inert unless RUDDER_OTEL_ENDPOINT is set (the collector's OTLP/HTTP base,
// e.g. http://rudder-observability.internal:4318). The OpenTelemetry SDK is
// imported only then, so an image without it still boots. Rudder-specific env
// names on purpose: the standard OTEL_* variables would leak into every agent
// and send their own apps' telemetry (Claude Code's included) to our collector.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { monitorEventLoopDelay } from "node:perf_hooks";

import { isAgentCli, readAgentRows, readProcesses } from "./cleanup.mjs";

const MB = 1024 * 1024;

// ---------------------------------------------------------------------------
// Reading the machine. Pure over a procRoot so tests run on a fake /proc.
// ---------------------------------------------------------------------------

/** Memory, swap, PSI, load and disks. Missing files read as absent, not zero. */
export function readHostStats(procRoot, mounts = ["/workspace", "/"]) {
  const meminfo = readText(path.join(procRoot, "meminfo"));
  const kb = (key) => {
    const match = new RegExp(`^${key}:\\s+(\\d+)`, "m").exec(meminfo);
    return match ? Number(match[1]) * 1024 : null;
  };
  const pressure = [];
  for (const resource of ["memory", "io", "cpu"]) {
    for (const line of readText(path.join(procRoot, "pressure", resource)).split("\n")) {
      const match = /^(some|full) avg10=([\d.]+) avg60=([\d.]+) avg300=([\d.]+)/.exec(line);
      if (!match) continue;
      const [, kind, avg10, avg60, avg300] = match;
      // The kernel reports percent of wall time; the catalog wants 0..1.
      for (const [window, value] of [["avg10", avg10], ["avg60", avg60], ["avg300", avg300]]) {
        pressure.push({ resource, kind, window, value: Number(value) / 100 });
      }
    }
  }
  const load = readText(path.join(procRoot, "loadavg")).split(/\s+/);
  const disks = [];
  for (const mount of mounts) {
    try {
      const stat = fs.statfsSync(mount);
      disks.push({ mount, total: stat.blocks * stat.bsize, free: stat.bavail * stat.bsize });
    } catch {
      // not mounted here (a laptop, a test)
    }
  }
  return {
    memTotal: kb("MemTotal"),
    memAvailable: kb("MemAvailable"),
    swapTotal: kb("SwapTotal"),
    swapFree: kb("SwapFree"),
    pressure,
    load: load.length >= 3 ? { "1m": Number(load[0]), "5m": Number(load[1]), "15m": Number(load[2]) } : null,
    cpus: os.cpus().length,
    disks,
  };
}

const BUILD_COMM = /^(rustc|cargo|cc1|cc1plus|collect2|ld|ld\.lld|lld|gcc|g\+\+|clang|clang\+\+|go|tsc|esbuild|swc|webpack|turbo)$/;
const BUILD_CMD = /\b(next build|vite build|webpack|tsc( |$)|esbuild|cargo (build|test|check))/;

/** Which catalog `group` a process's memory counts toward. */
export function processGroup(proc) {
  if (isAgentCli(proc)) {
    const head = `${proc.comm} ${proc.cmdline.split(" ")[0] ?? ""}`;
    if (/codex/.test(head)) return "codex";
    if (/opencode/.test(head)) return "opencode";
    return "claude";
  }
  if (proc.comm === "rudder-native") return "dashboard";
  if (/\/opt\/rudder-worker\//.test(proc.cmdline)) return "supervisor";
  if (/mcp/i.test(proc.cmdline)) return "mcp";
  if (BUILD_COMM.test(proc.comm) || BUILD_CMD.test(proc.cmdline)) return "build";
  if (proc.comm === "node" || /^node( |$)/.test(proc.cmdline)) return "node";
  return "other";
}

/** RSS bytes per group, plus the process count. */
export function processMemoryByGroup(processes) {
  const groups = Object.fromEntries(["claude", "codex", "opencode", "dashboard", "supervisor", "mcp", "build", "node", "other"].map((group) => [group, 0]));
  for (const proc of processes.values()) {
    groups[processGroup(proc)] += proc.rssMb * MB;
  }
  return { groups, count: processes.size };
}

/** Agent rows (cleanup.mjs readAgentRows) counted by catalog `state`. */
export function agentStates(rows) {
  const states = { working: 0, waiting: 0, idle: 0, asleep: 0, stopped: 0 };
  for (const row of rows) {
    if (row.live) {
      states[row.status === "working" ? "working" : row.status === "waiting" ? "waiting" : "idle"]++;
    } else {
      states[row.parked ? "asleep" : "stopped"]++;
    }
  }
  return states;
}

// ---------------------------------------------------------------------------
// Exporting.
// ---------------------------------------------------------------------------

/**
 * Start pushing home metrics. Returns hooks the supervisor calls on events;
 * every hook is a no-op when telemetry is off, so call sites need no checks.
 * `controlConnected()` reports whether the supervisor's control socket is up.
 */
export async function startHomeTelemetry({
  env = process.env,
  homeId = "",
  machineId = env.FLY_MACHINE_ID || "",
  region = env.FLY_REGION || "",
  procRoot = "/proc",
  paths,
  controlConnected = () => false,
  projectCount = () => 0,
  log = () => {},
  sdk,
} = {}) {
  const off = { enabled: false, recordReconnect() {}, recordCleanupRun() {}, async flush() {}, async shutdown() {} };
  const endpoint = (env.RUDDER_OTEL_ENDPOINT || "").trim().replace(/\/$/, "");
  if (!endpoint) return off;
  let otel;
  try {
    otel = sdk ?? await loadSdk();
  } catch (error) {
    log(`telemetry: off (OpenTelemetry SDK not installed: ${error.message})`);
    return off;
  }
  const intervalMs = Math.max(5_000, Number(env.RUDDER_OTEL_INTERVAL_MS) || 30_000);
  const headers = env.RUDDER_OTEL_TOKEN ? { authorization: `Bearer ${env.RUDDER_OTEL_TOKEN}` } : {};
  const resource = otel.resourceFromAttributes({
    "service.name": "rudder-home",
    "service.instance.id": machineId || os.hostname(),
    "service.version": imageVersion(env),
    "rudder.home.id": homeId,
    "fly.machine.id": machineId,
    "fly.region": region,
    "deployment.environment.name": env.RUDDER_OTEL_ENVIRONMENT || "production",
  });
  const reader = new otel.PeriodicExportingMetricReader({
    exporter: new otel.OTLPMetricExporter({ url: `${endpoint}/v1/metrics`, headers, timeoutMillis: 10_000 }),
    exportIntervalMillis: intervalMs,
    exportTimeoutMillis: 10_000,
  });
  const provider = new otel.MeterProvider({ resource, readers: [reader] });
  const meter = provider.getMeter("rudder-home");

  const gauge = (name, unit, description) => meter.createObservableGauge(name, { unit, description });
  const g = {
    memTotal: gauge("rudder.home.memory.total", "By", "Physical memory"),
    memAvailable: gauge("rudder.home.memory.available", "By", "Memory available without swapping (MemAvailable)"),
    swapTotal: gauge("rudder.home.swap.total", "By", "Swap configured"),
    swapFree: gauge("rudder.home.swap.free", "By", "Swap unused"),
    pressure: gauge("rudder.home.pressure", "1", "Share of time tasks were stalled (Linux PSI)"),
    cpus: gauge("rudder.home.cpu.count", "{cpu}", "CPUs"),
    load: gauge("rudder.home.load", "", "Load average"),
    diskTotal: gauge("rudder.home.disk.total", "By", "Filesystem size"),
    diskFree: gauge("rudder.home.disk.free", "By", "Filesystem space available"),
    processes: gauge("rudder.home.processes", "{process}", "Processes"),
    processMemory: gauge("rudder.home.process.memory", "By", "Resident memory by process group"),
    agents: gauge("rudder.home.agents", "{agent}", "Agent rows by state"),
    projects: gauge("rudder.home.projects", "{project}", "Repos on the home"),
    loopDelay: gauge("rudder.home.supervisor.event_loop.delay", "s", "Supervisor event loop delay"),
    connected: gauge("rudder.home.control.connected", "1", "1 while the control socket to the control plane is open"),
  };
  const reconnects = meter.createCounter("rudder.home.control.reconnects", { unit: "{reconnect}", description: "Control socket reconnects" });
  const cleanupRuns = meter.createCounter("rudder.home.cleanup.runs", { unit: "{run}", description: "Cleanup job runs" });
  const cleanupActions = meter.createCounter("rudder.home.cleanup.actions", { unit: "{action}", description: "Agents parked and processes stopped by the cleanup job" });
  // Start every series at 0: increase() cannot see a counter whose first
  // sample is already 1, so the first cleanup run would otherwise vanish.
  reconnects.add(0);
  for (const trigger of ["hourly", "memory", "boot"]) {
    for (const outcome of ["ok", "failed", "timeout"]) cleanupRuns.add(0, { trigger, outcome });
  }
  for (const action of ["park", "stop"]) cleanupActions.add(0, { action });

  // The supervisor stalling is what users feel as "reconnecting", so measure
  // its event loop directly rather than infer it from machine load.
  const loop = monitorEventLoopDelay({ resolution: 20 });
  loop.enable();

  meter.addBatchObservableCallback((result) => {
    const host = safe(() => readHostStats(procRoot));
    if (host) {
      const observe = (instrument, value, attributes) => {
        if (value !== null && value !== undefined && Number.isFinite(value)) result.observe(instrument, value, attributes);
      };
      observe(g.memTotal, host.memTotal);
      observe(g.memAvailable, host.memAvailable);
      observe(g.swapTotal, host.swapTotal);
      observe(g.swapFree, host.swapFree);
      for (const { resource: res, kind, window, value } of host.pressure) observe(g.pressure, value, { resource: res, kind, window });
      observe(g.cpus, host.cpus);
      for (const [window, value] of Object.entries(host.load ?? {})) observe(g.load, value, { window });
      for (const disk of host.disks) {
        observe(g.diskTotal, disk.total, { mount: disk.mount });
        observe(g.diskFree, disk.free, { mount: disk.mount });
      }
    }
    const memory = safe(() => processMemoryByGroup(readProcesses(procRoot)));
    if (memory) {
      result.observe(g.processes, memory.count);
      for (const [group, bytes] of Object.entries(memory.groups)) result.observe(g.processMemory, bytes, { group });
    }
    const states = paths ? safe(() => agentStates(readAgentRows(paths))) : null;
    if (states) {
      for (const [state, count] of Object.entries(states)) result.observe(g.agents, count, { state });
    }
    result.observe(g.projects, projectCount());
    result.observe(g.connected, controlConnected() ? 1 : 0);
    result.observe(g.loopDelay, loop.percentile(50) / 1e9, { stat: "p50" });
    result.observe(g.loopDelay, loop.percentile(99) / 1e9, { stat: "p99" });
    result.observe(g.loopDelay, loop.max / 1e9, { stat: "max" });
    loop.reset();
  }, Object.values(g));

  log(`telemetry: pushing to ${endpoint} every ${Math.round(intervalMs / 1000)}s`);
  return {
    enabled: true,
    recordReconnect() {
      reconnects.add(1);
    },
    /** One finished cleanup run; `actions` counts what it did ({park, stop}). */
    recordCleanupRun({ trigger, outcome, actions = {} }) {
      cleanupRuns.add(1, { trigger, outcome });
      for (const [action, count] of Object.entries(actions)) {
        if (count > 0) cleanupActions.add(count, { action });
      }
    },
    async flush() {
      await reader.forceFlush().catch(() => undefined);
    },
    async shutdown() {
      loop.disable();
      await provider.shutdown().catch(() => undefined);
    },
  };
}

async function loadSdk() {
  const [metrics, exporter, resources] = await Promise.all([
    import("@opentelemetry/sdk-metrics"),
    import("@opentelemetry/exporter-metrics-otlp-http"),
    import("@opentelemetry/resources"),
  ]);
  return {
    MeterProvider: metrics.MeterProvider,
    PeriodicExportingMetricReader: metrics.PeriodicExportingMetricReader,
    OTLPMetricExporter: exporter.OTLPMetricExporter,
    resourceFromAttributes: resources.resourceFromAttributes,
  };
}

/** The image tag the machine booted (Fly sets FLY_IMAGE_REF), e.g. 4.0.0-alpha.34. */
export function imageVersion(env = process.env) {
  const ref = String(env.FLY_IMAGE_REF || "").split("@")[0];
  const tag = ref.includes(":") ? ref.slice(ref.lastIndexOf(":") + 1) : "";
  return tag || env.RUDDER_VERSION || "unknown";
}

function readText(file) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch {
    return "";
  }
}

function safe(fn) {
  try {
    return fn();
  } catch {
    return null;
  }
}
