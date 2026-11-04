#!/usr/bin/env node
// The home machine's cleanup. Once an hour, and sooner when memory runs short,
// a small model looks at what is running and gives memory back: it puts agents
// nobody has touched for hours to sleep and stops processes agents started and
// forgot (dev servers, watchers, test runners). Without it the machine filled
// 16 GB with resumed sessions idle for days, thrashed, and every terminal
// attached to it dropped while the supervisor starved.
//
// The model decides; this file is all it can act through (it runs with Bash
// limited to `node cleanup.mjs ...`), and the rules that make a wrong call
// cheap live here and in the dashboard, not in the prompt:
//   - an agent is put to sleep by its own dashboard (steer kind "park"), which
//     refuses one that printed anything in the last 20 minutes, waits on a
//     prompt, or has a terminal attached. Asleep keeps the conversation and the
//     workspace; one message wakes it.
//   - a process is stopped only if it is ours, has run for an hour, is not an
//     agent, the supervisor, a dashboard or tailscale, and no agent active in
//     the last 20 minutes owns it.
//
//   node cleanup.mjs inventory                         what is running (JSON)
//   node cleanup.mjs park <project> <runId> "<reason>"  put an idle agent to sleep
//   node cleanup.mjs stop <pid> "<reason>"              stop a leftover process

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { statusFields, pidAlive } from "./agent-status.mjs";
import { readAsks } from "./asks.mjs";
import { transcriptFile, userTask } from "./transcripts.mjs";
import { queueSteer, readSteerReceipt } from "./steer-inbox.mjs";

export const CLI_PATH = fileURLToPath(import.meta.url);
const MINUTE = 60_000;
// An agent whose transcript moved this recently is working, whatever else says.
const ACTIVE_WINDOW_MS = 20 * MINUTE;
// Younger processes are never stopped: a build or test run may just be slow.
const STOP_MIN_AGE_MS = 60 * MINUTE;
const CLK_TCK = 100;

/** Where things live; overridable so tests run against a fake machine. */
export function cleanupPaths(env = process.env) {
  const home = env.HOME || os.homedir();
  return {
    home,
    codeDir: path.join(env.RUDDER_HOME_VOLUME || "/workspace", "code"),
    rudderHome: env.RUDDER_HOME || path.join(home, ".rudder"),
    procRoot: env.RUDDER_CLEANUP_PROC || "/proc",
  };
}

// ---------------------------------------------------------------------------
// The machine: memory, pressure, processes.
// ---------------------------------------------------------------------------

export function readMachine(procRoot) {
  const meminfo = readText(path.join(procRoot, "meminfo"));
  const kb = (key) => Number(new RegExp(`^${key}:\\s+(\\d+)`, "m").exec(meminfo)?.[1] ?? 0);
  const pressure = (kind, line) => {
    const match = new RegExp(`^${line} avg10=[\\d.]+ avg60=([\\d.]+)`, "m").exec(readText(path.join(procRoot, "pressure", kind)));
    return match ? Number(match[1]) : null;
  };
  const load = readText(path.join(procRoot, "loadavg")).split(/\s+/);
  return {
    cpus: os.cpus().length,
    memTotalMb: Math.round(kb("MemTotal") / 1024),
    memAvailableMb: Math.round(kb("MemAvailable") / 1024),
    swapTotalMb: Math.round(kb("SwapTotal") / 1024),
    load1: Number(load[0]) || 0,
    // Share of the last minute every non-idle task was stalled on memory / IO.
    memoryPressure: pressure("memory", "full"),
    ioPressure: pressure("io", "some"),
  };
}

/** Memory is short enough to clean up now instead of waiting for the hour. */
export function memoryIsShort(machine) {
  if (!machine.memTotalMb) return false;
  return machine.memAvailableMb < machine.memTotalMb * 0.12 || (machine.memoryPressure ?? 0) >= 20;
}

export function readProcesses(procRoot) {
  const uptimeSec = Number(readText(path.join(procRoot, "uptime")).split(/\s+/)[0]) || 0;
  const processes = new Map();
  let names = [];
  try {
    names = fs.readdirSync(procRoot);
  } catch {
    return processes;
  }
  for (const name of names) {
    if (!/^\d+$/.test(name)) continue;
    const dir = path.join(procRoot, name);
    const stat = readText(path.join(dir, "stat"));
    // comm sits in parentheses and may itself contain spaces or parentheses.
    const close = stat.lastIndexOf(")");
    if (close < 0) continue;
    const fields = stat.slice(close + 2).split(" ");
    const status = readText(path.join(dir, "status"));
    const cmdline = readText(path.join(dir, "cmdline")).split("\0").filter(Boolean).join(" ");
    let cwd = null;
    try {
      cwd = fs.readlinkSync(path.join(dir, "cwd"));
    } catch {
      // gone, or not ours
    }
    processes.set(Number(name), {
      pid: Number(name),
      comm: stat.slice(stat.indexOf("(") + 1, close),
      ppid: Number(fields[1]) || 0,
      pgid: Number(fields[2]) || 0,
      ageMs: Math.max(0, (uptimeSec - (Number(fields[19]) || 0) / CLK_TCK) * 1000),
      rssMb: Math.round(Number(/^VmRSS:\s+(\d+)/m.exec(status)?.[1] ?? 0) / 1024),
      uid: Number(/^Uid:\s+(\d+)/m.exec(status)?.[1] ?? -1),
      cmdline,
      cwd,
    });
  }
  return processes;
}

function descendants(processes, root) {
  const children = new Map();
  for (const proc of processes.values()) {
    if (!children.has(proc.ppid)) children.set(proc.ppid, []);
    children.get(proc.ppid).push(proc.pid);
  }
  const out = new Set();
  const stack = [root];
  while (stack.length) {
    const pid = stack.pop();
    for (const child of children.get(pid) ?? []) {
      if (!out.has(child)) {
        out.add(child);
        stack.push(child);
      }
    }
  }
  return out;
}

function ancestors(processes, pid) {
  const out = new Set();
  let current = processes.get(pid)?.ppid;
  while (current && current > 1 && !out.has(current)) {
    out.add(current);
    current = processes.get(current)?.ppid;
  }
  return out;
}

// The machine's own plumbing: never anything for the cleanup to stop.
function isInfrastructure(proc) {
  return proc.pid <= 1
    || /(^|\/)rudder( |$)/.test(proc.cmdline.split(" ").slice(0, 2).join(" "))
    || /^(rudder-native|rudder|tailscaled|tailscale|hallpass|sshd|init|tini)$/.test(proc.comm)
    || /\/opt\/rudder(-worker)?\//.test(proc.cmdline);
}

// An agent CLI itself: put to sleep through its dashboard, never killed.
export function isAgentCli(proc) {
  return /^(claude|codex|opencode)$/.test(proc.comm)
    || /(^|\/)(claude|codex|opencode)( |$)/.test(proc.cmdline.split(" ")[0] ?? "")
    || /\/claude\/versions\//.test(proc.cmdline);
}

// ---------------------------------------------------------------------------
// The agents: every run.json under ~/code that claims a live process.
// ---------------------------------------------------------------------------

export function readAgentRows({ codeDir, home, rudderHome, alive = pidAlive, now = Date.now() }) {
  const rows = [];
  let projects = [];
  try {
    projects = fs.readdirSync(codeDir);
  } catch {
    return rows;
  }
  for (const project of projects) {
    const runsDir = path.join(codeDir, project, ".rudder", "runs");
    let ids = [];
    try {
      ids = fs.readdirSync(runsDir);
    } catch {
      continue;
    }
    for (const runId of ids) {
      let record;
      try {
        record = JSON.parse(fs.readFileSync(path.join(runsDir, runId, "run.json"), "utf8"));
      } catch {
        continue;
      }
      if (!record || record.id !== runId) continue;
      const pid = record.process?.pid;
      const live = Number.isInteger(pid) && pid > 1 && alive(pid);
      let transcriptAt = null;
      try {
        const file = transcriptFile(record, home);
        if (file) transcriptAt = fs.statSync(file).mtimeMs;
      } catch {
        // no transcript yet
      }
      const lastInputAt = toMillis(record.lastUserInputAt);
      const lastActivity = Math.max(transcriptAt ?? 0, lastInputAt ?? 0, toMillis(record.createdAt) ?? 0);
      rows.push({
        project,
        runId,
        pid: live ? pid : null,
        live,
        parked: Boolean(record.parked?.at),
        title: String(record.taskSummary || userTask(record.task) || runId).slice(0, 80),
        status: statusFields(record, { alive }).status,
        mode: record.mode ?? null,
        backend: record.backend ?? "claude",
        orchestrator: record.mode === "rudder-plan",
        sessionId: record.session?.nativeSessionId ?? null,
        asks: /^[A-Za-z0-9._-]+$/.test(runId) ? readAsks(path.join(rudderHome, "signals", `${runId}.asks`)).length : 0,
        idleMinutes: lastActivity ? Math.max(0, Math.round((now - lastActivity) / MINUTE)) : null,
        lastInputMinutesAgo: lastInputAt ? Math.max(0, Math.round((now - lastInputAt) / MINUTE)) : null,
        active: transcriptAt !== null && now - transcriptAt < ACTIVE_WINDOW_MS,
      });
    }
  }
  return rows;
}

// ---------------------------------------------------------------------------
// Inventory: the one picture the model works from.
// ---------------------------------------------------------------------------

export function buildInventory({ paths = cleanupPaths(), alive = pidAlive, now = Date.now(), selfPid = process.pid } = {}) {
  const machine = readMachine(paths.procRoot);
  const processes = readProcesses(paths.procRoot);
  const rows = readAgentRows({ ...paths, alive, now });
  const agentByPid = new Map();
  const agentBySession = new Map();
  for (const row of rows) {
    if (row.live) agentByPid.set(row.pid, row);
    if (row.sessionId) agentBySession.set(row.sessionId, row);
  }
  const owners = new Map(); // pid -> agent row whose process tree it is in
  for (const row of rows) {
    if (!row.live) continue;
    for (const pid of descendants(processes, row.pid)) owners.set(pid, row);
  }
  const ownerOf = (proc) => {
    // A Claude agent team's members run beside their lead, not under it.
    const parent = /--parent-session-id\s+(\S+)/.exec(proc.cmdline)?.[1];
    return owners.get(proc.pid) ?? (parent ? agentBySession.get(parent) : undefined) ?? null;
  };
  const treeRss = (pid) => [pid, ...descendants(processes, pid)].reduce((sum, p) => sum + (processes.get(p)?.rssMb ?? 0), 0);

  const agents = rows
    .filter((row) => row.live)
    .map((row) => ({
      project: row.project,
      runId: row.runId,
      title: row.title,
      status: row.status,
      mode: row.mode,
      backend: row.backend,
      orchestrator: row.orchestrator || undefined,
      asks: row.asks || undefined,
      rssMb: treeRss(row.pid),
      idleMinutes: row.idleMinutes,
      lastInputMinutesAgo: row.lastInputMinutesAgo,
    }))
    .sort((a, b) => b.rssMb - a.rssMb);

  const protectedPids = ancestors(processes, selfPid).add(selfPid);
  const uid = typeof process.getuid === "function" ? process.getuid() : null;
  const leftovers = [...processes.values()]
    .filter((proc) => !protectedPids.has(proc.pid) && !isInfrastructure(proc) && !agentByPid.has(proc.pid))
    .filter((proc) => uid === null || proc.uid === uid)
    .filter((proc) => proc.rssMb >= 40)
    .map((proc) => {
      const owner = ownerOf(proc);
      return {
        pid: proc.pid,
        command: proc.cmdline.slice(0, 160) || proc.comm,
        cwd: proc.cwd,
        ageMinutes: Math.round(proc.ageMs / MINUTE),
        rssMb: proc.rssMb,
        agentCli: isAgentCli(proc) || undefined,
        owner: owner ? `${owner.project}/${owner.runId}` : null,
        ownerStatus: owner ? owner.status : null,
        ownerIdleMinutes: owner ? owner.idleMinutes : null,
      };
    })
    .sort((a, b) => b.rssMb - a.rssMb)
    .slice(0, 30);

  return {
    machine,
    agents,
    asleep: rows.filter((row) => !row.live && row.parked).length,
    processes: leftovers,
    totals: {
      processes: processes.size,
      agentClis: [...processes.values()].filter(isAgentCli).length,
    },
  };
}

// ---------------------------------------------------------------------------
// Acting: the guarded commands.
// ---------------------------------------------------------------------------

/** Why `pid` must not be stopped, or null when it may. */
export function stopRefusal(pid, { paths = cleanupPaths(), alive = pidAlive, now = Date.now(), selfPid = process.pid } = {}) {
  const processes = readProcesses(paths.procRoot);
  const proc = processes.get(pid);
  if (!proc) return "no such process";
  if (ancestors(processes, selfPid).has(pid) || pid === selfPid) return "that is the cleanup itself";
  if (isInfrastructure(proc)) return "that is part of the machine (supervisor, dashboard, tailscale)";
  const uid = typeof process.getuid === "function" ? process.getuid() : null;
  if (uid !== null && proc.uid !== uid) return "not our process";
  if (proc.ageMs < STOP_MIN_AGE_MS) return `it has only run for ${Math.round(proc.ageMs / MINUTE)} minutes`;
  const rows = readAgentRows({ ...paths, alive, now });
  if (rows.some((row) => row.live && row.pid === pid)) return "that is an agent: put it to sleep with park instead";
  for (const row of rows) {
    if (!row.live || !(row.active || row.status === "working" || row.status === "waiting")) continue;
    const parent = /--parent-session-id\s+(\S+)/.exec(proc.cmdline)?.[1];
    if (descendants(processes, row.pid).has(pid) || (parent && parent === row.sessionId)) {
      return `agent ${row.project}/${row.runId} is still active and owns it`;
    }
  }
  return null;
}

export async function stopProcess(pid, reason, { paths = cleanupPaths(), log = appendAction, kill = process.kill.bind(process) } = {}) {
  const refusal = stopRefusal(pid, { paths });
  if (refusal) {
    return { ok: false, pid, refused: refusal };
  }
  const processes = readProcesses(paths.procRoot);
  const proc = processes.get(pid);
  const targets = [pid, ...descendants(processes, pid)];
  for (const target of targets) {
    try { kill(target, "SIGTERM"); } catch { /* already gone */ }
  }
  await new Promise((resolve) => setTimeout(resolve, 5000));
  for (const target of targets) {
    try { kill(target, 0); kill(target, "SIGKILL"); } catch { /* exited */ }
  }
  log(paths, { action: "stop", pid, command: proc.cmdline.slice(0, 200), rssMb: proc.rssMb, reason });
  return { ok: true, pid, stopped: targets.length, freedMb: targets.reduce((sum, p) => sum + (processes.get(p)?.rssMb ?? 0), 0) };
}

export async function parkAgent(project, runId, reason, { paths = cleanupPaths(), log = appendAction, waitMs = 15_000 } = {}) {
  if (!/^[A-Za-z0-9._-]+$/.test(project) || !/^[A-Za-z0-9._-]+$/.test(runId)) {
    return { ok: false, refused: "bad project or run id" };
  }
  const repoDir = path.join(paths.codeDir, project);
  if (!fs.existsSync(path.join(repoDir, ".rudder", "runs", runId, "run.json"))) {
    return { ok: false, refused: `no agent ${runId} in ${project}` };
  }
  const instruction = String(reason || "").trim().slice(0, 240) || "idle";
  const requestId = queueSteer(repoDir, { kind: "park", taskId: runId, instruction });
  const deadline = Date.now() + waitMs;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 300));
    const receipt = readSteerReceipt(repoDir, requestId);
    if (receipt?.status === "delivered") {
      log(paths, { action: "park", project, runId, reason: instruction });
      return { ok: true, project, runId, asleep: true };
    }
    if (receipt?.status === "failed") {
      return { ok: false, project, runId, refused: receipt.error || "the dashboard refused" };
    }
  }
  return { ok: false, project, runId, refused: "the dashboard did not answer (is it running?)" };
}

/** What the audit log says was done since `since` (ISO): {park, stop}. */
export function actionsSince(paths, since) {
  const counts = { park: 0, stop: 0 };
  for (const line of readText(path.join(paths.rudderHome, "cleanup", "actions.jsonl")).split("\n").slice(-500)) {
    try {
      const entry = JSON.parse(line);
      if (entry.ts >= since && entry.action in counts) counts[entry.action]++;
    } catch {
      // a torn line
    }
  }
  return counts;
}

function appendAction(paths, entry) {
  try {
    const dir = path.join(paths.rudderHome, "cleanup");
    fs.mkdirSync(dir, { recursive: true });
    fs.appendFileSync(path.join(dir, "actions.jsonl"), `${JSON.stringify({ ts: new Date().toISOString(), ...entry })}\n`);
  } catch {
    // the audit line is a nicety; the act already happened
  }
}

// ---------------------------------------------------------------------------
// The job: a small model, once an hour or when memory is short.
// ---------------------------------------------------------------------------

export function cleanupPrompt({ cli = CLI_PATH, trigger = "hourly" } = {}) {
  const run = `node ${cli}`;
  return `You are the cleanup job on a Rudder cloud machine: one Linux VM, no swap, that runs every project's coding agents for one person. When its memory runs out it thrashes and every terminal attached to it drops. Your job is to give memory back from things nobody is using, and nothing else.${trigger === "memory" ? " You were started early because memory is short right now." : ""}

Your only tool is this command. Call it exactly like this, one command per call, no pipes or other commands:
  ${run} inventory                          what is running now (JSON)
  ${run} park <project> <runId> "<reason>"   put an idle agent to sleep
  ${run} stop <pid> "<reason>"               stop a leftover process

Putting an agent to sleep ends its process but keeps its conversation and workspace; one message from the person wakes it. It is cheap to undo but it does close a chat someone may come back to, so:
- Put to sleep an agent idle (idleMinutes) for at least 180 minutes.
- When memory is short (memAvailableMb under a quarter of memTotalMb, or memoryPressure above 10), also put to sleep agents idle for at least 30 minutes, largest rssMb first, until about a third of memory is free.
- Never an agent whose status is working or waiting, that has asks, or that got a message (lastInputMinutesAgo) in the last 60 minutes.
- The dashboard refuses anything that may still be working. If it refuses, accept that and move on.

Stop a process only when it is a leftover: not an agent (agentCli), its owner is null or idle (ownerIdleMinutes) for at least 120 minutes, it has run for at least 120 minutes, and it is the kind of thing an agent starts and forgets: a dev server, a file watcher, a test runner, a build that is not finishing, a headless browser, a member of an agent team (a claude process with --agent-id) whose lead agent is asleep or gone. When unsure, leave it.

Reasons are shown to the person on the agent's card, so write them for a person: "idle 5 hours", "idle 40 minutes while memory was short".

Do not read files, run any other command, or work around a refusal. Start with inventory. End by running inventory again, then reply with a short plain report: what you put to sleep or stopped and why, available memory before and after, and anything that looked wrong that you left alone. If there was nothing to do, say so in one line.`;
}

/**
 * Schedule the job: the first run some minutes after boot (after the boot
 * reopen has settled), then every `intervalMs`, plus an early run when memory
 * is short (at most every `pressureCooldownMs`). `RUDDER_HOME_CLEANUP=0` turns
 * it off. One run at a time.
 */
export function createCleanupJob({
  env = process.env,
  log = () => {},
  spawnImpl = spawn,
  model = (env.RUDDER_HOME_CLEANUP_MODEL || "claude-sonnet-5-5").trim(),
  intervalMs = Number(env.RUDDER_HOME_CLEANUP_MS) || 60 * MINUTE,
  firstRunMs = 10 * MINUTE,
  pressureCheckMs = MINUTE,
  pressureCooldownMs = 15 * MINUTE,
  timeoutMs = 10 * MINUTE,
  paths = cleanupPaths(env),
  onRun = () => {},
} = {}) {
  let running = false;
  let lastRunAt = 0;
  const timers = [];

  function runNow(trigger) {
    if (running) return false;
    running = true;
    lastRunAt = Date.now();
    const startedAt = new Date().toISOString();
    const before = safe(() => readMachine(paths.procRoot));
    log(`cleanup: starting (${trigger}; ${before?.memAvailableMb ?? "?"} of ${before?.memTotalMb ?? "?"} MB free)`);
    const args = [
      "-p", cleanupPrompt({ trigger }),
      "--model", model,
      // None of the person's hooks, plugins or MCP servers: this is the machine
      // tidying itself, not an agent of theirs.
      "--setting-sources", "",
      "--strict-mcp-config",
      "--disable-slash-commands",
      "--tools", "Bash",
      "--allowedTools", `Bash(node ${CLI_PATH} *)`,
      "--max-turns", "40",
    ];
    const workDir = path.join(paths.rudderHome, "cleanup");
    try { fs.mkdirSync(workDir, { recursive: true }); } catch { /* spawn reports it */ }
    let child;
    try {
      child = spawnImpl("claude", args, { cwd: workDir, env: { ...env, CLAUDE_CODE_NO_FLICKER: "0" }, stdio: ["ignore", "pipe", "pipe"] });
    } catch (error) {
      running = false;
      log(`cleanup: could not start claude: ${error.message}`);
      return false;
    }
    let out = "";
    let err = "";
    child.stdout?.on("data", (chunk) => { out = (out + chunk).slice(-20_000); });
    child.stderr?.on("data", (chunk) => { err = (err + chunk).slice(-4_000); });
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try { child.kill("SIGKILL"); } catch { /* gone */ }
    }, timeoutMs);
    timer.unref?.();
    const finish = (code) => {
      clearTimeout(timer);
      running = false;
      safe(() => onRun({ trigger, outcome: timedOut ? "timeout" : code === 0 ? "ok" : "failed", actions: actionsSince(paths, startedAt) }));
      const after = safe(() => readMachine(paths.procRoot));
      const report = out.trim() || err.trim() || `claude exited ${code}`;
      for (const line of report.split("\n").slice(0, 40)) {
        if (line.trim()) log(`cleanup: ${line}`);
      }
      log(`cleanup: done (exit ${code}; ${after?.memAvailableMb ?? "?"} MB free)`);
      safe(() => fs.writeFileSync(path.join(workDir, "last.json"), JSON.stringify({
        trigger, model, startedAt, finishedAt: new Date().toISOString(), exitCode: code,
        memAvailableMbBefore: before?.memAvailableMb ?? null, memAvailableMbAfter: after?.memAvailableMb ?? null,
        report,
      }, null, 2)));
    };
    child.on("error", (error) => { err += error.message; });
    child.on("close", finish);
    return true;
  }

  function start() {
    if (env.RUDDER_HOME_CLEANUP === "0") {
      log("cleanup: off (RUDDER_HOME_CLEANUP=0)");
      return;
    }
    const first = setTimeout(() => runNow("hourly"), firstRunMs);
    const hourly = setInterval(() => runNow("hourly"), intervalMs);
    const pressure = setInterval(() => {
      const machine = safe(() => readMachine(paths.procRoot));
      if (machine && memoryIsShort(machine) && Date.now() - lastRunAt >= pressureCooldownMs) {
        runNow("memory");
      }
    }, pressureCheckMs);
    for (const timer of [first, hourly, pressure]) {
      timer.unref?.();
      timers.push(timer);
    }
  }

  function stop() {
    for (const timer of timers.splice(0)) clearTimeout(timer);
  }

  return { start, stop, runNow, get running() { return running; } };
}

// ---------------------------------------------------------------------------

function readText(file) {
  try {
    return fs.readFileSync(file, "utf8");
  } catch {
    return "";
  }
}

function toMillis(value) {
  if (value === null || value === undefined || value === "") return null;
  const number = Number(value);
  if (Number.isFinite(number) && number > 0) return number;
  const parsed = Date.parse(String(value));
  return Number.isFinite(parsed) ? parsed : null;
}

function safe(fn) {
  try {
    return fn();
  } catch {
    return null;
  }
}

async function main(argv) {
  const [command, ...rest] = argv;
  let result;
  if (command === "inventory") {
    result = buildInventory();
  } else if (command === "park" && rest.length >= 2) {
    result = await parkAgent(rest[0], rest[1], rest.slice(2).join(" "));
  } else if (command === "stop" && /^\d+$/.test(rest[0] ?? "")) {
    result = await stopProcess(Number(rest[0]), rest.slice(1).join(" "));
  } else {
    process.stderr.write("usage: cleanup.mjs inventory | park <project> <runId> \"<reason>\" | stop <pid> \"<reason>\"\n");
    process.exit(2);
  }
  process.stdout.write(`${JSON.stringify(result, null, 1)}\n`);
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fs.realpathSync(CLI_PATH)) {
  await main(process.argv.slice(2));
}
