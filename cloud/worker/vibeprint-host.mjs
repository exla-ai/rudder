// One vibeprint canvas per project, run BESIDE its agents, never inside them.
//
// vibeprint (npm `vibeprint`, the user's architecture canvas) normally lives in
// the repo: `vibeprint init` adds a rule to AGENTS.md, an import to CLAUDE.md
// and an MCP server, and the daemon writes and commits `.vibeprint/`. Here none
// of that happens. The daemon runs with `--state` in a directory of its own
// under the volume, so the checkout is only read: no agent sees the canvas in
// its context, its files or its diff. The app shows the canvas beside the chat
// through vibeprint's relay (`https://<id>.live.vibeprint.viraat.dev`), framed
// with `--embed-origin` set to this home's Cloud; the iPad uses the same link.
// What crosses to an agent is only what the user sends it as a message
// ("Send to agent" posts the canvas brief through the normal send path).
//
// Turning it on is remembered (`rudder.json` in its state directory), so a
// restarted supervisor brings the canvas, and the iPad's link, back. So are the
// agents whose turns update the canvas (vibeprint-track.mjs).
import { spawn } from "node:child_process";
import fs from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { createBuildTracker, readAgentDiff, RUN_ID } from "./vibeprint-track.mjs";

const ID = /^[A-Za-z0-9_-]{1,128}$/;
const START_TIMEOUT_MS = 20_000;
const STOP_GRACE_MS = 5000;
const TRACK_EVERY_MS = 15_000;

/** The pinned vibeprint CLI (a dependency of the worker package). */
export function vibeprintCli() {
  const require = createRequire(import.meta.url);
  let pkg;
  try {
    pkg = require.resolve("vibeprint/package.json");
  } catch {
    throw new Error("vibeprint is not installed on this home. It ships with the worker image.");
  }
  return path.join(path.dirname(pkg), "dist", "cli.js");
}

/** Its relay link without the token, from runtime/daemon.json, or null until the relay accepts it. */
function relayBase(info) {
  const url = (info?.urls ?? []).find((u) => typeof u === "string" && u.startsWith("https://"));
  if (!url) return null;
  const u = new URL(url);
  return `${u.origin}/`;
}

function readJson(file) {
  try { return JSON.parse(fs.readFileSync(file, "utf8")); } catch { return null; }
}

function pidAlive(pid) {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

/** A daemon a previous supervisor started for this same state directory. */
function staleDaemon(dir) {
  const info = readJson(path.join(dir, "runtime", "daemon.json"));
  if (!Number.isSafeInteger(info?.pid) || info.pid <= 1 || !pidAlive(info.pid)) return null;
  let cmdline = "";
  try { cmdline = fs.readFileSync(`/proc/${info.pid}/cmdline`, "utf8").split("\0").join(" "); } catch { return null; }
  return cmdline.includes("vibeprint") && cmdline.includes(dir) ? info.pid : null;
}

/**
 * @param {object} opts
 * @param {string} opts.stateRoot   where each project's canvas lives (`<stateRoot>/<workspaceId>`)
 * @param {string} opts.embedOrigin the Cloud origin the app is served from (frames the canvas)
 * @param {() => Record<string,string>} opts.env  environment for the daemon
 * @param {(message: string) => void} opts.log
 * @param {() => string} [opts.cli]  path to vibeprint's dist/cli.js
 * @param {string[]} [opts.extraArgs] more `serve` flags (tests: `--no-relay`)
 */
export function createVibeprintHosts({ stateRoot, embedOrigin, env, log, cli = vibeprintCli, extraArgs = [] }) {
  /** workspaceId -> { child, dir, startedAt, error, tail, stopping } */
  const hosts = new Map();

  const dirOf = (workspaceId) => {
    if (!ID.test(workspaceId || "")) throw new Error("vibeprint: not a project id");
    return path.join(stateRoot, workspaceId);
  };
  /** rudder.json: { enabled, tracked: { runId: { pending, lastHash, lastAt, error } } } */
  const config = (dir) => {
    const c = readJson(path.join(dir, "rudder.json"));
    return { enabled: c?.enabled === true, tracked: c?.tracked && typeof c.tracked === "object" ? c.tracked : {} };
  };
  const writeConfig = (dir, c) => {
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    const file = path.join(dir, "rudder.json");
    fs.writeFileSync(`${file}.tmp`, `${JSON.stringify(c)}\n`, { mode: 0o600 });
    fs.renameSync(`${file}.tmp`, file);
  };
  const enabled = (dir) => config(dir).enabled;
  const setEnabled = (dir, on) => writeConfig(dir, { ...config(dir), enabled: on });
  const saveTracked = (dir, runId, entry) => {
    const c = config(dir);
    if (entry) c.tracked[runId] = entry;
    else delete c.tracked[runId];
    writeConfig(dir, c);
  };

  /** The running daemon's port and token, or null. */
  const daemonOf = (workspaceId) => {
    const host = hosts.get(workspaceId);
    const info = host?.child ? readJson(path.join(host.dir, "runtime", "daemon.json")) : null;
    return info && info.pid === host.child.pid && Number.isInteger(info.port) && typeof info.token === "string" ? info : null;
  };
  const tracker = createBuildTracker({
    log,
    diff: (cwd) => readAgentDiff(cwd, env()),
    async send(project, body) {
      const info = daemonOf(project.workspaceId);
      if (!info) throw new Error("The canvas is not running");
      const res = await fetch(`http://127.0.0.1:${info.port}/api/track`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-vibeprint-token": info.token },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(10_000),
      });
      if (res.status === 409) throw Object.assign(new Error("The canvas is busy"), { busy: true });
      if (!res.ok) throw new Error((await res.json().catch(() => null))?.error || `The canvas did not answer (${res.status})`);
    },
  });
  let ticking = false;
  /** One look at every running canvas's tracked agents. */
  async function trackTick() {
    if (ticking) return;
    ticking = true;
    try {
      for (const [workspaceId, host] of hosts) {
        if (!host.project || !daemonOf(workspaceId)) continue;
        const tracked = config(host.dir).tracked;
        if (!Object.keys(tracked).length) continue;
        await tracker.tick(host.project, tracked, (runId, entry) => saveTracked(host.dir, runId, entry))
          .catch((error) => log(`vibeprint ${host.project.name}: tracking: ${error.message}`));
      }
    } finally {
      ticking = false;
    }
  }
  const timer = setInterval(() => void trackTick(), TRACK_EVERY_MS);
  timer.unref();

  /** What the panel shows for one agent: whether its turns update the canvas. */
  function trackingView(dir, runId) {
    const entry = config(dir).tracked[runId];
    if (!entry) return { on: false };
    return { on: true, pending: entry.pending === true, ...(entry.lastAt ? { lastAt: new Date(entry.lastAt).toISOString() } : {}), ...(entry.error ? { error: entry.error } : {}) };
  }

  /** Track an agent's turns, or stop. `check`: also check its changes as soon as it is not working. */
  function setTracking(project, runId, on, check) {
    const dir = dirOf(project.workspaceId);
    if (!on) { saveTracked(dir, runId, null); return; }
    const entry = config(dir).tracked[runId] ?? { pending: false };
    saveTracked(dir, runId, check ? { ...entry, pending: true, lastAt: 0 } : entry);
    if (check) void trackTick();
  }

  function view(project) {
    const dir = dirOf(project.workspaceId);
    const host = hosts.get(project.workspaceId);
    if (!host?.child) {
      if (host?.error) return { state: "failed", error: host.error };
      return { state: "off" };
    }
    const info = readJson(path.join(dir, "runtime", "daemon.json"));
    const mine = info && info.pid === host.child.pid;
    const url = mine ? relayBase(info) : null;
    if (!mine || !url || typeof info.token !== "string") {
      const late = Date.now() - host.startedAt > START_TIMEOUT_MS;
      return { state: "starting", ...(late ? { note: "Still connecting to the vibeprint relay" } : {}) };
    }
    return { state: "running", url, token: info.token };
  }

  function start(project) {
    const dir = dirOf(project.workspaceId);
    if (hosts.get(project.workspaceId)?.child) return;
    if (!/^https?:\/\/[^/]+$/.test(embedOrigin || "")) throw new Error("This home does not know its Cloud address (RUDDER_CLOUD_URL)");
    if (!fs.existsSync(path.join(project.dir, ".git"))) throw new Error("This project is still being set up on the home");
    fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
    const stale = staleDaemon(dir);
    if (stale) {
      log(`vibeprint ${project.name}: stopping the canvas a previous supervisor left (pid ${stale})`);
      try { process.kill(stale, "SIGTERM"); } catch { /* already gone */ }
    }
    const args = [cli(), "serve", "--root", project.dir, "--state", dir, "--host", "127.0.0.1", "--port", "0", "--embed-origin", embedOrigin, ...extraArgs];
    const child = spawn(process.execPath, args, {
      cwd: dir,
      env: { ...env(), VIBEPRINT_NO_UPDATE_CHECK: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const host = { child, dir, project, startedAt: Date.now(), error: null, tail: [], stopping: false };
    hosts.set(project.workspaceId, host);
    const keep = (chunk) => {
      for (const line of String(chunk).split("\n")) if (line.trim()) host.tail.push(line.trim());
      host.tail.splice(0, Math.max(0, host.tail.length - 20));
    };
    child.stdout.on("data", keep);
    child.stderr.on("data", keep);
    child.on("error", (error) => { host.error = `vibeprint could not start: ${error.message}`; });
    child.on("exit", (code, signal) => {
      if (hosts.get(project.workspaceId) !== host) return;
      host.child = null;
      if (host.stopping) { hosts.delete(project.workspaceId); return; }
      const last = host.tail.filter((l) => !/^\[vibeprint\] (git|relay):/.test(l)).at(-1) ?? "";
      host.error = `vibeprint exited (${signal ?? `exit ${code}`})${last ? `: ${last.replace(/^vibeprint: /, "").slice(0, 240)}` : ""}`;
      log(`vibeprint ${project.name}: ${host.error}`);
    });
    log(`vibeprint ${project.name}: started (pid ${child.pid})`);
  }

  function stop(workspaceId) {
    const host = hosts.get(workspaceId);
    if (!host) return;
    if (!host.child) { hosts.delete(workspaceId); return; }
    host.stopping = true;
    const { child } = host;
    child.kill("SIGTERM");
    setTimeout(() => { if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL"); }, STOP_GRACE_MS).unref();
  }

  async function brief(project, since) {
    const info = readJson(path.join(dirOf(project.workspaceId), "runtime", "daemon.json"));
    const host = hosts.get(project.workspaceId);
    if (!host?.child || info?.pid !== host.child.pid) throw new Error("The canvas is not running");
    const query = typeof since === "string" && since ? `?since=${encodeURIComponent(since)}` : "";
    const res = await fetch(`http://127.0.0.1:${info.port}/api/brief${query}`, {
      headers: { "x-vibeprint-token": info.token },
      signal: AbortSignal.timeout(5000),
    });
    if (!res.ok) throw new Error(`The canvas did not answer (${res.status})`);
    const body = await res.json();
    if (typeof body?.text !== "string" || typeof body.at !== "string" || !Number.isInteger(body.components)) throw new Error("The canvas sent an invalid brief");
    return { text: body.text, at: body.at, components: body.components };
  }

  return {
    /** The app's `vibeprint` RPC for one project: status, start, stop, brief or tracking. */
    async handle(project, params) {
      const op = String(params?.op || "");
      const dir = dirOf(project.workspaceId);
      const run = params?.run == null ? null : String(params.run);
      if (run !== null && !RUN_ID.test(run)) throw new Error("vibeprint: not an agent id");
      const withTracking = (v) => (run ? { ...v, tracking: trackingView(dir, run) } : v);
      if (op === "status") {
        // A canvas turned on before a restart comes back when the app looks for it.
        if (!hosts.get(project.workspaceId)?.child && !hosts.get(project.workspaceId)?.error && enabled(dir)) start(project);
        return withTracking(view(project));
      }
      if (op === "tracking") {
        if (!run) throw new Error("vibeprint: tracking needs an agent");
        setTracking(project, run, params.on === true, params.check === true);
        return withTracking(view(project));
      }
      if (op === "start") {
        if (hosts.get(project.workspaceId)?.error) hosts.delete(project.workspaceId);
        setEnabled(dir, true);
        start(project);
        return withTracking(view(project));
      }
      if (op === "stop") {
        setEnabled(dir, false);
        stop(project.workspaceId);
        return withTracking({ state: "off" });
      }
      if (op === "brief") return brief(project, params.since);
      throw new Error("vibeprint: unknown request");
    },
    /** Projects present now: bring back canvases left on; stop those of projects that went away. */
    resume(projectList) {
      const ids = new Set(projectList.map((p) => p.workspaceId));
      for (const id of hosts.keys()) if (!ids.has(id)) stop(id);
      for (const project of projectList) {
        if (!ID.test(project.workspaceId || "") || hosts.has(project.workspaceId)) continue;
        if (!enabled(dirOf(project.workspaceId)) || !fs.existsSync(path.join(project.dir, ".git"))) continue;
        try { start(project); } catch (error) { log(`vibeprint ${project.name}: ${error.message}`); }
      }
    },
    stop,
    stopAll() {
      clearInterval(timer);
      for (const id of [...hosts.keys()]) stop(id);
    },
    /** One look at the tracked agents now (the timer does this every 15 s). */
    trackTick,
  };
}
