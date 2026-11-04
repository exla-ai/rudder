// Home machine metrics (cloud/worker/telemetry.mjs) against a fake /proc and a
// stand-in OpenTelemetry SDK, so no network and no worker node_modules.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  agentStates,
  imageVersion,
  processGroup,
  processMemoryByGroup,
  readHostStats,
  startHomeTelemetry,
} from "../cloud/worker/telemetry.mjs";

function fakeProc() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-telemetry-"));
  const proc = path.join(root, "proc");
  fs.mkdirSync(path.join(proc, "pressure"), { recursive: true });
  fs.writeFileSync(path.join(proc, "meminfo"), "MemTotal:       16380712 kB\nMemFree:          137372 kB\nMemAvailable:     699332 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n");
  fs.writeFileSync(path.join(proc, "pressure", "memory"), "some avg10=91.44 avg60=86.81 avg300=44.04 total=1\nfull avg10=77.92 avg60=74.53 avg300=35.30 total=1\n");
  fs.writeFileSync(path.join(proc, "pressure", "io"), "some avg10=97.17 avg60=93.37 avg300=49.22 total=1\nfull avg10=54.80 avg60=57.22 avg300=28.98 total=1\n");
  fs.writeFileSync(path.join(proc, "loadavg"), "235.44 118.44 57.69 20/1595 14057\n");
  fs.writeFileSync(path.join(proc, "uptime"), "100000.00 1.00\n");
  const add = (pid, comm, cmdline, rssMb) => {
    const dir = path.join(proc, String(pid));
    fs.mkdirSync(dir);
    fs.writeFileSync(path.join(dir, "stat"), `${pid} (${comm}) S 1 ${pid} ${pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 100\n`);
    fs.writeFileSync(path.join(dir, "status"), `Name:\t${comm}\nUid:\t1500\t1500\t1500\t1500\nVmRSS:\t${rssMb * 1024} kB\n`);
    fs.writeFileSync(path.join(dir, "cmdline"), cmdline.split(" ").join("\0"));
  };
  add(10, "node", "node /opt/rudder-worker/home.mjs", 140);
  add(11, "rudder-native", "/opt/rudder/dist/native/rudder-native", 180);
  add(12, "claude", "claude --resume abc", 280);
  add(13, "2.1.314", "/home/rudder/.local/share/claude/versions/2.1.314 --agent-id x --parent-session-id s", 270);
  add(14, "codex", "codex resume", 400);
  add(15, "npm exec offsta", "npm exec offstage-mcp", 90);
  add(16, "rustc", "rustc --crate-name rudder_engine", 1000);
  add(17, "node", "node vitest run", 160);
  add(18, "sleep", "sleep 900", 1);
  return { root, proc };
}

test("host stats read PSI as ratios and keep missing values absent", () => {
  const { proc } = fakeProc();
  const host = readHostStats(proc, [os.tmpdir(), "/definitely/not/mounted"]);
  assert.equal(host.memTotal, 16380712 * 1024);
  assert.equal(host.memAvailable, 699332 * 1024);
  assert.equal(host.swapTotal, 0);
  const memFull60 = host.pressure.find((p) => p.resource === "memory" && p.kind === "full" && p.window === "avg60");
  assert.equal(memFull60.value, 0.7453);
  assert.equal(host.pressure.filter((p) => p.resource === "cpu").length, 0, "no cpu PSI file, no cpu series");
  assert.deepEqual(host.load, { "1m": 235.44, "5m": 118.44, "15m": 57.69 });
  assert.deepEqual(host.disks.map((d) => d.mount), [os.tmpdir()]);
  assert.ok(host.disks[0].total > host.disks[0].free);
});

test("process memory is grouped the way the catalog says", () => {
  const { proc } = fakeProc();
  const groups = {};
  for (const [pid, name] of [[10, "supervisor"], [11, "dashboard"], [12, "claude"], [13, "claude"], [14, "codex"], [15, "mcp"], [16, "build"], [17, "node"], [18, "other"]]) {
    groups[pid] = name;
  }
  const { groups: bytes, count } = processMemoryByGroup(new Map(Object.keys(groups).map((pid) => {
    const dir = path.join(proc, pid);
    const stat = fs.readFileSync(path.join(dir, "stat"), "utf8");
    const comm = stat.slice(stat.indexOf("(") + 1, stat.lastIndexOf(")"));
    const cmdline = fs.readFileSync(path.join(dir, "cmdline"), "utf8").split("\0").join(" ");
    const rssMb = Number(/VmRSS:\s+(\d+)/.exec(fs.readFileSync(path.join(dir, "status"), "utf8"))[1]) / 1024;
    const p = { pid: Number(pid), comm, cmdline, rssMb };
    assert.equal(processGroup(p), groups[pid], `pid ${pid} (${cmdline})`);
    return [Number(pid), p];
  })));
  assert.equal(count, 9);
  assert.equal(bytes.claude, (280 + 270) * 1024 * 1024);
  assert.equal(bytes.build, 1000 * 1024 * 1024);
  assert.equal(bytes.opencode, 0);
});

test("agent rows count by state", () => {
  assert.deepEqual(agentStates([
    { live: true, status: "working" },
    { live: true, status: "waiting" },
    { live: true, status: "done" },
    { live: true, status: "review" },
    { live: false, parked: true, status: "done" },
    { live: false, status: "paused" },
  ]), { working: 1, waiting: 1, idle: 2, asleep: 1, stopped: 1 });
});

test("image version comes from the booted image tag", () => {
  assert.equal(imageVersion({ FLY_IMAGE_REF: "registry.fly.io/rudder-workers-exla:4.0.0-alpha.7@sha256:abc" }), "4.0.0-alpha.7");
  assert.equal(imageVersion({}), "unknown");
});

test("telemetry is off without an endpoint and never loads the SDK", async () => {
  const hooks = await startHomeTelemetry({ env: {}, sdk: { get MeterProvider() { throw new Error("loaded"); } } });
  assert.equal(hooks.enabled, false);
  hooks.recordReconnect();
  hooks.recordCleanupRun({ trigger: "hourly", outcome: "ok" });
});

test("one collection observes every catalog instrument with the right attributes", async () => {
  const { root, proc } = fakeProc();
  const codeDir = path.join(root, "code");
  const runDir = (project, id) => path.join(codeDir, project, ".rudder", "runs", id);
  for (const [project, id, record] of [
    ["libra", "run-a", { status: "running", process: { pid: process.pid } }],
    ["libra", "run-b", { status: "completed", parked: { at: "2026-11-02T10:00:00Z", reason: "idle 3 days" } }],
  ]) {
    fs.mkdirSync(runDir(project, id), { recursive: true });
    fs.writeFileSync(path.join(runDir(project, id), "run.json"), JSON.stringify({ id, createdAt: String(Date.now()), ...record }));
  }
  const calls = { resource: null, exporter: null, reader: null, counters: {}, callback: null };
  const sdk = {
    resourceFromAttributes: (attrs) => { calls.resource = attrs; return attrs; },
    OTLPMetricExporter: class { constructor(options) { calls.exporter = options; } },
    PeriodicExportingMetricReader: class {
      constructor(options) { calls.reader = options; }
      async forceFlush() {}
    },
    MeterProvider: class {
      getMeter() {
        return {
          createObservableGauge: (name, options) => ({ name, ...options }),
          createCounter: (name) => ({ add: (value, attrs) => (calls.counters[name] ??= []).push([value, attrs]) }),
          addBatchObservableCallback: (callback) => { calls.callback = callback; },
        };
      }
      async shutdown() {}
    },
  };
  const hooks = await startHomeTelemetry({
    env: { RUDDER_OTEL_ENDPOINT: "http://collector:4318/", RUDDER_OTEL_TOKEN: "t0k", FLY_IMAGE_REF: "r:4.0.0-alpha.34" },
    homeId: "home-53c2",
    machineId: "8edd225a7d0378",
    region: "iad",
    procRoot: proc,
    paths: { codeDir, home: root, rudderHome: path.join(root, ".rudder") },
    controlConnected: () => true,
    projectCount: () => 3,
    sdk,
  });
  assert.equal(hooks.enabled, true);
  assert.equal(calls.exporter.url, "http://collector:4318/v1/metrics");
  assert.equal(calls.exporter.headers.authorization, "Bearer t0k");
  assert.equal(calls.resource["service.name"], "rudder-home");
  assert.equal(calls.resource["rudder.home.id"], "home-53c2");
  assert.equal(calls.resource["service.version"], "4.0.0-alpha.34");

  const seen = [];
  calls.callback({ observe: (instrument, value, attrs) => seen.push({ name: instrument.name, unit: instrument.unit, value, attrs }) });
  const one = (name, attrs = {}) => seen.filter((s) => s.name === name && Object.entries(attrs).every(([k, v]) => s.attrs?.[k] === v));
  assert.equal(one("rudder.home.memory.available")[0].value, 699332 * 1024);
  assert.equal(one("rudder.home.memory.available")[0].unit, "By");
  assert.equal(one("rudder.home.pressure", { resource: "memory", kind: "full", window: "avg10" })[0].value, 0.7792);
  assert.equal(one("rudder.home.pressure")[0].unit, "1");
  assert.equal(one("rudder.home.process.memory", { group: "build" })[0].value, 1000 * 1024 * 1024);
  assert.equal(one("rudder.home.agents", { state: "working" })[0].value, 1);
  assert.equal(one("rudder.home.agents", { state: "asleep" })[0].value, 1);
  assert.equal(one("rudder.home.projects")[0].value, 3);
  assert.equal(one("rudder.home.control.connected")[0].value, 1);
  assert.deepEqual(one("rudder.home.supervisor.event_loop.delay").map((s) => s.attrs.stat).sort(), ["max", "p50", "p99"]);
  assert.deepEqual(one("rudder.home.load").map((s) => s.attrs.window), ["1m", "5m", "15m"]);

  // Every name observed is in the catalog.
  const catalog = fs.readFileSync(new URL("../observability/metrics.md", import.meta.url), "utf8");
  for (const name of new Set(seen.map((s) => s.name))) {
    assert.ok(catalog.includes(`\`${name}\``), `${name} is in observability/metrics.md`);
  }

  // Every counter series starts at 0 so increase() sees the first real event.
  assert.deepEqual(calls.counters["rudder.home.control.reconnects"], [[0, undefined]]);
  assert.equal(calls.counters["rudder.home.cleanup.runs"].length, 9);
  assert.ok(calls.counters["rudder.home.cleanup.runs"].every(([value]) => value === 0));
  assert.deepEqual(calls.counters["rudder.home.cleanup.actions"], [[0, { action: "park" }], [0, { action: "stop" }]]);
  for (const name of Object.keys(calls.counters)) calls.counters[name] = [];

  hooks.recordReconnect();
  hooks.recordCleanupRun({ trigger: "memory", outcome: "ok", actions: { park: 2, stop: 0 } });
  assert.deepEqual(calls.counters["rudder.home.control.reconnects"], [[1, undefined]]);
  assert.deepEqual(calls.counters["rudder.home.cleanup.runs"], [[1, { trigger: "memory", outcome: "ok" }]]);
  assert.deepEqual(calls.counters["rudder.home.cleanup.actions"], [[2, { action: "park" }]]);
  await hooks.shutdown();
});
