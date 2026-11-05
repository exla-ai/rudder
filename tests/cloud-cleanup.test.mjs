// The home machine's cleanup (cloud/worker/cleanup.mjs): the inventory the
// model works from, and the guards on the only two things it can do. Runs
// against a fake /proc and a fake ~/code, so it needs no Linux and no model.
import { test } from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import {
  actionsSince,
  buildInventory,
  cleanupPrompt,
  createCleanupJob,
  memoryIsShort,
  parkAgent,
  readMachine,
  stopRefusal,
  sweepScratch,
} from "../cloud/worker/cleanup.mjs";

const MIN = 60_000;
const NOW = Date.parse("2026-11-02T22:00:00Z");
const UPTIME_SEC = 100_000;
const uid = process.getuid();

function fakeMachine() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-cleanup-"));
  const proc = path.join(root, "proc");
  const home = path.join(root, "home");
  const volume = path.join(root, "workspace");
  fs.mkdirSync(path.join(proc, "pressure"), { recursive: true });
  fs.writeFileSync(path.join(proc, "uptime"), `${UPTIME_SEC}.00 1.00\n`);
  fs.writeFileSync(path.join(proc, "loadavg"), "238.46 167.64 84.78 3/361 9221\n");
  fs.writeFileSync(path.join(proc, "meminfo"), "MemTotal:       16379904 kB\nMemFree:          631808 kB\nMemAvailable:    1365000 kB\nSwapTotal:             0 kB\n");
  fs.writeFileSync(path.join(proc, "pressure", "memory"), "some avg10=10.76 avg60=60.60 avg300=59.92 total=1\nfull avg10=6.66 avg60=54.43 avg300=53.24 total=1\n");
  fs.writeFileSync(path.join(proc, "pressure", "io"), "some avg10=12.69 avg60=69.41 avg300=67.73 total=1\nfull avg10=1.00 avg60=14.01 avg300=24.48 total=1\n");
  const paths = { home, codeDir: path.join(volume, "code"), rudderHome: path.join(home, ".rudder"), procRoot: proc };

  const addProcess = ({ pid, ppid, comm, cmdline, ageMin, rssMb, owner = uid, cwd = "/" }) => {
    const dir = path.join(proc, String(pid));
    fs.mkdirSync(dir, { recursive: true });
    const startTicks = Math.round((UPTIME_SEC - ageMin * 60) * 100);
    const fields = ["S", ppid, pid, pid, 0, -1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 20, 0, 1, 0, startTicks];
    fs.writeFileSync(path.join(dir, "stat"), `${pid} (${comm}) ${fields.join(" ")}\n`);
    fs.writeFileSync(path.join(dir, "status"), `Name:\t${comm}\nUid:\t${owner}\t${owner}\t${owner}\t${owner}\nVmRSS:\t${rssMb * 1024} kB\n`);
    fs.writeFileSync(path.join(dir, "cmdline"), cmdline.split(" ").join("\0"));
    fs.symlinkSync(cwd, path.join(dir, "cwd"));
  };

  const addRun = ({ project, runId, pid, status = "completed", sessionId, lastInputAt, transcriptAt, parked = null }) => {
    const runDir = path.join(paths.codeDir, project, ".rudder", "runs", runId);
    fs.mkdirSync(runDir, { recursive: true });
    const cwd = path.join(paths.codeDir, project);
    const record = {
      id: runId,
      task: `work in ${project}`,
      taskSummary: `${runId} title`,
      status,
      mode: "main",
      backend: "claude",
      createdAt: String(NOW - 3 * 24 * 60 * MIN),
      lastUserInputAt: String(lastInputAt),
      workspace: { enabled: false, path: cwd },
      session: { nativeSessionId: sessionId },
      ...(pid ? { process: { pid, owner: "native-pty" } } : {}),
      ...(parked ? { parked } : {}),
    };
    fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify(record));
    const transcriptDir = path.join(home, ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"));
    fs.mkdirSync(transcriptDir, { recursive: true });
    const transcript = path.join(transcriptDir, `${sessionId}.jsonl`);
    fs.writeFileSync(transcript, "{}\n");
    fs.utimesSync(transcript, new Date(transcriptAt), new Date(transcriptAt));
  };

  return { root, paths, addProcess, addRun };
}

// The machine from 2026-11-02: an idle agent for days, a working one, a dev
// server left by the idle one, a young test run, and the plumbing.
function scenario() {
  const machine = fakeMachine();
  const { addProcess, addRun } = machine;
  addProcess({ pid: 633, ppid: 1, comm: "node", cmdline: "node /opt/rudder-worker/home.mjs", ageMin: 4000, rssMb: 140 });
  addProcess({ pid: 2253, ppid: 633, comm: "rudder-native", cmdline: "/opt/rudder/dist/native/rudder-native", ageMin: 4000, rssMb: 180 });
  // idle 3 days, with an MCP server and a forgotten dev server under it
  addProcess({ pid: 4499, ppid: 2253, comm: "claude", cmdline: "claude --permission-mode bypassPermissions --resume edacf534", ageMin: 4000, rssMb: 280 });
  addProcess({ pid: 4500, ppid: 4499, comm: "npm exec offstage-mcp", cmdline: "npm exec offstage-mcp", ageMin: 4000, rssMb: 90 });
  addProcess({ pid: 4121, ppid: 4499, comm: "next-server", cmdline: "next-server (v16.2.2)", ageMin: 600, rssMb: 550 });
  addRun({ project: "libra", runId: "run-idle", pid: 4499, sessionId: "s-idle", lastInputAt: NOW - 3 * 24 * 60 * MIN, transcriptAt: NOW - 3 * 24 * 60 * MIN });
  // working right now, running a build
  addProcess({ pid: 32361, ppid: 2253, comm: "claude", cmdline: "claude --permission-mode bypassPermissions --model opus --session-id s-busy", ageMin: 400, rssMb: 430 });
  addProcess({ pid: 9392, ppid: 32361, comm: "rustc", cmdline: "rustc --crate-name rudder_engine", ageMin: 90, rssMb: 1000 });
  addRun({ project: "rudder", runId: "run-busy", pid: 32361, status: "running", sessionId: "s-busy", lastInputAt: NOW - 30 * MIN, transcriptAt: NOW - 1 * MIN });
  addProcess({ pid: 20680, ppid: 20666, comm: "2.1.314", cmdline: "/home/rudder/.local/share/claude/versions/2.1.314 --agent-id sources@session-s-busy --parent-session-id s-busy", ageMin: 400, rssMb: 270 });
  // a young test run nobody owns, and someone else's process
  addProcess({ pid: 27659, ppid: 1, comm: "node", cmdline: "node vitest run", ageMin: 6, rssMb: 160 });
  addProcess({ pid: 700, ppid: 1, comm: "dockerd", cmdline: "dockerd", ageMin: 4000, rssMb: 200, owner: 0 });
  // asleep already
  addRun({ project: "libra", runId: "run-asleep", sessionId: "s-asleep", lastInputAt: NOW - 9000 * MIN, transcriptAt: NOW - 9000 * MIN, parked: { at: String(NOW - 600 * MIN), reason: "idle 5 hours" } });
  const live = new Set([633, 2253, 4499, 4500, 4121, 32361, 9392, 20680, 27659, 700]);
  return { ...machine, alive: (pid) => live.has(pid) };
}

test("the inventory shows what holds memory, who owns each process, and how long each agent has been idle", () => {
  const { paths, alive, root } = scenario();
  const inventory = buildInventory({ paths, alive, now: NOW, selfPid: 999_999 });

  assert.equal(inventory.machine.memTotalMb, 15996);
  assert.equal(inventory.machine.memAvailableMb, 1333);
  assert.equal(inventory.machine.memoryPressure, 54.43);
  assert.equal(inventory.asleep, 1);
  assert.deepEqual(inventory.agents.map((agent) => agent.runId), ["run-busy", "run-idle"], "largest first; the asleep one is not listed as live");

  const idle = inventory.agents.find((agent) => agent.runId === "run-idle");
  assert.equal(idle.status, "done");
  assert.equal(idle.idleMinutes, 3 * 24 * 60);
  assert.equal(idle.rssMb, 280 + 90 + 550, "an agent's memory includes the tree under it");
  const busy = inventory.agents.find((agent) => agent.runId === "run-busy");
  assert.equal(busy.status, "working");
  assert.equal(busy.idleMinutes, 1);

  const byPid = new Map(inventory.processes.map((proc) => [proc.pid, proc]));
  assert.equal(byPid.get(4121).owner, "libra/run-idle");
  assert.equal(byPid.get(4121).ownerIdleMinutes, 3 * 24 * 60);
  assert.equal(byPid.get(20680).owner, "rudder/run-busy", "a team member belongs to its lead by session id");
  assert.equal(byPid.get(20680).agentCli, true);
  assert.equal(byPid.get(27659).owner, null);
  for (const pid of [633, 2253, 4499, 32361, 700]) {
    assert.ok(!byPid.has(pid), `pid ${pid} (supervisor, dashboard, agent CLI or not ours) is not offered as a leftover`);
  }
  fs.rmSync(root, { recursive: true, force: true });
});

test("only a leftover can be stopped: old, ours, not an agent, not owned by an active one", () => {
  const { paths, alive, root, addProcess } = scenario();
  // The cleanup's own command, running somewhere under the dev server's pid.
  addProcess({ pid: 5000, ppid: 4121, comm: "node", cmdline: "node cleanup.mjs stop 4121", ageMin: 0, rssMb: 10 });
  const opts = { paths, alive, now: NOW, selfPid: 999_999 };
  assert.equal(stopRefusal(4121, opts), null, "the idle agent's forgotten dev server may go");
  assert.match(stopRefusal(9392, opts), /still active/, "a working agent's build stays");
  assert.match(stopRefusal(20680, opts), /still active/, "a working agent's team member stays");
  assert.match(stopRefusal(27659, opts), /only run for 6 minutes/);
  assert.match(stopRefusal(4499, opts), /park instead/, "agents go to sleep through their dashboard, never kill");
  assert.match(stopRefusal(633, opts), /part of the machine/);
  assert.match(stopRefusal(2253, opts), /part of the machine/);
  assert.match(stopRefusal(700, opts), /not our process/);
  assert.match(stopRefusal(424242, opts), /no such process/);
  assert.match(stopRefusal(4121, { ...opts, selfPid: 5000 }), /cleanup itself/, "never the chain that runs the cleanup");
  fs.rmSync(root, { recursive: true, force: true });
});

test("park goes through the dashboard's steer inbox and reports its answer", async () => {
  const { paths, root } = scenario();
  const repo = path.join(paths.codeDir, "libra");
  const inbox = path.join(repo, ".rudder", "steer");
  const receipts = path.join(repo, ".rudder", "steer-receipts");
  // A stand-in dashboard: answers every park request, refusing run-busy.
  const answer = setInterval(() => {
    for (const name of fs.existsSync(inbox) ? fs.readdirSync(inbox) : []) {
      if (!name.endsWith(".json")) continue;
      const request = JSON.parse(fs.readFileSync(path.join(inbox, name), "utf8"));
      assert.equal(request.kind, "park");
      fs.mkdirSync(receipts, { recursive: true });
      const receipt = request.taskId === "run-idle"
        ? { requestId: request.requestId, status: "delivered" }
        : { requestId: request.requestId, status: "failed", error: "its pane printed something 12s ago" };
      fs.writeFileSync(path.join(receipts, `${request.requestId}.json`), JSON.stringify(receipt));
      fs.rmSync(path.join(inbox, name));
    }
  }, 50);
  const logged = [];
  const log = (_paths, entry) => logged.push(entry);
  try {
    const parked = await parkAgent("libra", "run-idle", "idle 3 days", { paths, log, waitMs: 3000 });
    assert.deepEqual(parked, { ok: true, project: "libra", runId: "run-idle", asleep: true });
    assert.deepEqual(logged, [{ action: "park", project: "libra", runId: "run-idle", reason: "idle 3 days" }]);

    fs.mkdirSync(path.join(repo, ".rudder", "runs", "run-other"), { recursive: true });
    fs.writeFileSync(path.join(repo, ".rudder", "runs", "run-other", "run.json"), "{}");
    const refused = await parkAgent("libra", "run-other", "idle", { paths, log, waitMs: 3000 });
    assert.equal(refused.ok, false);
    assert.match(refused.refused, /printed something/);
    assert.equal(logged.length, 1, "a refusal is not recorded as an action");

    assert.match((await parkAgent("libra", "../etc", "x", { paths, log })).refused, /bad project or run id/);
    assert.match((await parkAgent("libra", "run-missing", "x", { paths, log })).refused, /no agent/);
  } finally {
    clearInterval(answer);
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("memory is short when little is available or everything stalls on it", () => {
  const { paths, root } = scenario();
  assert.equal(memoryIsShort(readMachine(paths.procRoot)), true, "1.3 of 16 GB free with 54% full pressure");
  assert.equal(memoryIsShort({ memTotalMb: 16000, memAvailableMb: 8000, memoryPressure: 0 }), false);
  assert.equal(memoryIsShort({ memTotalMb: 16000, memAvailableMb: 1500, memoryPressure: 0 }), true);
  assert.equal(memoryIsShort({ memTotalMb: 16000, memAvailableMb: 8000, memoryPressure: 25 }), true);
  fs.rmSync(root, { recursive: true, force: true });
});

test("the job runs a small model limited to the cleanup command, one run at a time", async () => {
  const { paths, root } = scenario();
  const calls = [];
  const children = [];
  const spawnImpl = (command, args, options) => {
    const child = new EventEmitter();
    child.stdout = new EventEmitter();
    child.stderr = new EventEmitter();
    child.kill = () => {};
    calls.push({ command, args, options });
    children.push(child);
    return child;
  };
  const lines = [];
  const job = createCleanupJob({ env: { HOME: paths.home }, paths, spawnImpl, log: (line) => lines.push(line) });
  assert.equal(job.runNow("memory"), true);
  assert.equal(job.runNow("hourly"), false, "a second run waits for the first");

  const { command, args, options } = calls[0];
  assert.equal(command, "claude");
  assert.equal(args[args.indexOf("--model") + 1], "claude-sonnet-5-5");
  assert.equal(args[args.indexOf("--tools") + 1], "Bash");
  assert.match(args[args.indexOf("--allowedTools") + 1], /^Bash\(node \S+cleanup\.mjs \*\)$/);
  assert.equal(args[args.indexOf("--setting-sources") + 1], "", "none of the person's hooks or plugins");
  const prompt = args[args.indexOf("-p") + 1];
  assert.match(prompt, /started early because memory is short/);
  assert.match(prompt, /inventory/);
  assert.equal(options.cwd, path.join(paths.rudderHome, "cleanup"));

  children[0].stdout.emit("data", "Put libra/run-idle to sleep (idle 3 days). 1333 -> 2253 MB free.\n");
  children[0].emit("close", 0);
  assert.equal(job.running, false);
  const last = JSON.parse(fs.readFileSync(path.join(paths.rudderHome, "cleanup", "last.json"), "utf8"));
  assert.equal(last.trigger, "memory");
  assert.match(last.report, /run-idle/);
  assert.ok(lines.some((line) => line.includes("Put libra/run-idle to sleep")), lines.join("\n"));

  const off = createCleanupJob({ env: { RUDDER_HOME_CLEANUP: "0" }, paths, spawnImpl, log: (line) => lines.push(line) });
  off.start();
  assert.ok(lines.includes("cleanup: off (RUDDER_HOME_CLEANUP=0)"));
  assert.doesNotMatch(cleanupPrompt({ trigger: "hourly" }), /memory is short right now/);
  fs.rmSync(root, { recursive: true, force: true });
});

// Agents' scratch under /workspace/scratch/<run id> and top-level .tmp* dirs:
// fixed rules by run state and mtime, and nothing else on the volume.
test("scratch goes once its run is finished, deleted or stale; live, recent and every other top-level dir stays", async () => {
  const { root, paths, addProcess } = fakeMachine();
  const HOUR = 60 * MIN;
  const volume = path.dirname(paths.codeDir);
  const scratch = path.join(volume, "scratch");
  // Sets an mtime on a whole tree, deepest first (creating a child bumps its parent).
  const age = (file, ageMs) => {
    const stat = fs.lstatSync(file);
    if (stat.isDirectory()) for (const name of fs.readdirSync(file)) age(path.join(file, name), ageMs);
    const when = new Date(NOW - ageMs);
    fs.lutimesSync(file, when, when);
  };
  const dir = (rel, ageMs, files = { "tmp/out.log": "x".repeat(1000) }) => {
    const base = path.join(volume, rel);
    fs.mkdirSync(base, { recursive: true });
    for (const [name, text] of Object.entries(files)) {
      fs.mkdirSync(path.dirname(path.join(base, name)), { recursive: true });
      fs.writeFileSync(path.join(base, name), text);
    }
    age(base, ageMs);
    return base;
  };
  const run = (runId, status, updatedAgoMs, pid) => {
    const runDir = path.join(paths.codeDir, "libra", ".rudder", "runs", runId);
    fs.mkdirSync(runDir, { recursive: true });
    fs.writeFileSync(path.join(runDir, "run.json"), JSON.stringify({
      id: runId, status, updatedAt: new Date(NOW - updatedAgoMs).toISOString(), ...(pid ? { process: { pid } } : {}),
    }));
  };

  run("run-merged", "merged", 25 * HOUR);
  dir("scratch/run-merged", 1 * HOUR);
  run("run-done-recent", "completed", 2 * HOUR);
  dir("scratch/run-done-recent", 2 * HOUR);
  run("run-cancelled-live", "cancelled", 30 * HOUR, 4242);
  dir("scratch/run-cancelled-live", 30 * HOUR);
  run("run-completed-env", "completed", 48 * HOUR);
  dir("scratch/run-completed-env", 48 * HOUR);
  addProcess({ pid: 5100, ppid: 1, comm: "bash", cmdline: "bash", ageMin: 60, rssMb: 5 });
  fs.writeFileSync(path.join(paths.procRoot, "5100", "environ"), `PATH=/bin\0RUDDER_SCRATCH=${scratch}/run-completed-env\0`);
  dir("scratch/run-gone", 30 * HOUR);
  const fresh = dir("scratch/run-gone-fresh", 30 * HOUR);
  fs.utimesSync(path.join(fresh, "tmp", "out.log"), new Date(NOW - HOUR), new Date(NOW - HOUR));
  dir("scratch/run-cwd", 30 * HOUR);
  addProcess({ pid: 5200, ppid: 1, comm: "node", cmdline: "node server.js", ageMin: 60, rssMb: 50, cwd: path.join(scratch, "run-cwd", "tmp") });
  run("run-paused-old", "paused", 9 * 24 * HOUR);
  dir("scratch/run-paused-old", 8 * 24 * HOUR);
  run("run-paused-week", "paused", 6 * 24 * HOUR);
  dir("scratch/run-paused-week", 6 * 24 * HOUR);
  // A link in scratch goes as a link; what it points at stays.
  fs.symlinkSync(paths.codeDir, path.join(scratch, "link-out"));
  fs.lutimesSync(path.join(scratch, "link-out"), new Date(NOW - 30 * HOUR), new Date(NOW - 30 * HOUR));
  // Top level: only .tmp* directories, and only old ones.
  dir(".tmpQ1w2e3", 30 * HOUR);
  dir(".tmpNew", 1 * HOUR);
  fs.writeFileSync(path.join(volume, ".tmpfile"), "a file, not a dir");
  age(path.join(volume, ".tmpfile"), 30 * 24 * HOUR);
  fs.symlinkSync(path.join(volume, "cache"), path.join(volume, ".tmp-link"));
  const untouchable = ["rudder-a10-x9", "rudder-covered-ingress-1", "home", "cache", "home-cache", ".pnpm-store", "rudder-build"];
  for (const name of untouchable) dir(name, 30 * 24 * HOUR);
  age(path.join(volume, "code"), 30 * 24 * HOUR);

  const result = sweepScratch({ paths, alive: (pid) => pid === 4242 || pid === 5100 || pid === 5200, now: NOW });
  const gone = Object.fromEntries(result.removed.map((entry) => [path.relative(volume, entry.path), entry.reason]));
  assert.deepEqual(gone, {
    "scratch/run-merged": "run merged over 24 hours ago",
    "scratch/run-gone": "run deleted, untouched for 24 hours",
    "scratch/link-out": "run deleted, untouched for 24 hours",
    "scratch/run-paused-old": "untouched for 7 days",
    ".tmpQ1w2e3": "temp dir untouched for 24 hours",
  });
  assert.deepEqual(result.failed, []);
  for (const rel of ["scratch/run-done-recent", "scratch/run-cancelled-live", "scratch/run-completed-env", "scratch/run-gone-fresh",
    "scratch/run-cwd", "scratch/run-paused-week", ".tmpNew", ".tmpfile", ".tmp-link", "code/libra/.rudder/runs/run-merged/run.json", ...untouchable]) {
    assert.ok(fs.existsSync(path.join(volume, rel)) || fs.lstatSync(path.join(volume, rel)), `${rel} stays`);
  }
  assert.ok(result.freedBytes >= 4 * 1000, `freed ${result.freedBytes}`);
  assert.equal(result.freedBytes, result.removed.reduce((sum, entry) => sum + entry.bytes, 0));
  const audit = fs.readFileSync(path.join(paths.rudderHome, "cleanup", "actions.jsonl"), "utf8");
  assert.equal(audit.trim().split("\n").filter((line) => JSON.parse(line).action === "scratch").length, 5);
  assert.deepEqual(actionsSince(paths, new Date(0).toISOString()), { park: 0, stop: 0 }, "scratch is not a park or a stop");

  // Without a readable ~/code nothing counts as deleted; only the 7-day rule applies.
  dir("scratch/run-orphan", 30 * HOUR);
  fs.renameSync(paths.codeDir, `${paths.codeDir}.away`);
  assert.deepEqual(sweepScratch({ paths, alive: () => false, now: NOW }).removed.map((entry) => path.basename(entry.path)), []);
  fs.renameSync(`${paths.codeDir}.away`, paths.codeDir);

  // The hourly run sweeps and reports what it freed; a memory-short run does not touch the disk.
  const spawnImpl = () => {
    const child = new EventEmitter();
    child.stdout = new EventEmitter();
    child.stderr = new EventEmitter();
    child.kill = () => {};
    setImmediate(() => child.emit("close", 0));
    return child;
  };
  const lines = [];
  const runs = [];
  dir(".tmpOld2", 30 * HOUR);
  const job = createCleanupJob({ env: { HOME: paths.home }, paths, spawnImpl, log: (line) => lines.push(line), onRun: (entry) => runs.push(entry) });
  assert.equal(job.runNow("memory"), true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(fs.existsSync(path.join(volume, ".tmpOld2")));
  assert.equal(runs[0].scratchFreedBytes, 0);
  assert.equal(job.runNow("hourly"), true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(!fs.existsSync(path.join(volume, ".tmpOld2")));
  assert.ok(lines.some((line) => /^cleanup: scratch freed \d+ bytes \(\d+ removed\)$/.test(line)), lines.join("\n"));
  assert.ok(runs[1].scratchFreedBytes > 0);
  const last = JSON.parse(fs.readFileSync(path.join(paths.rudderHome, "cleanup", "last.json"), "utf8"));
  assert.equal(last.scratch.freedBytes, runs[1].scratchFreedBytes);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a detached worker on a Linux home gets its own scratch and TMPDIR; anywhere else, nothing", async () => {
  const { homeScratchEnv } = await import("../dist/home-scratch.js");
  const volume = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-scratch-env-"));
  const env = { RUDDER_CLOUD_RUNTIME: "home", RUDDER_HOME_VOLUME: volume };
  assert.deepEqual(homeScratchEnv("run-7f3a", env, "linux"), {
    RUDDER_SCRATCH: path.join(volume, "scratch", "run-7f3a"),
    TMPDIR: path.join(volume, "scratch", "run-7f3a", "tmp"),
  });
  assert.ok(fs.statSync(path.join(volume, "scratch", "run-7f3a", "tmp")).isDirectory());
  assert.deepEqual(homeScratchEnv("run-7f3a", env, "darwin"), {}, "a Mac keeps its own temp dir");
  assert.deepEqual(homeScratchEnv("run-7f3a", { RUDDER_HOME_VOLUME: volume }, "linux"), {}, "only on the cloud home");
  for (const bad of ["", "..", "../code", "a/b", ".hidden"]) {
    assert.deepEqual(homeScratchEnv(bad, env, "linux"), {}, JSON.stringify(bad));
  }
  fs.rmSync(volume, { recursive: true, force: true });
});
