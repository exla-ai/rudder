// A synthetic Rudder fleet for local dashboards: several home machines and the
// control plane, pushed as OTLP/JSON to the collector with the exact names in
// observability/metrics.md. It backfills SIM_BACKFILL_HOURS of history, then
// keeps pushing live. Deterministic (seeded) so graphs look the same each run.
//
// The homes, each a shape the scaler and alerts should react to:
//   home-53c2  replay of 2026-11-02: idle agents pile up, memory stalls hit
//              75%, the control socket flaps; swap is added, then the machine
//              grows to 32 GB and the cleanup parks idle agents.
//   home-7a10  healthy.
//   home-b2e4  /workspace nearly full (extend-volume).
//   home-c91f  steady memory pressure, cleanup already ran (scale-up-memory).
//   home-d0aa  32 GB and barely used for a day (scale-down-memory).
//   home-e5f3  stopped reporting 25 minutes ago (investigate-silent).

const COLLECTOR = (process.env.COLLECTOR_URL || "http://localhost:4318").replace(/\/$/, "");
const TOKEN = process.env.RUDDER_OTEL_TOKEN || "";
const BACKFILL_HOURS = Number(process.env.SIM_BACKFILL_HOURS ?? 5);
const STEP_MS = 30_000;
const LIVE_MS = 15_000;
const GB = 1024 ** 3;
const HOUR = 3600_000;

// ---------------------------------------------------------------------------
// Deterministic noise.
// ---------------------------------------------------------------------------

function hash(text) {
  let h = 2166136261;
  for (const c of text) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return h >>> 0;
}
/** Smooth-ish noise in [-1, 1] for a key at time t (stable across runs). */
function noise(key, t) {
  const k = hash(key);
  const x = t / (7 * 60_000);
  return Math.sin(x + (k % 97)) * 0.6 + Math.sin(x * 3.7 + (k % 13)) * 0.3 + Math.sin(x * 11.3 + (k % 7)) * 0.1;
}
const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
const lerp = (a, b, f) => a + (b - a) * clamp(f, 0, 1);

// ---------------------------------------------------------------------------
// Home profiles: h is hours relative to now (negative = past).
// ---------------------------------------------------------------------------

const HOMES = [
  { id: "home-53c2", machine: "8edd225a7d03", region: "iad", version: "4.0.0-alpha.7", profile: incident },
  { id: "home-7a10", machine: "1781e2a4b5c6", region: "sjc", version: "4.0.0-alpha.33", profile: healthy },
  { id: "home-b2e4", machine: "2c9f00d1e2f3", region: "iad", version: "4.0.0-alpha.33", profile: diskFull },
  { id: "home-c91f", machine: "3a7c11223344", region: "ord", version: "4.0.0-alpha.33", profile: pressured },
  { id: "home-d0aa", machine: "4d0e55667788", region: "sjc", version: "4.0.0-alpha.33", profile: oversized },
  { id: "home-e5f3", machine: "5e5f99aabbcc", region: "fra", version: "4.0.0-alpha.33", profile: healthy, silentAfterH: -25 / 60 },
];

function base(t, key) {
  return {
    memTotal: 16 * GB, swapTotal: 0, swapUsed: 0, cpus: 4,
    diskTotal: 100 * GB, diskUsed: 40 * GB, rootTotal: 8 * GB, rootUsed: 5 * GB,
    stall: clamp(0.002 + 0.002 * noise(key + "stall", t), 0, 1), io: clamp(0.01 + 0.01 * noise(key + "io", t), 0, 1), cpu: clamp(0.08 + 0.05 * noise(key + "cpu", t), 0, 1),
    groups: { claude: 2.4 * GB, codex: 0.3 * GB, opencode: 0, dashboard: 0.5 * GB, supervisor: 0.15 * GB, mcp: 0.2 * GB, build: 0.4 * GB, node: 0.6 * GB, other: 0.5 * GB },
    agents: { working: 2, waiting: 0, idle: 4, asleep: 0, stopped: 12 },
    projects: 6, loopP99: 0.012, connected: 1,
  };
}

function healthy(h, t, key) {
  const s = base(t, key);
  s.groups.build = Math.max(0, (1.2 + 1.2 * noise(key + "build", t)) * GB);
  s.agents.working = Math.round(clamp(2 + 2 * noise(key + "work", t), 0, 5));
  return s;
}

function incident(h, t, key) {
  const s = base(t, key);
  s.projects = 23;
  // Idle Claude sessions resumed at boot keep accumulating until the swap
  // fix (-2h), the resize to 32 GB (-1h) and the first cleanup (-0.75h).
  const pile = h < -2 ? lerp(5.2, 7.4, (h + BACKFILL_HOURS) / (BACKFILL_HOURS - 2)) : h < -0.75 ? 7.4 : lerp(7.4, 3.1, (h + 0.75) / 0.5);
  s.groups.claude = pile * GB;
  s.groups.node = 2.9 * GB;
  s.groups.codex = 1.3 * GB;
  s.groups.opencode = 0.7 * GB;
  s.groups.dashboard = 0.7 * GB;
  s.groups.mcp = 0.5 * GB;
  s.groups.build = Math.max(0, (1.0 + 0.8 * noise(key + "build", t)) * GB);
  s.agents = h < -0.75
    ? { working: 5, waiting: 2, idle: 34, asleep: 0, stopped: 12 }
    : { working: 5, waiting: 2, idle: 6, asleep: 28, stopped: 12 };
  if (h >= -1) {
    s.memTotal = 32 * GB;
  }
  if (h >= -2) {
    s.swapTotal = 4 * GB;
    s.swapUsed = h < -1 ? 2.6 * GB : 0.9 * GB;
  }
  const used = Object.values(s.groups).reduce((a, b) => a + b, 0) + 1.2 * GB; // + kernel and caches it cannot drop
  s.memAvailable = Math.max(0.5 * GB, s.memTotal - used + s.swapUsed);
  if (h < -2) {
    const severity = clamp((h + BACKFILL_HOURS) / (BACKFILL_HOURS - 2.5), 0, 1);
    s.stall = clamp(lerp(0.03, 0.75, severity) + 0.06 * noise(key + "stall", t), 0, 0.95);
    s.io = clamp(lerp(0.08, 0.93, severity) + 0.04 * noise(key + "io", t), 0, 0.99);
    s.loopP99 = lerp(0.02, 4.5, severity);
    s.load1 = lerp(12, 235, severity);
    s.connected = severity > 0.55 && noise(key + "flap", t) > 0.2 ? 0 : 1;
  } else if (h < -1) {
    s.stall = clamp(0.06 + 0.04 * noise(key + "stall", t), 0, 1);
    s.io = clamp(0.12 + 0.05 * noise(key + "io", t), 0, 1);
    s.loopP99 = 0.08;
    s.load1 = 60;
  } else {
    s.loopP99 = 0.015;
    s.load1 = 9;
  }
  s.diskUsed = 91 * GB;
  return s;
}

function diskFull(h, t, key) {
  const s = healthy(h, t, key);
  s.diskUsed = lerp(88, 94.5, (h + BACKFILL_HOURS) / BACKFILL_HOURS) * GB;
  return s;
}

function pressured(h, t, key) {
  const s = healthy(h, t, key);
  s.swapTotal = 4 * GB;
  s.swapUsed = 3.1 * GB;
  s.groups.claude = 7.8 * GB;
  s.groups.build = 3.2 * GB;
  s.agents = { working: 6, waiting: 1, idle: 3, asleep: 9, stopped: 20 };
  s.stall = clamp(0.16 + 0.05 * noise(key + "stall", t), 0, 1);
  s.io = clamp(0.22 + 0.06 * noise(key + "io", t), 0, 1);
  s.loopP99 = 0.18;
  return s;
}

function oversized(h, t, key) {
  const s = healthy(h, t, key);
  s.memTotal = 32 * GB;
  s.groups.claude = 1.1 * GB;
  s.groups.build = 0.2 * GB;
  s.agents = { working: 0, waiting: 0, idle: 1, asleep: 2, stopped: 8 };
  return s;
}

/** Fill derived numbers every profile shares. */
function finish(s, t, key) {
  const used = Object.values(s.groups).reduce((a, b) => a + b, 0) + 1.2 * GB;
  s.memAvailable ??= clamp(s.memTotal - used, 0.3 * GB, s.memTotal);
  s.load1 ??= clamp(s.agents.working * 1.4 + 0.8 + noise(key + "load", t), 0.1, 64);
  s.processes = 120 + (s.agents.working + s.agents.idle + s.agents.waiting) * 6;
  s.loopP50 = s.loopP99 / 6;
  s.loopMax = s.loopP99 * 2.5;
  return s;
}

// ---------------------------------------------------------------------------
// Cumulative counters (OTLP sums are cumulative; Prometheus rates them).
// ---------------------------------------------------------------------------

const counters = new Map();
function bump(key, by) {
  counters.set(key, (counters.get(key) ?? 0) + by);
  return counters.get(key);
}

// ---------------------------------------------------------------------------
// OTLP/JSON encoding.
// ---------------------------------------------------------------------------

const attrs = (object) => Object.entries(object).map(([key, value]) => (
  typeof value === "number" ? { key, value: { intValue: String(value) } } : { key, value: { stringValue: String(value) } }
));
const nanos = (ms) => `${BigInt(Math.round(ms)) * 1_000_000n}`;
const START_MS = Date.now() - BACKFILL_HOURS * HOUR - 60_000;

class Batch {
  constructor() { this.metrics = new Map(); }
  #metric(name, unit, kind) {
    if (!this.metrics.has(name)) {
      const metric = { name, unit };
      metric[kind] = kind === "gauge" ? { dataPoints: [] } : { aggregationTemporality: 2, isMonotonic: true, dataPoints: [] };
      if (kind === "histogram") delete metric[kind].isMonotonic;
      this.metrics.set(name, metric);
    }
    return this.metrics.get(name)[kind].dataPoints;
  }
  gauge(name, unit, t, value, attributes = {}) {
    if (!Number.isFinite(value)) return;
    this.#metric(name, unit, "gauge").push({ attributes: attrs(attributes), timeUnixNano: nanos(t), asDouble: value });
  }
  sum(name, unit, t, value, attributes = {}) {
    this.#metric(name, unit, "sum").push({ attributes: attrs(attributes), startTimeUnixNano: nanos(START_MS), timeUnixNano: nanos(t), asDouble: value });
  }
  histogram(name, unit, t, point, attributes = {}) {
    this.#metric(name, unit, "histogram").push({ attributes: attrs(attributes), startTimeUnixNano: nanos(START_MS), timeUnixNano: nanos(t), ...point });
  }
  toResource(resource) {
    return { resource: { attributes: attrs(resource) }, scopeMetrics: [{ scope: { name: "rudder-fleet-sim" }, metrics: [...this.metrics.values()] }] };
  }
}

function homeBatch(home, times, batch = new Batch()) {
  for (const t of times) {
    const h = (t - Date.now()) / HOUR;
    if (home.silentAfterH !== undefined && h > home.silentAfterH) continue;
    const key = home.id;
    const s = finish(home.profile(h, t, key), t, key);
    batch.gauge("rudder.home.memory.total", "By", t, s.memTotal);
    batch.gauge("rudder.home.memory.available", "By", t, s.memAvailable);
    batch.gauge("rudder.home.swap.total", "By", t, s.swapTotal);
    batch.gauge("rudder.home.swap.free", "By", t, s.swapTotal - s.swapUsed);
    for (const [resource, avg60] of [["memory", s.stall], ["io", s.io], ["cpu", s.cpu]]) {
      for (const kind of resource === "cpu" ? ["some"] : ["some", "full"]) {
        const k = kind === "some" ? Math.min(0.99, avg60 * 1.25 + 0.002) : avg60;
        batch.gauge("rudder.home.pressure", "1", t, clamp(k * (1 + 0.15 * noise(`${key}${resource}${kind}10`, t)), 0, 1), { resource, kind, window: "avg10" });
        batch.gauge("rudder.home.pressure", "1", t, k, { resource, kind, window: "avg60" });
        batch.gauge("rudder.home.pressure", "1", t, k * 0.8, { resource, kind, window: "avg300" });
      }
    }
    batch.gauge("rudder.home.cpu.count", "{cpu}", t, s.cpus);
    batch.gauge("rudder.home.load", "", t, s.load1, { window: "1m" });
    batch.gauge("rudder.home.load", "", t, s.load1 * 0.8, { window: "5m" });
    batch.gauge("rudder.home.load", "", t, s.load1 * 0.6, { window: "15m" });
    batch.gauge("rudder.home.disk.total", "By", t, s.diskTotal, { mount: "/workspace" });
    batch.gauge("rudder.home.disk.free", "By", t, s.diskTotal - s.diskUsed, { mount: "/workspace" });
    batch.gauge("rudder.home.disk.total", "By", t, s.rootTotal, { mount: "/" });
    batch.gauge("rudder.home.disk.free", "By", t, s.rootTotal - s.rootUsed, { mount: "/" });
    batch.gauge("rudder.home.processes", "{process}", t, s.processes);
    for (const [group, bytes] of Object.entries(s.groups)) batch.gauge("rudder.home.process.memory", "By", t, bytes, { group });
    for (const [state, count] of Object.entries(s.agents)) batch.gauge("rudder.home.agents", "{agent}", t, count, { state });
    batch.gauge("rudder.home.projects", "{project}", t, s.projects);
    batch.gauge("rudder.home.supervisor.event_loop.delay", "s", t, s.loopP50, { stat: "p50" });
    batch.gauge("rudder.home.supervisor.event_loop.delay", "s", t, s.loopP99, { stat: "p99" });
    batch.gauge("rudder.home.supervisor.event_loop.delay", "s", t, s.loopMax, { stat: "max" });
    batch.gauge("rudder.home.control.connected", "1", t, s.connected);
    // Counters: a reconnect whenever the socket comes back; hourly cleanups,
    // plus early "memory" runs when stalls are high.
    const wasDown = home.lastConnected === 0 && s.connected === 1;
    home.lastConnected = s.connected;
    batch.sum("rudder.home.control.reconnects", "{reconnect}", t, bump(`${key}:reconnects`, wasDown ? 1 : 0));
    // Compare with the previous sample, not t - STEP_MS: the gap between the
    // backfill and the first live push must not swallow a boundary.
    const previous = home.lastT ?? t - STEP_MS;
    home.lastT = t;
    const hourTick = Math.floor(t / HOUR) !== Math.floor(previous / HOUR);
    const memoryTick = s.stall >= 0.2 && Math.floor(t / (15 * 60_000)) !== Math.floor(previous / (15 * 60_000));
    const parked = key === "home-53c2" && previous < Date.now() - 0.75 * HOUR && t >= Date.now() - 0.75 * HOUR ? 28 : hourTick ? 1 : 0;
    batch.sum("rudder.home.cleanup.runs", "{run}", t, bump(`${key}:runs:hourly:ok`, hourTick ? 1 : 0), { trigger: "hourly", outcome: "ok" });
    batch.sum("rudder.home.cleanup.runs", "{run}", t, bump(`${key}:runs:memory:ok`, memoryTick ? 1 : 0), { trigger: "memory", outcome: "ok" });
    batch.sum("rudder.home.cleanup.actions", "{action}", t, bump(`${key}:park`, parked), { action: "park" });
    batch.sum("rudder.home.cleanup.actions", "{action}", t, bump(`${key}:stop`, hourTick && key !== "home-d0aa" ? 1 : 0), { action: "stop" });
  }
  return batch;
}

const ROUTES = [
  ["GET", "/api/app/state", 6, 0.04],
  ["POST", "/api/rudder/workspace/:id/heartbeat", 3, 0.008],
  ["GET", "/api/app/transcript", 1.5, 0.09],
  ["POST", "/api/app/task", 0.1, 0.35],
  ["GET", "/app/*", 0.8, 0.004],
  ["GET", "/health", 0.033, 0.002],
  ["GET", "/api/rudder/home/:id/control", 0.01, 0.003],
];
const BOUNDS = [0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1, 2.5, 5, 7.5, 10, 30];

function controlBatch(times, batch = new Batch()) {
  for (const t of times) {
    const h = (t - Date.now()) / HOUR;
    const incidentLoad = h < -2 ? clamp((h + BACKFILL_HOURS) / (BACKFILL_HOURS - 2.5), 0, 1) : 0;
    for (const [method, route, rps, median] of ROUTES) {
      const key = `${method} ${route}`;
      const n = Math.max(0, Math.round(rps * (STEP_MS / 1000) * (1 + 0.3 * noise(key, t))));
      const slow = route.includes("state") || route.includes("transcript") ? 1 + incidentLoad * 4 : 1;
      const buckets = new Array(BOUNDS.length + 1).fill(0);
      let total = 0;
      for (let i = 0; i < n; i++) {
        const u = (hash(`${key}${t}${i}`) % 10_000) / 10_000;
        const seconds = median * slow * Math.exp((u - 0.5) * 2.2);
        total += seconds;
        const index = BOUNDS.findIndex((bound) => seconds <= bound);
        buckets[index === -1 ? BOUNDS.length : index]++;
      }
      const errors = route === "/api/app/state" && incidentLoad > 0.6 ? Math.round(n * 0.06) : 0;
      for (const [status, count, share] of [[200, n - errors, 1 - errors / Math.max(1, n)], [502, errors, errors / Math.max(1, n)]]) {
        if (status === 502 && !counters.has(`${key}:502:count`) && errors === 0) continue;
        const cumKey = `${key}:${status}`;
        const prev = counters.get(cumKey) ?? { count: 0, sum: 0, buckets: new Array(BOUNDS.length + 1).fill(0) };
        prev.count += count;
        prev.sum += total * share;
        prev.buckets = prev.buckets.map((b, i) => b + Math.round(buckets[i] * share));
        counters.set(cumKey, prev);
        counters.set(`${key}:${status}:count`, prev.count);
        batch.histogram("http.server.request.duration", "s", t, {
          count: String(prev.buckets.reduce((a, b) => a + b, 0)),
          sum: prev.sum,
          bucketCounts: prev.buckets.map(String),
          explicitBounds: BOUNDS,
        }, { "http.request.method": method, "http.route": route, "http.response.status_code": status });
      }
    }
    batch.gauge("rudder.control.homes", "{home}", t, 5, { state: "running" });
    batch.gauge("rudder.control.homes", "{home}", t, 3, { state: "stopped" });
    batch.gauge("rudder.control.sails", "{sail}", t, 41, { state: "completed" });
    batch.gauge("rudder.control.sails", "{sail}", t, 2, { state: "running" });
    let clients = 0;
    for (const home of HOMES) {
      const silent = home.silentAfterH !== undefined && h > home.silentAfterH;
      const flapping = home.id === "home-53c2" && incidentLoad > 0.55 && noise(home.id + "flap", t) > 0.2;
      const attrsHome = { "rudder.home.id": home.id };
      batch.gauge("rudder.control.home.heartbeat.age", "s", t, silent ? (h - home.silentAfterH) * 3600 : flapping ? 45 + 30 * Math.abs(noise(home.id, t)) : 5 + 20 * Math.abs(noise(home.id + "hb", t)), attrsHome);
      batch.gauge("rudder.control.home.control_connected", "1", t, silent || flapping ? 0 : 1, attrsHome);
      // home-e5f3 is meant to be running, so its silence is worth a look.
      batch.gauge("rudder.control.home.state", "", t, 1, { ...attrsHome, state: "running" });
      const attached = home.id === "home-53c2" ? 3 : home.id === "home-d0aa" || silent ? 0 : 1;
      clients += attached;
      batch.gauge("rudder.control.home.clients", "{client}", t, attached, attrsHome);
      const dropped = flapping && !home.wasFlapping;
      home.wasFlapping = flapping;
      batch.sum("rudder.control.home.disconnects", "{disconnect}", t, bump(`${home.id}:disconnects`, dropped || (silent && !home.wentSilent) ? 1 : 0), attrsHome);
      if (silent) home.wentSilent = true;
    }
    batch.gauge("rudder.control.ws.connections", "{connection}", t, 5, { role: "home-control" });
    batch.gauge("rudder.control.ws.connections", "{connection}", t, 2, { role: "worker" });
    batch.gauge("rudder.control.ws.connections", "{connection}", t, clients, { role: "client" });
    const loop = 0.004 + incidentLoad * 0.05 + 0.002 * Math.abs(noise("loop", t));
    batch.gauge("rudder.control.event_loop.delay", "s", t, loop / 3, { stat: "p50" });
    batch.gauge("rudder.control.event_loop.delay", "s", t, loop, { stat: "p99" });
    batch.gauge("rudder.control.event_loop.delay", "s", t, loop * 3, { stat: "max" });
    batch.gauge("rudder.control.process.memory", "By", t, (310 + 40 * noise("rss", t)) * 1024 * 1024, { kind: "rss" });
    batch.gauge("rudder.control.process.memory", "By", t, (120 + 25 * noise("heap", t)) * 1024 * 1024, { kind: "heap" });
  }
  return batch;
}

// ---------------------------------------------------------------------------
// Pushing.
// ---------------------------------------------------------------------------

async function push(resourceMetrics) {
  const response = await fetch(`${COLLECTOR}/v1/metrics`, {
    method: "POST",
    headers: { "content-type": "application/json", ...(TOKEN ? { authorization: `Bearer ${TOKEN}` } : {}) },
    body: JSON.stringify({ resourceMetrics }),
  });
  if (!response.ok) throw new Error(`collector said ${response.status}: ${(await response.text()).slice(0, 300)}`);
}

function resources(times) {
  const out = HOMES.map((home) => homeBatch(home, times).toResource({
    "service.name": "rudder-home",
    "service.instance.id": home.machine,
    "service.version": home.version,
    "rudder.home.id": home.id,
    "fly.machine.id": home.machine,
    "fly.region": home.region,
    "deployment.environment.name": "sim",
  }));
  out.push(controlBatch(times).toResource({
    "service.name": "rudder-control",
    "service.instance.id": "sim-control",
    "service.version": "sim",
    "fly.machine.id": "sim-control",
    "fly.region": "sjc",
    "deployment.environment.name": "sim",
  }));
  return out;
}

async function waitForCollector() {
  for (let i = 0; i < 60; i++) {
    try {
      await push([]);
      return;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 2000));
    }
  }
  throw new Error(`no collector at ${COLLECTOR}`);
}

await waitForCollector();
const now = Date.now();
const steps = [];
for (let t = now - BACKFILL_HOURS * HOUR; t < now; t += STEP_MS) steps.push(t);
for (let i = 0; i < steps.length; i += 20) {
  await push(resources(steps.slice(i, i + 20)));
}
console.log(`fleet-sim: backfilled ${steps.length} steps (${BACKFILL_HOURS}h) for ${HOMES.length} homes and the control plane`);
setInterval(() => {
  push(resources([Date.now()])).catch((error) => console.error(`fleet-sim: ${error.message}`));
}, LIVE_MS);
