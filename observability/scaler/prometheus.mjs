// Reading the homes out of Prometheus: a tiny client for its HTTP API and the
// queries that turn the metric catalog (observability/metrics.md) into one
// row per home for policy.mjs.
//
// Home series are grouped by (rudder_home_id, fly_machine_id): when a home
// moves to a new machine, the old machine's series linger for minutes (and in
// 24 h windows for a day), and mixing them would judge the new machine on the
// old one's memory. The row uses the machine that reported most recently.

const HOME = "rudder_home_id, fly_machine_id";

/** A client for Prometheus' instant query API. `fetch` is injectable for tests. */
export function createPrometheusClient({ url = "http://localhost:9090", fetch: fetchImpl = globalThis.fetch, timeoutMs = 10_000 } = {}) {
  const base = url.endsWith("/") ? url : `${url}/`;
  async function query(promql, { time } = {}) {
    const target = new URL("api/v1/query", base);
    target.searchParams.set("query", promql);
    if (time !== undefined) target.searchParams.set("time", String(time));
    let response;
    try {
      response = await fetchImpl(target.toString(), {
        headers: { Accept: "application/json" },
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (error) {
      const why = error?.name === "TimeoutError" ? `no answer in ${timeoutMs} ms` : (error?.cause?.code || error?.cause?.message || error?.message || String(error));
      throw new Error(`Prometheus at ${url} is unreachable: ${why}`);
    }
    const text = await response.text();
    let body;
    try {
      body = JSON.parse(text);
    } catch {
      throw new Error(`Prometheus answered HTTP ${response.status} without JSON`);
    }
    if (body?.status !== "success") {
      throw new Error(`Prometheus query failed (${body?.errorType || `HTTP ${response.status}`}): ${body?.error || "no detail"}`);
    }
    if (body.data?.resultType !== "vector" || !Array.isArray(body.data.result)) {
      throw new Error(`Prometheus returned ${body.data?.resultType ?? "nothing"} where an instant vector was expected`);
    }
    return body.data.result;
  }
  return { query };
}

/**
 * The PromQL behind each home field. `selector` adds label matchers to every
 * series, e.g. `deployment_environment_name="production"` to keep sims out.
 */
export function homeQueries(selector = "") {
  const m = (name, matchers = "") => {
    const all = [matchers, selector.trim()].filter(Boolean).join(",");
    return all ? `${name}{${all}}` : name;
  };
  const total = m("rudder_home_memory_total_bytes");
  const available = m("rudder_home_memory_available_bytes");
  const fullAvg60 = m("rudder_home_pressure_ratio", 'resource="memory",kind="full",window="avg60"');
  const agents = m("rudder_home_agents");
  const cleanups = m("rudder_home_cleanup_runs_total");
  return {
    // OTLP-ingested series get no staleness markers, so a plain instant query
    // forgets a home five minutes after it goes quiet. timestamp() inside a
    // subquery keeps the real sample times for the last hour.
    lastSeenSeconds: `time() - max by (${HOME}, fly_region) (max_over_time(timestamp(${total})[1h:30s]))`,
    // How much history the 24 h windows actually cover.
    memoryHistorySeconds: `time() - min by (${HOME}) (min_over_time(timestamp(${total})[24h:5m]))`,
    memoryTotalBytes: `max by (${HOME}) (${total})`,
    memoryAvailableBytes: `min by (${HOME}) (${available})`,
    swapTotalBytes: `max by (${HOME}) (${m("rudder_home_swap_total_bytes")})`,
    swapFreeBytes: `min by (${HOME}) (${m("rudder_home_swap_free_bytes")})`,
    memoryFullAvg60: `max by (${HOME}) (${fullAvg60})`,
    memoryFullAvg60Over30m: `max by (${HOME}) (avg_over_time(${fullAvg60}[30m]))`,
    availableRatioOver30mAvg: `min by (${HOME}) (avg_over_time(${available}[30m])) / max by (${HOME}) (avg_over_time(${total}[30m]))`,
    usedRatioMax24h: `1 - min by (${HOME}) (min_over_time(${available}[24h])) / max by (${HOME}) (max_over_time(${total}[24h]))`,
    memoryFullMax24h: `max by (${HOME}) (max_over_time(${fullAvg60}[24h]))`,
    diskWorkspaceTotalBytes: `max by (${HOME}) (${m("rudder_home_disk_total_bytes", 'mount="/workspace"')})`,
    diskWorkspaceFreeBytes: `min by (${HOME}) (${m("rudder_home_disk_free_bytes", 'mount="/workspace"')})`,
    agents: `max by (${HOME}, state) (${agents})`,
    // A home that reports agents but none working or waiting counts as 0, not
    // as missing, even if the emitter leaves out states with no agents.
    busyMax15m: `max_over_time((sum by (${HOME}) (${m("rudder_home_agents", 'state=~"working|waiting"')}) or 0 * sum by (${HOME}) (${agents}))[15m:30s])`,
    cleanupRunsLast1h: `sum by (${HOME}) (increase(${cleanups}[1h]))`,
    cleanupRunsLast15m: `sum by (${HOME}) (increase(${cleanups}[15m]))`,
    clients: `max by (rudder_home_id) (${m("rudder_control_home_clients")})`,
    controlConnected: `max by (rudder_home_id) (${m("rudder_control_home_control_connected_ratio")})`,
    // The home's control-plane state: each state series at 1, valued by its
    // newest sample time. After a change the old state's series can linger
    // at 1 for the five minute lookback, and the newest one is current.
    controlState: `max by (rudder_home_id, state) (timestamp(${m("rudder_control_home_state")}) and ${m("rudder_control_home_state")} == 1)`,
  };
}

// Fields that come straight from one query's value, per home machine.
const MACHINE_FIELDS = [
  "lastSeenSeconds",
  "memoryHistorySeconds",
  "memoryTotalBytes",
  "memoryAvailableBytes",
  "swapTotalBytes",
  "swapFreeBytes",
  "memoryFullAvg60",
  "memoryFullAvg60Over30m",
  "availableRatioOver30mAvg",
  "usedRatioMax24h",
  "memoryFullMax24h",
  "diskWorkspaceTotalBytes",
  "diskWorkspaceFreeBytes",
  "busyMax15m",
  "cleanupRunsLast1h",
  "cleanupRunsLast15m",
];
// From the control plane, which knows homes but not machines.
const HOME_FIELDS = ["clients", "controlConnected"];
const AGENT_STATES = { working: "agentsWorking", waiting: "agentsWaiting", idle: "agentsIdle", asleep: "agentsAsleep" };

export function blankHome(homeId, machineId = null) {
  return {
    homeId,
    machineId,
    region: null,
    memoryTotalBytes: null,
    memoryAvailableBytes: null,
    swapTotalBytes: null,
    swapFreeBytes: null,
    memoryFullAvg60: null,
    memoryFullAvg60Over30m: null,
    availableRatioOver30mAvg: null,
    usedRatioMax24h: null,
    memoryFullMax24h: null,
    memoryHistorySeconds: null,
    diskWorkspaceTotalBytes: null,
    diskWorkspaceFreeBytes: null,
    agentsWorking: null,
    agentsWaiting: null,
    agentsIdle: null,
    agentsAsleep: null,
    busyMax15m: null,
    cleanupRunsLast1h: null,
    cleanupRunsLast15m: null,
    lastSeenSeconds: null,
    clients: null,
    controlConnected: null,
    controlState: null,
  };
}

/**
 * Run every home query at one evaluation time and fold the results into one
 * row per home. Any failed query fails the whole collection: a missing
 * reading can loosen a rule (no cleanup metrics allows a resize), so a
 * partial picture is not safe to act on.
 */
export async function collectHomes(client, { time = Date.now() / 1000, selector = "", concurrency = 6 } = {}) {
  const entries = Object.entries(homeQueries(selector));
  const at = Number(time).toFixed(3);
  const results = Object.fromEntries(await mapLimit(entries, concurrency, async ([key, promql]) => [key, await client.query(promql, { time: at })]));

  const machines = new Map();
  const row = (metric) => {
    const homeId = metric?.rudder_home_id;
    if (!homeId) return null;
    const machineId = metric.fly_machine_id || null;
    const key = `${homeId}\u0000${machineId ?? ""}`;
    if (!machines.has(key)) machines.set(key, { ...blankHome(homeId, machineId), agentsSeen: false });
    return machines.get(key);
  };

  for (const field of MACHINE_FIELDS) {
    for (const sample of results[field] ?? []) {
      const home = row(sample.metric);
      if (!home) continue;
      home[field] = number(sample.value?.[1]);
      if (field === "lastSeenSeconds" && sample.metric.fly_region) home.region = sample.metric.fly_region;
    }
  }
  for (const sample of results.agents ?? []) {
    const home = row(sample.metric);
    const field = AGENT_STATES[sample.metric?.state];
    if (!home) continue;
    home.agentsSeen = true;
    if (field) home[field] = number(sample.value?.[1]);
  }

  // One row per home: the machine that reported last.
  const homes = new Map();
  for (const home of machines.values()) {
    if (home.agentsSeen) {
      for (const field of Object.values(AGENT_STATES)) home[field] ??= 0;
    }
    delete home.agentsSeen;
    for (const field of ["cleanupRunsLast1h", "cleanupRunsLast15m"]) {
      // increase() extrapolates to the window edges; runs are whole numbers.
      if (home[field] !== null) home[field] = Math.round(home[field]);
    }
    if (home.lastSeenSeconds !== null) home.lastSeenSeconds = Math.max(0, home.lastSeenSeconds);
    const current = homes.get(home.homeId);
    if (!current || newer(home, current)) homes.set(home.homeId, home);
  }
  for (const field of HOME_FIELDS) {
    for (const sample of results[field] ?? []) {
      const home = homes.get(sample.metric?.rudder_home_id);
      if (home) home[field] = number(sample.value?.[1]);
    }
  }
  const stateSeenAt = new Map();
  for (const sample of results.controlState ?? []) {
    const home = homes.get(sample.metric?.rudder_home_id);
    const at = number(sample.value?.[1]);
    if (!home || !sample.metric.state || at === null) continue;
    const seen = stateSeenAt.get(home.homeId);
    // On a tie, a state that expects the home to run wins: when in doubt the
    // scaler keeps reporting a silent home.
    if (seen === undefined || at > seen || (at === seen && ["running", "queued"].includes(sample.metric.state))) {
      stateSeenAt.set(home.homeId, at);
      home.controlState = sample.metric.state;
    }
  }
  return [...homes.values()].sort((a, b) => a.homeId.localeCompare(b.homeId));
}

function newer(candidate, current) {
  if (candidate.lastSeenSeconds === null) return false;
  if (current.lastSeenSeconds === null) return true;
  return candidate.lastSeenSeconds < current.lastSeenSeconds;
}

// Prometheus sends values as strings, including "NaN" and "+Inf".
function number(raw) {
  const value = Number(raw);
  return Number.isFinite(value) ? value : null;
}

async function mapLimit(items, limit, fn) {
  const out = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const index = next++;
      out[index] = await fn(items[index]);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, worker));
  return out;
}
