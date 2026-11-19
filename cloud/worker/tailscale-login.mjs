// Tailscale on the cloud machine, as a step the user takes from the app.
//
// Connect runs `tailscale up` with no auth key. tailscaled asks Tailscale for
// a sign-in link, `up` prints it ("To authenticate, visit: https://login.
// tailscale.com/a/..."), and the app opens it. `up` exits 0 once the machine
// is on the tailnet; a tailnet that approves new devices holds it at
// NeedsMachineAuth until an admin does. Disconnect is `tailscale logout`.
//
// tailscaled runs as this user in userspace networking, started here when it
// is not already answering on its socket. Its node key lives in the state dir
// on the volume, so a restart rejoins without signing in again: boot() starts
// tailscaled whenever that state exists.
//
// The sign-in link goes only to the request that started Connect: whoever
// opens it puts this machine on their tailnet. The state every client polls
// (snapshot) says where the sign-in is, never the link.

import { spawn as spawnProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { runTailnetCommand, tailnetStatus } from "./tailscale-previews.mjs";

// How long a sign-in link waits for the user (`tailscale up --timeout`).
const LOGIN_TIMEOUT_MS = 10 * 60 * 1000;
// `up` prints the link within a second or two; this long means it never will.
const URL_WAIT_MS = 45_000;
const DAEMON_WAIT_MS = 10_000;
// A finished sign-in stays in the snapshot so the app can say how it ended.
const ENDED_TTL_MS = 10 * 60 * 1000;

const AUTH_URL = /To authenticate, visit:\s+(https:\/\/\S+)/;
const APPROVE = /To approve your machine, visit \(as admin\):\s+(https:\/\/\S+)/;
const TIMED_OUT = /timeout waiting for Tailscale service to enter a Running state/;
// tailscaled's log lines start with a timestamp.
const LOG_STAMP = /^\d{4}\/\d\d\/\d\d \d\d:\d\d:\d\d(?:\.\d+)? /;

/** The sign-in link `tailscale up` printed, or null. */
export function authUrl(output) {
  return AUTH_URL.exec(output)?.[1] ?? null;
}

function span(ms) {
  const [count, unit] = ms >= 60_000 ? [Math.round(ms / 60_000), "minute"] : [Math.round(ms / 1000), "second"];
  return `${count} ${unit}${count === 1 ? "" : "s"}`;
}

/** Why `tailscale up` failed, from what it printed, without the links. */
export function upFailure(output, { approval = false, timeoutMs = LOGIN_TIMEOUT_MS } = {}) {
  if (TIMED_OUT.test(output)) {
    return approval
      ? `No admin approved this machine within ${span(timeoutMs)}. It joins your tailnet once one does.`
      : `The sign-in link was not used within ${span(timeoutMs)}, so it expired. Connect again for a new one.`;
  }
  const text = output.replace(new RegExp(AUTH_URL.source, "g"), "").replace(new RegExp(APPROVE.source, "g"), "")
    .replace(/^Success\.$/gm, "").replace(/\s+/g, " ").trim();
  return text.slice(0, 400);
}

/**
 * @param {{
 *   socket: string,
 *   stateDir: string,
 *   hostname: string,
 *   log?: (message: string) => void,
 *   onChange?: () => void,
 *   spawn?: typeof spawnProcess,
 *   loginTimeoutMs?: number,
 *   urlWaitMs?: number,
 *   daemonWaitMs?: number,
 * }} options
 */
export function createTailscale({
  socket, stateDir, hostname, log = () => {}, onChange = () => {}, spawn = spawnProcess,
  loginTimeoutMs = LOGIN_TIMEOUT_MS, urlWaitMs = URL_WAIT_MS, daemonWaitMs = DAEMON_WAIT_MS,
}) {
  // What the app shows: hostname, dnsName and online as the previews read
  // them (tailnetStatus); backendState is null while tailscaled is not
  // answering; error is why tailscaled is not running, when it should be.
  const state = { hostname, dnsName: null, online: false, backendState: null, tailnetName: null, error: null };
  let daemon = null; // the tailscaled this process started
  let daemonLog = ""; // the end of its stderr, for the reason it stopped
  let daemonError = null;
  let starting = null; // the ensureDaemon in flight
  let session = null; // the last sign-in

  function changed() {
    try { onChange(); } catch (error) { log(`tailscale: ${error.message}`); }
  }

  const NO_CLI = "tailscale is not installed on this machine";
  const command = (args, timeoutMs) => runTailnetCommand(args, { socket, timeoutMs });
  const commandError = (result) => (result.stderr === "ENOENT"
    ? NO_CLI
    : result.stderr.trim() || result.stdout.trim() || `exit ${result.code}`);

  async function readStatus() {
    const result = await command(["status", "--json"], 3000);
    if (result.code !== 0) return { reachable: false, detail: commandError(result) };
    try {
      return { reachable: true, value: JSON.parse(result.stdout) };
    } catch {
      return { reachable: false, detail: "tailscale status printed no JSON" };
    }
  }

  function lastDaemonLine() {
    const lines = daemonLog.split("\n").map((line) => line.replace(LOG_STAMP, "").trim()).filter(Boolean);
    return lines.at(-1) ?? "";
  }

  function startDaemon() {
    fs.mkdirSync(stateDir, { recursive: true, mode: 0o700 });
    daemonLog = "";
    daemonError = null;
    const child = spawn("tailscaled", [
      "--tun=userspace-networking",
      `--statedir=${stateDir}`,
      `--socket=${socket}`,
      "--port=41641",
    ], { stdio: ["ignore", "ignore", "pipe"] });
    daemon = child;
    child.stderr.on("data", (data) => { daemonLog = (daemonLog + data).slice(-8000); });
    child.on("error", (error) => {
      if (daemon !== child) return;
      daemon = null;
      daemonError = error.code === "ENOENT" ? "tailscaled is not installed on this machine" : `tailscaled could not start: ${error.message}`;
      log(`tailscale: ${daemonError}`);
      Object.assign(state, { online: false, backendState: null, error: daemonError });
      changed();
    });
    child.on("exit", (code, signal) => {
      if (daemon !== child) return;
      daemon = null;
      const last = lastDaemonLine();
      daemonError = `tailscaled exited (${signal || code})${last ? `: ${last}` : ""}`;
      log(`tailscale: ${daemonError}`);
      Object.assign(state, { online: false, backendState: null, error: daemonError });
      changed();
    });
  }

  // tailscaled answering on the socket: the one already running, or one
  // started now. Throws why it is not.
  function ensureDaemon() {
    starting ??= (async () => {
      let status = await readStatus();
      if (status.reachable) return;
      if (status.detail === NO_CLI) throw new Error(NO_CLI);
      if (!daemon) startDaemon();
      const deadline = Date.now() + daemonWaitMs;
      while (Date.now() < deadline) {
        if (daemonError) throw new Error(daemonError);
        status = await readStatus();
        if (status.reachable) return;
        await sleep(200);
      }
      if (daemonError) throw new Error(daemonError);
      daemonError = `tailscaled did not start within ${span(daemonWaitMs)} (${status?.detail || lastDaemonLine() || "no output"})`;
      throw new Error(daemonError);
    })().finally(() => { starting = null; });
    return starting;
  }

  async function refresh() {
    const before = JSON.stringify(state);
    const status = await readStatus();
    if (status.reachable) {
      const seen = tailnetStatus(status.value, hostname);
      Object.assign(state, {
        hostname: seen.hostname,
        dnsName: seen.dnsName,
        online: seen.online,
        backendState: typeof status.value?.BackendState === "string" ? status.value.BackendState : null,
        tailnetName: typeof status.value?.CurrentTailnet?.Name === "string" ? status.value.CurrentTailnet.Name : null,
        error: null,
      });
      daemonError = null;
    } else {
      // Never started (no sign-in yet) is not an error; one that should be
      // running and is not answering is.
      const error = daemonError ?? (daemon ? `tailscaled is not answering: ${status.detail}` : null);
      Object.assign(state, { dnsName: null, online: false, backendState: null, tailnetName: null, error });
    }
    if (before !== JSON.stringify(state)) changed();
  }

  // After a restart: tailscaled again on the node key from the last sign-in.
  async function boot() {
    if (fs.existsSync(path.join(stateDir, "tailscaled.state"))) {
      try {
        await ensureDaemon();
      } catch (error) {
        log(`tailscale: ${error.message}`);
      }
    }
    await refresh();
  }

  function end(current, status, error = null) {
    if (current.endedAt) return;
    current.status = status;
    current.error = error;
    current.endedAt = new Date().toISOString();
    clearTimeout(current.urlTimer);
    if (current.child && current.child.exitCode === null && current.child.signalCode === null) {
      try { current.child.kill(); } catch { /* already gone */ }
    }
    log(`tailscale sign-in: ${status}${error ? ` (${error})` : ""}`);
    if (status === "failed") current.reject(new Error(error));
    else current.resolve(publicStart(current));
    changed();
  }

  /** Start a sign-in, or hand back the one in flight. Resolves once its link is out. */
  async function connect() {
    if (session && !session.endedAt) {
      return session.status === "starting" ? session.ready : publicStart(session);
    }
    const current = {
      id: randomUUID(),
      status: "starting",
      error: null,
      startedAt: new Date().toISOString(),
      endedAt: null,
      url: null,
      approval: false,
      output: "",
      child: null,
      urlTimer: null,
    };
    current.ready = new Promise((resolve, reject) => { current.resolve = resolve; current.reject = reject; });
    // The caller gets the rejection; nothing else waits on it.
    current.ready.catch(() => {});
    session = current;
    changed();
    void run(current);
    return current.ready;
  }

  async function run(current) {
    try {
      await ensureDaemon();
    } catch (error) {
      end(current, "failed", error.message);
      return;
    }
    await refresh();
    if (current.endedAt) return;
    if (state.online) {
      end(current, "done");
      return;
    }
    const child = spawn("tailscale", [
      "--socket", socket, "up",
      `--hostname=${hostname}`,
      "--accept-dns=false",
      `--timeout=${Math.round(loginTimeoutMs / 1000)}s`,
    ], { stdio: ["ignore", "pipe", "pipe"] });
    current.child = child;
    const read = (data) => {
      current.output = (current.output + data).slice(-16_000);
      if (current.endedAt) return;
      const url = authUrl(current.output);
      if (url && url !== current.url) {
        current.url = url;
        if (current.status === "starting") current.status = "waiting";
        current.resolve(publicStart(current));
        changed();
      }
      if (!current.approval && APPROVE.test(current.output)) {
        current.approval = true;
        current.status = "approval";
        current.resolve(publicStart(current));
        void refresh();
        changed();
      }
    };
    child.stdout.on("data", read);
    child.stderr.on("data", read);
    child.on("error", (error) => {
      end(current, "failed", error.code === "ENOENT" ? NO_CLI : `tailscale up could not start: ${error.message}`);
    });
    child.on("exit", (code, signal) => {
      void refresh().finally(() => {
        if (code === 0) end(current, "done");
        else end(current, "failed", upFailure(current.output, { approval: current.approval, timeoutMs: loginTimeoutMs }) || `tailscale up exited (${signal || code})`);
      });
    });
    current.urlTimer = setTimeout(() => {
      if (current.status === "starting") {
        end(current, "failed", `Tailscale did not show a sign-in link (${upFailure(current.output) || "no output"})`);
      }
    }, urlWaitMs);
  }

  /** `tailscale logout`: the machine leaves the tailnet and forgets its node key. */
  async function disconnect() {
    if (session && !session.endedAt) end(session, "cancelled");
    const result = await command(["logout"], 30_000);
    if (result.code !== 0) {
      throw new Error(`tailscale logout failed: ${commandError(result)}`);
    }
    await refresh();
    return snapshot();
  }

  /** What the home's state message carries as `tailnet`. */
  function snapshot() {
    const recent = session && !(session.endedAt && Date.now() - Date.parse(session.endedAt) > ENDED_TTL_MS);
    return { ...state, login: recent ? publicView(session) : null };
  }

  function dispose() {
    if (session && !session.endedAt) end(session, "cancelled");
  }

  return { boot, refresh, connect, disconnect, snapshot, dispose };
}

function publicView(current) {
  return {
    id: current.id,
    status: current.status,
    error: current.error,
    startedAt: current.startedAt,
    endedAt: current.endedAt,
  };
}

// Only for the request that started the sign-in.
function publicStart(current) {
  return { ...publicView(current), url: current.url };
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
