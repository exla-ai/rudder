#!/usr/bin/env node
// The scaler: once a minute it reads every home machine out of Prometheus,
// decides what each needs (policy.mjs), and, only when told to, carries out
// the memory resizes and volume extends on Fly (fly.mjs). Everything else it
// decides is a recommendation for a person or a later control-plane call.
//
// It is a dry run unless RUDDER_SCALER_APPLY=1 and FLY_API_TOKEN are both set.
// Its decisions are on GET /metrics (for Prometheus and Grafana), GET
// /decisions (JSON, with the reasons) and in its log, one line per change.
//
//   node observability/scaler/main.mjs          run the loop and the server
//   node observability/scaler/main.mjs --once   one evaluation, JSON to stdout
//
// Environment:
//   PROMETHEUS_URL              default http://localhost:9090
//   RUDDER_SCALER_INTERVAL_MS   default 60000
//   RUDDER_SCALER_PORT          default 9464 (RUDDER_SCALER_HOST to bind one address)
//   RUDDER_SCALER_STATE         default observability/scaler/.state.json
//   RUDDER_SCALER_SELECTOR      extra label matchers, e.g. deployment_environment_name="production"
//   RUDDER_SCALER_APPLY=1       change machines (also needs FLY_API_TOKEN)
//   RUDDER_FLY_APP              default rudder-workers-exla
//   RUDDER_SCALER_<THRESHOLD>   any DEFAULT_CONFIG key in upper snake case,
//                               e.g. RUDDER_SCALER_SHORT_AVAILABLE_RATIO=0.15,
//                               RUDDER_SCALER_MEMORY_STEPS_MB=16384,24576,32768

import fs from "node:fs";
import http from "node:http";
import { fileURLToPath } from "node:url";

import { DEFAULT_CONFIG, EXECUTABLE_ACTIONS, MiB, RESIZE_ACTIONS, evaluate } from "./policy.mjs";
import { collectHomes, createPrometheusClient } from "./prometheus.mjs";
import { createFlyExecutor } from "./fly.mjs";
import { loadState, recordEvaluation, recordOutcome, saveState, statePath } from "./state.mjs";

const SCRIPT_PATH = fileURLToPath(import.meta.url);

/** `shortAvailableRatio` -> `RUDDER_SCALER_SHORT_AVAILABLE_RATIO`. */
export function configEnvName(key) {
  return `RUDDER_SCALER_${key.replace(/[A-Z]/g, (letter) => `_${letter}`).toUpperCase()}`;
}

/** DEFAULT_CONFIG with any RUDDER_SCALER_* overrides; a bad value stops startup. */
export function configFromEnv(env = process.env, base = DEFAULT_CONFIG) {
  const config = { ...base };
  for (const key of Object.keys(base)) {
    const name = configEnvName(key);
    const raw = env[name];
    if (raw === undefined || raw === "") continue;
    if (Array.isArray(base[key])) {
      const values = raw.split(",").map((part) => Number(part.trim()));
      const ascending = values.every((value, index) => Number.isInteger(value) && value > 0 && (index === 0 || value > values[index - 1]));
      if (!values.length || !ascending) throw new Error(`${name} must be ascending whole numbers separated by commas, got "${raw}"`);
      config[key] = values;
    } else {
      const value = Number(raw);
      if (!Number.isFinite(value) || value < 0) throw new Error(`${name} must be a number of zero or more, got "${raw}"`);
      config[key] = value;
    }
  }
  return config;
}

export function scalerOptions(env = process.env) {
  const token = (env.FLY_API_TOKEN || "").trim() || null;
  const applyRequested = env.RUDDER_SCALER_APPLY === "1";
  const port = env.RUDDER_SCALER_PORT === undefined || env.RUDDER_SCALER_PORT === "" ? 9464 : Number(env.RUDDER_SCALER_PORT);
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error(`RUDDER_SCALER_PORT must be a port number, got "${env.RUDDER_SCALER_PORT}"`);
  return {
    prometheusUrl: env.PROMETHEUS_URL || "http://localhost:9090",
    flyApp: env.RUDDER_FLY_APP || "rudder-workers-exla",
    intervalMs: positive(env.RUDDER_SCALER_INTERVAL_MS, 60_000),
    queryTimeoutMs: positive(env.RUDDER_SCALER_QUERY_TIMEOUT_MS, 10_000),
    port,
    host: env.RUDDER_SCALER_HOST || undefined,
    statePath: statePath(env),
    selector: (env.RUDDER_SCALER_SELECTOR || "").trim(),
    applyRequested,
    apply: applyRequested && Boolean(token),
    token,
    config: configFromEnv(env),
  };
}

/**
 * The scaler without its timer or server, so tests can drive one evaluation
 * at a time. `fetch` serves both Prometheus and Fly.
 */
export function createScaler({ options, fetch: fetchImpl = globalThis.fetch, log = defaultLog, now = Date.now } = {}) {
  const prometheus = createPrometheusClient({ url: options.prometheusUrl, fetch: fetchImpl, timeoutMs: options.queryTimeoutMs });
  const executor = createFlyExecutor({ token: options.token, app: options.flyApp, fetch: fetchImpl, apply: options.apply });
  let state = loadState(options.statePath, { log });
  // Decisions from before a restart: only for logging changes and reusing dry
  // run answers, never exported as current.
  let previous = state.decisions ?? [];
  let decisions = [];
  const counts = { ok: 0, error: 0 };
  let lastEvaluationAt = null;
  let lastError = null;

  async function act(list, at) {
    const before = new Map(previous.map((decision) => [decisionKey(decision), decision]));
    let resizes = 0;
    for (const decision of list) {
      if (!EXECUTABLE_ACTIONS.has(decision.action) || decision.state !== "recommended" || !decision.target) continue;
      if (!executor.apply) {
        // A dry run asks Fly once per new decision, not every minute.
        const last = before.get(decisionKey(decision));
        if (last?.executor && last.state === decision.state && targetText(last) === targetText(decision)) {
          decision.executor = last.executor;
        } else {
          decision.executor = await executor.execute(decision);
        }
        continue;
      }
      if (RESIZE_ACTIONS.has(decision.action)) {
        if (resizes >= options.config.maxResizesPerEvaluation) continue;
        resizes += 1;
      }
      const result = await executor.execute(decision);
      decision.executor = result;
      state = recordOutcome(state, decision, result, at);
      if (result.applied) {
        decision.state = "applied";
        decision.reason = `${decision.reason} ${appliedText(result)}`;
      } else {
        decision.state = "failed";
        decision.reason = `${decision.reason} The attempt failed: ${result.refused ? `refused, ${result.refused}` : result.error}.`;
      }
    }
  }

  function failed(message) {
    counts.error += 1;
    if (message !== lastError) log(`evaluation failed: ${message}`);
    lastError = message;
    return { ok: false, error: message };
  }

  async function evaluateOnce() {
    const at = now();
    let homes;
    let next;
    try {
      homes = await collectHomes(prometheus, { time: at / 1000, selector: options.selector });
      next = evaluate(homes, { now: at, state, config: options.config });
      await act(next, at);
    } catch (error) {
      return failed(error?.message || String(error));
    }
    for (const line of decisionChanges(previous, next)) log(line);
    decisions = next;
    previous = next;
    state = recordEvaluation(state, { homes, decisions: next, now: at });
    try {
      saveState(options.statePath, state);
    } catch (error) {
      // The decisions stand, but a restart would forget cooldowns: an error.
      return failed(`could not save state to ${options.statePath}: ${error.message}`);
    }
    counts.ok += 1;
    lastEvaluationAt = at;
    if (lastError) log(`evaluations work again (${homes.length} homes)`);
    lastError = null;
    return { ok: true, homes, decisions: next };
  }

  function snapshot() {
    return {
      apply: options.apply,
      applyRequested: options.applyRequested,
      prometheusUrl: options.prometheusUrl,
      flyApp: options.flyApp,
      evaluatedAt: lastEvaluationAt ? new Date(lastEvaluationAt).toISOString() : null,
      lastError,
      evaluations: { ...counts },
      decisions,
      history: Object.fromEntries(Object.entries(state.homes ?? {}).map(([homeId, entry]) => [homeId, { applied: entry.applied ?? {}, failed: entry.failed ?? {} }])),
    };
  }

  return {
    evaluateOnce,
    snapshot,
    metrics: () => metricsText({ decisions, counts, lastEvaluationAt, apply: options.apply }),
    get state() {
      return state;
    },
  };
}

/** GET /metrics, /decisions and /healthz. */
export function createServer(scaler) {
  return http.createServer((req, res) => {
    const { pathname } = new URL(req.url ?? "/", "http://scaler");
    if (req.method !== "GET" && req.method !== "HEAD") {
      res.writeHead(405, { Allow: "GET, HEAD" }).end();
      return;
    }
    if (pathname === "/metrics") {
      res.writeHead(200, { "Content-Type": "text/plain; version=0.0.4; charset=utf-8" }).end(scaler.metrics());
    } else if (pathname === "/decisions") {
      res.writeHead(200, { "Content-Type": "application/json" }).end(`${JSON.stringify(scaler.snapshot(), null, 2)}\n`);
    } else if (pathname === "/healthz") {
      // Alive is enough: a scaler that cannot reach Prometheus should keep
      // serving its error count, not be restarted.
      const { evaluatedAt, lastError, apply } = scaler.snapshot();
      res.writeHead(200, { "Content-Type": "application/json" }).end(`${JSON.stringify({ ok: true, apply, evaluatedAt, lastError })}\n`);
    } else {
      res.writeHead(404, { "Content-Type": "text/plain" }).end("not found\n");
    }
  });
}

export function metricsText({ decisions = [], counts = { ok: 0, error: 0 }, lastEvaluationAt = null, apply = false }) {
  const lines = [];
  const metric = (name, type, help, samples) => {
    lines.push(`# HELP ${name} ${help}`, `# TYPE ${name} ${type}`);
    for (const [labels, value] of samples) lines.push(`${name}${labelText(labels)} ${value}`);
  };
  metric("rudder_scaler_decision", "gauge", "1 for each current scaler decision.",
    decisions.map((decision) => [{ rudder_home_id: decision.homeId, action: decision.action, state: decision.state }, 1]));
  // Pending: wanted but not yet done, including after a failed attempt.
  const pending = decisions.filter((decision) => RESIZE_ACTIONS.has(decision.action)
    && ["recommended", "waiting-for-idle", "failed"].includes(decision.state)
    && Number.isFinite(decision.target?.memoryMb));
  metric("rudder_scaler_target_memory_bytes", "gauge", "Memory the scaler wants for the home while a resize is pending.",
    pending.map((decision) => [{ rudder_home_id: decision.homeId }, decision.target.memoryMb * MiB]));
  metric("rudder_scaler_evaluations_total", "counter", "Evaluation loops by outcome.",
    [[{ outcome: "ok" }, counts.ok], [{ outcome: "error" }, counts.error]]);
  metric("rudder_scaler_last_evaluation_timestamp_seconds", "gauge", "Unix time of the last successful evaluation, 0 before the first.",
    [[{}, lastEvaluationAt ? lastEvaluationAt / 1000 : 0]]);
  metric("rudder_scaler_apply_enabled", "gauge", "1 when the scaler may change machines, 0 for a dry run.",
    [[{}, apply ? 1 : 0]]);
  return `${lines.join("\n")}\n`;
}

/** Log lines for decisions that appeared, changed state or target, or cleared. */
export function decisionChanges(before = [], after = []) {
  const lines = [];
  const previous = new Map(before.map((decision) => [decisionKey(decision), decision]));
  const seen = new Set();
  for (const decision of after) {
    const key = decisionKey(decision);
    seen.add(key);
    const last = previous.get(key);
    if (!last || last.state !== decision.state || targetText(last) !== targetText(decision)) {
      lines.push(`${decision.homeId} ${decision.action} ${decision.state}${targetText(decision)}: ${decision.reason}`);
    }
  }
  for (const [key, last] of previous) {
    if (!seen.has(key)) lines.push(`${last.homeId} ${last.action} cleared`);
  }
  return lines;
}

function decisionKey(decision) {
  return `${decision.homeId}\u0000${decision.action}`;
}

function targetText(decision) {
  if (Number.isFinite(decision.target?.memoryMb)) return ` (target ${decision.target.memoryMb} MiB)`;
  if (Number.isFinite(decision.target?.sizeGb)) return ` (target ${decision.target.sizeGb} GB)`;
  return "";
}

function appliedText(result) {
  if (result.memoryMb) {
    return result.reconciled
      ? `Fly already shows ${result.memoryMb.to} MiB, so it is recorded as applied.`
      : `Applied: memory ${result.memoryMb.from} -> ${result.memoryMb.to} MiB; the machine restarts.`;
  }
  if (result.sizeGb) {
    const restart = result.needsRestart
      ? " Fly says the machine needs a restart to use the new size; the scaler does not restart it, so it takes effect on the home's next idle restart."
      : "";
    return result.reconciled
      ? `Fly already shows volume ${result.volumeId} at ${result.sizeGb.to} GB, so it is recorded as applied.`
      : `Applied: volume ${result.volumeId} ${result.sizeGb.from ?? "?"} -> ${result.sizeGb.to} GB.${restart}`;
  }
  return "Applied.";
}

function labelText(labels) {
  const entries = Object.entries(labels);
  if (!entries.length) return "";
  const escape = (value) => String(value).replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n");
  return `{${entries.map(([name, value]) => `${name}="${escape(value)}"`).join(",")}}`;
}

function positive(raw, fallback) {
  const value = Number(raw);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function defaultLog(line) {
  console.log(`${new Date().toISOString()} [scaler] ${line}`);
}

async function main(argv) {
  const options = scalerOptions();
  const scaler = createScaler({ options });
  if (argv.includes("--once")) {
    const result = await scaler.evaluateOnce();
    process.stdout.write(`${JSON.stringify(scaler.snapshot(), null, 2)}\n`);
    process.exit(result.ok ? 0 : 1);
  }

  const server = createServer(scaler);
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port, options.host, resolve);
  });
  const mode = options.apply
    ? `APPLY: may resize machines and extend volumes in ${options.flyApp}`
    : options.applyRequested ? "dry run (RUDDER_SCALER_APPLY=1 but FLY_API_TOKEN is not set)" : "dry run";
  defaultLog(`listening on port ${server.address().port}; ${mode}; Prometheus at ${options.prometheusUrl}; every ${options.intervalMs} ms`);

  let timer = null;
  let running = null;
  let stopping = false;
  const tick = async () => {
    running = scaler.evaluateOnce();
    await running;
    running = null;
    if (!stopping) timer = setTimeout(tick, options.intervalMs);
  };
  const stop = async (signal) => {
    if (stopping) return;
    stopping = true;
    clearTimeout(timer);
    defaultLog(`${signal}: finishing the current evaluation and stopping`);
    // An evaluation may be mid-resize; let it finish and record what it did.
    await running;
    server.close();
    server.closeAllConnections?.();
    process.exit(0);
  };
  process.on("SIGTERM", () => void stop("SIGTERM"));
  process.on("SIGINT", () => void stop("SIGINT"));
  void tick();
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fs.realpathSync(SCRIPT_PATH)) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`[scaler] ${error.message}`);
    process.exit(1);
  });
}
