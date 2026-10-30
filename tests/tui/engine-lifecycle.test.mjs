// Persistent ownership is tested with a real non-TTY service and real terminal
// views. The worker CLIs are disposable scripts; no model/auth/network calls.
import assert from "node:assert/strict";
import { execFile, spawn, spawnSync } from "node:child_process";
import fsp from "node:fs/promises";
import net from "node:net";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { launch } from "tui-integration-tests";
import { assertPrerequisites, nativeBin, pidAlive, removeScratch, repoRoot, scratchRepo, waitPidGone, waitUntil } from "./helpers.mjs";

assertPrerequisites();
const exec = promisify(execFile);

async function records(repo) {
  const root = path.join(repo, ".rudder", "runs");
  const names = await fsp.readdir(root).catch(() => []);
  const result = [];
  for (const name of names) {
    try { result.push(JSON.parse(await fsp.readFile(path.join(root, name, "run.json"), "utf8"))); } catch {}
  }
  return result;
}

function exited(child, timeout = 10_000) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve({ code: child.exitCode, signal: child.signalCode });
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("engine did not exit")), timeout);
    child.once("exit", (code, signal) => { clearTimeout(timer); resolve({ code, signal }); });
  });
}

async function fixture(t, backendBody) {
  const repo = await scratchRepo("rudder-engine-");
  const home = path.join(repo, ".engine-home");
  const bin = path.join(repo, "fake-bin");
  await fsp.mkdir(bin);
  const backend = path.join(bin, "claude");
  await fsp.writeFile(backend, backendBody ?? `#!/bin/sh
printf '%s\\n' "$$" > ENGINE_WORKER_PID
printf 'ENGINE_WORKER_READY\\n'
trap 'exit 0' HUP TERM INT
while :; do printf '.' >> ENGINE_HEARTBEAT; sleep 0.1; done
`, { mode: 0o755 });
  await fsp.copyFile(backend, path.join(bin, "codex"));
  const env = { ...process.env, RUDDER_HOME: home, RUDDER_OFFLINE: "1", RUDDER_TELEMETRY: "0", RUDDER_CLAUDE_EXECUTION_PROFILE: "native",
    RUDDER_TASK_ROUTING: "local", RUDDER_DISABLE_AUTO_UPDATE: "1", RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_CLI: path.join(repoRoot, "dist/index.js"), RUDDER_CLAUDE_BIN: backend, RUDDER_CODEX_BIN: backend,
    NO_COLOR: "", COLORTERM: "truecolor", TERM: "xterm-256color", PATH: `${bin}${path.delimiter}${process.env.PATH ?? ""}` };
  for (const name of ["RUDDER_ENGINE", "RUDDER_ATTACH_SOCKET", "RUDDER_CLOUD_WORKER", "RUDDER_CLOUD_RUNTIME", "RUDDER_WORKSPACE_ID", "RUDDER_WORKER_TOKEN", "RUDDER_SAIL_ID"]) delete env[name];
  const processes = [];
  const views = [];
  const state = { repo, env, processes, views };
  state.native = async (operation) => {
    const result = await exec(nativeBin, ["engine", operation, "--repo", repo, "--json"], { env, cwd: repo, timeout: 8_000 });
    return JSON.parse(result.stdout);
  };
  state.boot = async () => {
    const child = spawn(nativeBin, ["engine", "--repo", repo], { env, cwd: repo, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    child.stderr.on("data", (value) => { stderr = (stderr + value).slice(-32_768); });
    processes.push(child);
    const ready = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`engine readiness timed out: ${stderr}`)), 10_000);
      child.once("error", (error) => { clearTimeout(timer); reject(error); });
      child.once("exit", (code) => { clearTimeout(timer); reject(new Error(`engine exited ${code} before readiness: ${stderr}`)); });
      child.stdout.on("data", (value) => {
        stdout += value;
        if (stdout.includes("\n")) {
          try { const result = JSON.parse(stdout.split("\n")[0]); clearTimeout(timer); resolve(result); }
          catch (error) { clearTimeout(timer); reject(new Error(`non-JSON readiness: ${stdout}`, { cause: error })); }
        }
      });
    });
    assert.equal(ready.running, true);
    assert.equal(ready.authority, "fabric");
    assert.equal(ready.implementation, "legacy-native-extraction");
    assert.ok(ready.owner.epoch >= 1);
    assert.ok(!stdout.includes("\x1b"), "headless service never initializes terminal/raw output");
    return { child, ready };
  };
  state.view = async (args = ["attach", "--dashboard"]) => {
    const view = await launch({ binary: nativeBin, args, cwd: repo, env, cols: 120, rows: 40 });
    views.push(view);
    return view;
  };
  t.after(async () => {
    for (const view of views) await view.close().catch(() => {});
    await state.native("shutdown").catch(() => {});
    for (const child of processes) {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGTERM");
        await exited(child, 6_000).catch(() => { child.kill("SIGKILL"); });
      }
    }
    await removeScratch(repo);
  });
  return state;
}

async function startWorker(state, view, task) {
  await view.press("Ctrl+W");
  await view.press("3");
  await view.type(task);
  await view.press("Enter");
  await view.waitForText("ENGINE_WORKER_READY", { timeout: 30_000 });
  let worker;
  await waitUntil(async () => {
    worker = (await records(state.repo)).find((record) => record.task.includes(task) && record.workspace?.path);
    return worker && (await fsp.readFile(path.join(worker.workspace.path, "ENGINE_WORKER_PID"), "utf8").catch(() => "")).trim();
  }, { timeout: 10_000, label: "the actual worker PID and workspace persisted" });
  const pid = Number((await fsp.readFile(path.join(worker.workspace.path, "ENGINE_WORKER_PID"), "utf8")).trim());
  assert.ok(pidAlive(pid));
  return { record: worker, pid, heartbeat: path.join(worker.workspace.path, "ENGINE_HEARTBEAT") };
}

async function rawControl(socket, request) {
  return new Promise((resolve, reject) => {
    const stream = net.createConnection(socket);
    let body = "";
    stream.setTimeout(3000, () => { stream.destroy(); reject(new Error("control response timed out")); });
    stream.once("error", reject);
    stream.once("connect", () => stream.write(`${JSON.stringify(request)}\n`));
    stream.on("data", (chunk) => {
      body += chunk;
      if (body.includes("\n")) { stream.destroy(); resolve(JSON.parse(body.split("\n")[0])); }
    });
  });
}

test("headless engine keeps the same PTY across closed views and pauses on explicit shutdown", { timeout: 120_000 }, async (t) => {
  const state = await fixture(t);
  const { child, ready } = await state.boot();
  const first = await state.view();
  await first.waitForText("Type a task", { timeout: 15_000 });
  const worker = await startWorker(state, first, "implement the persistent service fixture");
  const ownerRecord = JSON.parse(await fsp.readFile(path.join(state.repo, ".rudder/engine/owner.json"), "utf8"));

  // Normal surface close never asks the service or agent to quit.
  await first.press("Ctrl+W");
  await first.press("q");
  await first.waitForExit({ timeout: 10_000 });
  const before = (await fsp.stat(worker.heartbeat)).size;
  await waitUntil(async () => (await fsp.stat(worker.heartbeat)).size > before + 2, { label: "worker makes progress with zero views" });
  const status = await state.native("status");
  assert.equal(status.instance_id, ready.instance_id);
  assert.equal(status.owner.epoch, ready.owner.epoch);
  assert.equal(status.live_ptys, 1);
  assert.ok(!JSON.stringify(status).includes(ownerRecord.control_nonce), "status never reveals control credentials");

  const second = await state.view();
  await second.waitFor((screen) => screen.includes("persistent") && screen.includes("service fixture") && screen.includes("ENGINE_WORKER_READY"), {
    timeout: 15_000, label: "the existing worker and wrapped task title reattach",
  });
  assert.ok(pidAlive(worker.pid), "reattach retains the same backend process");
  await second.kill(); // Abrupt view death must have the same ownership semantics.
  await waitUntil(async () => (await state.native("status")).attached_views === 0, { label: "disconnected views removed" });
  assert.ok(pidAlive(worker.pid));

  const third = await state.view();
  await third.waitFor((screen) => screen.includes("persistent") && screen.includes("service fixture") && screen.includes("ENGINE_WORKER_READY"), {
    timeout: 15_000, label: "the same worker reattaches after abrupt surface death",
  });
  assert.equal((await records(state.repo)).length, 1, "reattachment never relaunches the task");
  const stopping = await state.native("shutdown");
  assert.equal(stopping.state, "stopping");
  assert.deepEqual(await exited(child), { code: 0, signal: null });
  await waitPidGone(worker.pid, "explicitly paused engine worker");
  const [paused] = await records(state.repo);
  assert.equal(paused.status, "paused");
  assert.ok(!paused.process, "paused record never advertises a live owned process");
  assert.match(paused.turns.at(-1).prompt, /persistent service fixture/);
  assert.equal((await state.native("status")).running, false);

  const next = await state.boot();
  assert.ok(next.ready.owner.epoch > ready.owner.epoch);
  assert.notEqual(next.ready.instance_id, ready.instance_id);
  assert.equal((await state.native("status")).live_ptys, 0, "a paused turn is not blindly relaunched");
  const stale = await rawControl(next.ready.control_socket, { format_version: 1, op: "shutdown",
    instance_id: ready.instance_id, epoch: ready.owner.epoch, nonce: ownerRecord.control_nonce });
  assert.equal(stale.ok, false, "stale control identity cannot terminate the new owner");
  assert.equal((await state.native("status")).running, true);
});

test("legacy dashboard and fabric engine cannot own one checkout concurrently", { timeout: 90_000 }, async (t) => {
  const state = await fixture(t);
  const legacy = await state.view([]);
  await legacy.waitForText("Type a task", { timeout: 15_000 });
  let status;
  await waitUntil(async () => { status = await state.native("status"); return status.running; });
  assert.equal(status.authority, "legacy-native");
  const attempt = spawnSync(nativeBin, ["engine", "--repo", state.repo], { env: state.env, encoding: "utf8", timeout: 5000 });
  assert.notEqual(attempt.status, 0);
  assert.match(attempt.stderr, /another Rudder owner|live legacy dashboard/);
  assert.equal((await state.native("status")).owner.epoch, status.owner.epoch);
  await legacy.press("Ctrl+C");
  await legacy.waitForExit({ timeout: 10_000 });

  const { ready } = await state.boot();
  assert.ok(ready.owner.epoch > status.owner.epoch, "explicit close and journal CAS enable migration");
  const duplicate = spawnSync(nativeBin, ["engine", "--repo", state.repo], { env: state.env, encoding: "utf8", timeout: 5000 });
  assert.notEqual(duplicate.status, 0);
  assert.match(duplicate.stderr, /another Rudder owner/);
  assert.equal((await state.native("status")).instance_id, ready.instance_id);
});

test("headless view confirms deletion after rendering and SIGTERM persists a resumable pause", { timeout: 120_000 }, async (t) => {
  const state = await fixture(t);
  const { child } = await state.boot();
  const view = await state.view();
  await view.waitForText("Type a task", { timeout: 15_000 });
  const removed = await startWorker(state, view, "implement the delete fixture");
  await view.press("Ctrl+W");
  await view.press("1");
  await view.press("d");
  await view.waitForText("press d again to confirm", { timeout: 10_000 });
  await view.press("d");
  await view.waitForGone("delete fixture", { timeout: 20_000 });
  await waitUntil(async () => {
    try { await fsp.access(removed.record.workspace.path); return false; }
    catch (error) { if (error.code === "ENOENT") return true; throw error; }
  }, { timeout: 20_000, label: "the confirmed worker's actual workspace is removed" });
  await view.waitForText("deleted agent and removed workspace", { timeout: 10_000 });
  await waitPidGone(removed.pid, "deleted worker");
  assert.equal((await state.native("status")).running, true);

  const live = await startWorker(state, view, "implement the signal shutdown fixture");
  child.kill("SIGTERM"); // Only this fixture's known child, never a saved PID.
  assert.deepEqual(await exited(child), { code: 0, signal: null });
  await waitPidGone(live.pid, "signal-paused worker");
  const paused = (await records(state.repo)).find((record) => record.id === live.record.id);
  assert.equal(paused.status, "paused");
  assert.ok(!paused.process);
  assert.equal((await state.native("status")).running, false);
});

test("fabric migration refuses a live detached writer and scheduler without stopping them", { timeout: 60_000 }, async (t) => {
  const state = await fixture(t);
  const legacy = spawn("/bin/sh", ["-c", "exec sleep 60"], { cwd: state.repo, stdio: "ignore" });
  t.after(async () => { if (legacy.exitCode === null && legacy.signalCode === null) legacy.kill("SIGTERM"); await exited(legacy).catch(() => {}); });
  const run = path.join(state.repo, ".rudder/runs/legacy-ts/run.json");
  await fsp.mkdir(path.dirname(run), { recursive: true });
  const original = JSON.stringify({ id: "legacy-ts", status: "running", process: { pid: legacy.pid, controllerPid: legacy.pid } });
  await fsp.writeFile(run, original);
  const invoke = () => spawnSync(nativeBin, ["engine", "--repo", state.repo], { env: state.env, encoding: "utf8", timeout: 5000 });
  const writer = invoke();
  assert.notEqual(writer.status, 0);
  assert.match(writer.stderr, /live legacy writer/);
  assert.equal(await fsp.readFile(run, "utf8"), original, "migration never relabels the existing run");
  assert.ok(pidAlive(legacy.pid), "migration never signals the existing writer");

  await fsp.rm(run);
  const daemon = path.join(state.repo, ".rudder/daemon.pid");
  await fsp.writeFile(daemon, JSON.stringify({ pid: legacy.pid, controlMode: "scheduler" }));
  const scheduler = invoke();
  assert.notEqual(scheduler.status, 0);
  assert.match(scheduler.stderr, /live legacy scheduler/);
  assert.ok(pidAlive(legacy.pid));

  await fsp.writeFile(daemon, JSON.stringify({ pid: legacy.pid, controlMode: "projector" }));
  const active = await state.boot();
  assert.equal(active.ready.running, true, "read-only projector metadata does not create another scheduler");
  assert.ok(pidAlive(legacy.pid));
});

test("managed agent MCP binds before spawn and refreshes peer grants in the same live attempt", { timeout: 75_000 }, async (t) => {
  const state = await fixture(t, `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const {spawn} = require('node:child_process');
const readline = require('node:readline');
if(process.argv.includes('--version')) {console.log('2.1.300');process.exit(0);}
fs.writeFileSync('ENGINE_WORKER_PID',String(process.pid));
const args=process.argv.slice(2);const config=JSON.parse(fs.readFileSync(args[args.indexOf('--mcp-config')+1],'utf8'));
const server=config.mcpServers.rudder_agents;
const mcp=spawn(server.command,server.args,{stdio:['pipe','pipe','pipe']});
let next=0;const waiting=new Map();
readline.createInterface({input:mcp.stdout}).on('line',line=>{let msg;try{msg=JSON.parse(line);}catch{return;}const pair=waiting.get(msg.id);if(pair){waiting.delete(msg.id);pair(msg);}});
const rpc=(method,params)=>new Promise(resolve=>{const id=++next;waiting.set(id,resolve);mcp.stdin.write(JSON.stringify({jsonrpc:'2.0',id,method,params})+'\\n');});
let busy=false;
async function tick(){if(busy)return;busy=true;try{const response=await rpc('tools/call',{name:'agents_find',arguments:{}});const identity=JSON.parse(fs.readFileSync(process.env.RUDDER_FABRIC_IDENTITY,'utf8'));fs.writeFileSync('ENGINE_MCP_PROOF.json',JSON.stringify({run_id:identity.run_id,run_epoch:identity.run_epoch,owner_epoch:identity.owner_epoch,attempt:process.env.RUDDER_FABRIC_ATTEMPT_ID,result:response.result}));}finally{busy=false;}}
(async()=>{await rpc('initialize',{protocolVersion:'2024-11-05',clientInfo:{name:'real-managed-launch-fixture',version:'1'},capabilities:{}});mcp.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized'})+'\\n');await tick();console.log('ENGINE_WORKER_READY');setInterval(()=>{fs.appendFileSync('ENGINE_HEARTBEAT','.');void tick();},500);})().catch(error=>{console.error(error.message);process.exit(1);});
process.on('SIGTERM',()=>{mcp.kill();process.exit(0);});
`);
  await state.boot();
  const view = await state.view();
  const first = await startWorker(state, view, "implement one managed peer");
  const second = await startWorker(state, view, "implement another managed peer");
  const proof = async (worker) => JSON.parse(await fsp.readFile(path.join(worker.record.workspace.path, "ENGINE_MCP_PROOF.json"), "utf8"));
  await waitUntil(async () => {
    const one = await proof(first).catch(() => null);
    const two = await proof(second).catch(() => null);
    return !one?.result?.isError && !two?.result?.isError && one?.result?.structuredContent?.agents?.length === 2 && two?.result?.structuredContent?.agents?.length === 2;
  }, { timeout: 15_000, label: "live MCP peers observe reminted exact-epoch grants" });
  for (const worker of [first, second]) {
    const observed = await proof(worker);
    assert.equal(observed.run_id, worker.record.id);
    assert.ok(observed.run_epoch >= 1);
    assert.match(observed.attempt, /^[a-f0-9-]{36}$/);
    assert.deepEqual(new Set(observed.result.structuredContent.agents.map((peer) => peer.run_id)), new Set([first.record.id, second.record.id]));
    assert.equal(observed.result.structuredContent.agents.find((peer) => peer.run_id === worker.record.id).run_epoch, observed.run_epoch);
    assert.ok(pidAlive(worker.pid));
  }
});
