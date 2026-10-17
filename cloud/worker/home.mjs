#!/usr/bin/env node
// Rudder home machine supervisor.
//
// One long-lived machine per account holds every repo under ~/code (cloned from
// GitHub, which stays the source of truth) and runs one Rudder dashboard per
// repo. This process:
//   1. writes the vault (credentials, config) and persists agent state on the
//      volume so conversations survive a stop/start,
//   2. joins the user's tailnet (when the vault has TS_AUTHKEY) and publishes
//      any dev server an agent starts as an HTTPS preview,
//   3. keeps one control socket to the control plane: the repo list comes down,
//      project state goes up, and the phone app's requests arrive as RPCs,
//   4. bridges each repo's dashboard PTY to its workspace channel, so
//      `rudder cloud` in that repo on a laptop attaches to it.

import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { StringDecoder } from "node:string_decoder";
import { WebSocket } from "ws";
import pty from "node-pty";

import { readTranscript, lastAssistantText, userTask } from "./transcripts.mjs";
import { readAsks, askDecision, isAskId } from "./asks.mjs";
import { sanitizeHomeHookConfigs } from "./hook-paths.mjs";

const cloudUrl = (process.env.RUDDER_CLOUD_URL || "").trim().replace(/\/$/, "");
const homeId = (process.env.RUDDER_WORKSPACE_ID || "").trim();
const workerToken = (process.env.RUDDER_WORKER_TOKEN || "").trim();
const flyMachineId = (process.env.FLY_MACHINE_ID || "").trim();

// Overridable so the boot can be tested off a real machine.
const VOLUME = process.env.RUDDER_HOME_VOLUME || "/workspace";
const CODE_DIR = path.join(VOLUME, "code");
// CLIs the image does not carry (AWS, SSM plugin) install here, on the volume.
const TOOLS_DIR = path.join(VOLUME, "tools");
const TOOLS_BIN = path.join(TOOLS_DIR, "bin");
const PERSIST_HOME = path.join(VOLUME, "home");
const HOME = os.homedir() || process.env.HOME || "/home/rudder";
const TS_SOCKET = "/tmp/tailscaled.sock";
const TS_HOSTNAME = (process.env.RUDDER_TAILSCALE_HOSTNAME || "rudder-home").trim();
// 1.5s: a prompt an agent is blocked on should reach the phone quickly.
const STATE_INTERVAL_MS = 1500;

// Set by configureGitCredentials, which runs during boot: declared up here so
// boot never reaches it uninitialized (2.16.5 crashed on exactly that).
let githubCredentials = null;
const PORT_SCAN_INTERVAL_MS = 5000;
// All module state lives up here, above the boot sequence below: boot calls
// into functions that read it, and a `let` declared further down is not
// initialized yet when they do (a ReferenceError that kills the machine).
let credentials = null;
let credentialsRunning = false;
let vaultRefreshedAt = 0;
const lastMessageCache = new Map(); // runId -> { mtime, text }

if (!homeId || !cloudUrl || !workerToken) {
  console.error("home supervisor: RUDDER_WORKSPACE_ID, RUDDER_CLOUD_URL and RUDDER_WORKER_TOKEN are required");
  process.exit(2);
}

log(`home ${homeId} starting`);
fs.mkdirSync(CODE_DIR, { recursive: true });
persistHomeDirs();
linkCodeDir();

const vault = await fetchVaultSecrets();
const deferredRepoFiles = [];
if (vault) {
  writeVaultFiles(vault.files, deferredRepoFiles);
  configureGitCredentials(vault);
}
acceptClaudeDialogs([]);

const tailnet = { hostname: TS_HOSTNAME, dnsName: null, online: false, https: null, error: null };
const servedPorts = new Map(); // port -> { url, https }
await startTailscale(vault?.env ?? {});

const baseChildEnv = buildChildEnv(vault?.env ?? {});

/** @type {Map<string, Project>} */
const projects = new Map();
let controlSocket = null;
let lastStateJson = "";
let ports = []; // [{ port, pid, label, workspaceId|null }]

connectControl();
void checkCredentials();
void ensureRustToolchain();
void ensureAwsTooling().then(() => checkCredentials());
setInterval(() => void checkCredentials(), 30 * 60 * 1000);
setInterval(() => void publishState().catch((error) => log(`state: ${error.message}`)), STATE_INTERVAL_MS);
setInterval(() => void scanPorts().catch((error) => log(`ports: ${error.message}`)), PORT_SCAN_INTERVAL_MS);
setInterval(reportHeartbeat, 30_000);
reportHeartbeat();

for (const signal of ["SIGTERM", "SIGINT"]) {
  process.on(signal, () => {
    log(`received ${signal}; stopping dashboards`);
    for (const project of projects.values()) {
      project.stop();
    }
    // Dashboards persist Paused rows on SIGTERM; give them a moment (Fly waits 10s).
    setTimeout(() => process.exit(0), 5000).unref();
  });
}

// ---------------------------------------------------------------------------
// Projects: one per repo, each a dashboard PTY bridged to its own channel.
// ---------------------------------------------------------------------------

class Project {
  constructor(descriptor) {
    this.descriptor = descriptor;
    this.workspaceId = descriptor.workspaceId;
    this.name = descriptor.name;
    this.dir = path.join(CODE_DIR, descriptor.name);
    this.status = "starting";
    this.error = null;
    this.term = null;
    this.ws = null;
    this.stopped = false;
    this.restarts = [];
    this.pending = [];
    this.pendingBytes = 0;
    this.decoder = new StringDecoder("utf8");
    this.cols = 120;
    this.rows = 32;
  }

  async start() {
    try {
      await this.ensureClone();
    } catch (error) {
      this.status = "failed";
      const reason = firstLine(error.message);
      this.error = !githubCredentials && /could not read Username|Authentication failed|Repository not found/i.test(reason)
        ? "can't clone: the cloud vault has no GitHub token. Run `rudder cloud` in this repo on your laptop to add one."
        : `git clone failed: ${reason}`;
      log(`${this.name}: ${this.error}`);
      return;
    }
    writeDeferredRepoFiles(this.name, this.dir);
    acceptClaudeDialogs([this.dir]);
    this.connect();
    this.spawnDashboard();
  }

  async ensureClone() {
    if (fs.existsSync(path.join(this.dir, ".git"))) {
      shSoft(`git -C ${q(this.dir)} fetch --all -q`);
      return;
    }
    this.status = "cloning";
    const ref = this.descriptor.ref ? `--branch ${q(this.descriptor.ref)} ` : "";
    log(`${this.name}: cloning ${this.descriptor.remote}`);
    await run(`git clone ${ref}${q(this.descriptor.remote)} ${q(this.dir)}`);
    shSoft(`git -C ${q(this.dir)} config user.email >/dev/null 2>&1 || git -C ${q(this.dir)} config user.email "rudder-cloud@local"`);
    shSoft(`git -C ${q(this.dir)} config user.name >/dev/null 2>&1 || git -C ${q(this.dir)} config user.name "Rudder Cloud"`);
  }

  spawnDashboard() {
    if (this.stopped) {
      return;
    }
    const env = {
      ...baseChildEnv,
      RUDDER_WORKSPACE_ID: this.workspaceId,
      RUDDER_REPO_NAME: this.name,
      RUDDER_CLOUD_RUNTIME: "home",
    };
    this.term = pty.spawn("rudder", [], { name: "xterm-256color", cols: this.cols, rows: this.rows, cwd: this.dir, env });
    this.status = "running";
    this.error = null;
    const term = this.term;
    term.onData((data) => {
      const chunk = Buffer.from(data, "utf8");
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(chunk, { binary: true });
      } else {
        this.buffer(chunk);
      }
    });
    term.onExit(({ exitCode, signal }) => {
      if (this.term !== term) {
        return;
      }
      this.term = null;
      if (this.stopped) {
        this.status = "stopped";
        return;
      }
      // The dashboard is what hosts this repo's agents, so it comes back. A
      // dashboard that keeps dying is reported instead of restarted forever.
      const now = Date.now();
      this.restarts = this.restarts.filter((at) => now - at < 60_000).concat(now);
      if (this.restarts.length > 5) {
        this.status = "failed";
        this.error = `dashboard keeps exiting (last code ${exitCode ?? signal})`;
        log(`${this.name}: ${this.error}`);
        return;
      }
      log(`${this.name}: dashboard exited (${exitCode ?? signal}); restarting`);
      this.status = "starting";
      setTimeout(() => this.spawnDashboard(), 2000).unref();
    });
  }

  connect() {
    if (this.stopped) {
      return;
    }
    const url = `${cloudUrl.replace(/^http/, "ws")}/api/rudder/workspace/${encodeURIComponent(this.workspaceId)}/worker`;
    const socket = new WebSocket(url, {
      headers: { authorization: `Bearer ${workerToken}` },
    });
    socket.binaryType = "nodebuffer";
    socket.on("open", () => {
      this.ws = socket;
      try { socket._socket?.setNoDelay?.(true); } catch { /* ignore */ }
      socket.send(JSON.stringify({ type: "hello", cols: this.cols, rows: this.rows, sessionKind: "workspace", sessionId: this.workspaceId }));
      if (this.pendingBytes > 0) {
        socket.send(Buffer.concat(this.pending, this.pendingBytes), { binary: true });
        this.pending = [];
        this.pendingBytes = 0;
      }
      // The relay starts a fresh replay buffer for this socket: begin it with
      // a whole picture, so the next laptop to attach sees one.
      this.repaint();
    });
    socket.on("message", (data, isBinary) => {
      if (isBinary && Buffer.isBuffer(data)) {
        this.term?.write(this.decoder.write(data));
        return;
      }
      let payload;
      try {
        payload = JSON.parse(Buffer.isBuffer(data) ? data.toString("utf8") : String(data));
      } catch {
        return;
      }
      if (payload?.type === "resize" && Number.isFinite(payload.cols) && Number.isFinite(payload.rows)) {
        this.cols = clamp(Math.floor(payload.cols), 20, 500);
        this.rows = clamp(Math.floor(payload.rows), 5, 200);
        try { this.term?.resize(this.cols, this.rows); } catch { /* ignore */ }
      } else if (payload?.type === "probe") {
        socket.send(JSON.stringify({ type: "probe-reply", id: payload.id ?? null }));
      } else if (payload?.type === "repaint") {
        this.repaint();
      }
      // "signal" (Ctrl+C from a detaching laptop) is deliberately ignored: the
      // dashboard here outlives any one terminal attached to it.
    });
    const retry = () => {
      if (this.ws === socket) {
        this.ws = null;
      }
      if (!this.stopped) {
        setTimeout(() => this.connect(), 2000).unref();
      }
    };
    socket.on("close", retry);
    socket.on("error", () => {
      try { socket.terminate(); } catch { /* ignore */ }
    });
  }

  // A laptop came back and wants every cell again. SIGWINCH makes the
  // dashboard redraw the whole screen without changing its size (a same-size
  // resize never reaches it: the kernel drops it).
  repaint() {
    signalRepaint(this.term?.pid);
  }

  buffer(chunk) {
    const MAX = 256 * 1024;
    this.pending.push(chunk);
    this.pendingBytes += chunk.length;
    while (this.pendingBytes > MAX && this.pending.length > 1) {
      this.pendingBytes -= this.pending.shift().length;
    }
  }

  stop() {
    this.stopped = true;
    // The whole process group: the dashboard (rudder-native) pauses its live
    // agents on SIGTERM, which turns into resumable rows after a restart.
    try {
      if (this.term?.pid) process.kill(-this.term.pid, "SIGTERM");
    } catch {
      try { this.term?.kill("SIGTERM"); } catch { /* ignore */ }
    }
    try { this.ws?.close(1000, "closed"); } catch { /* ignore */ }
  }

  agents() {
    return readAgents(this.dir);
  }
}

// The dashboard is the PTY's session leader; its process group is the
// rudder launcher and rudder-native (agents run in PTYs of their own).
function signalRepaint(pid) {
  if (!pid) {
    return;
  }
  try {
    process.kill(-pid, "SIGWINCH");
  } catch {
    try { process.kill(pid, "SIGWINCH"); } catch { /* ignore */ }
  }
}

function reconcileProjects(descriptors) {
  const wanted = new Map(descriptors.map((descriptor) => [descriptor.workspaceId, descriptor]));
  for (const [id, project] of projects) {
    if (!wanted.has(id)) {
      log(`${project.name}: removed`);
      project.stop();
      projects.delete(id);
    }
  }
  for (const descriptor of descriptors) {
    if (!descriptor?.workspaceId || !descriptor.name || !descriptor.remote) {
      continue;
    }
    if (projects.has(descriptor.workspaceId)) {
      const existing = projects.get(descriptor.workspaceId);
      // A project that failed to clone gets another try when the list arrives again.
      if (existing.status === "failed" && !existing.term) {
        existing.status = "starting";
        void existing.start();
      }
      continue;
    }
    const project = new Project(descriptor);
    projects.set(descriptor.workspaceId, project);
    void project.start();
  }
}

// ---------------------------------------------------------------------------
// Control socket + RPCs.
// ---------------------------------------------------------------------------

function connectControl() {
  const url = `${cloudUrl.replace(/^http/, "ws")}/api/rudder/home/${encodeURIComponent(homeId)}/control`;
  const socket = new WebSocket(url, { headers: { authorization: `Bearer ${workerToken}` } });
  socket.on("open", () => {
    controlSocket = socket;
    lastStateJson = "";
    log("control connected");
    void publishState();
  });
  socket.on("message", (data) => {
    let message;
    try {
      message = JSON.parse(Buffer.isBuffer(data) ? data.toString("utf8") : String(data));
    } catch {
      return;
    }
    if (message?.type === "repos" && Array.isArray(message.repos)) {
      // The list is re-sent whenever a laptop runs `rudder cloud`, which may
      // have just put a repo's .env files in the vault: pick those up first.
      void refreshVaultRepoFiles().finally(() => {
        reconcileProjects(message.repos);
        lastStateJson = "";
      });
      return;
    }
    if (message?.type === "rpc" && typeof message.id === "string") {
      void handleRpc(message.method, message.params ?? {})
        .then((result) => sendControl({ type: "rpc-result", id: message.id, result: result ?? null }))
        .catch((error) => sendControl({ type: "rpc-result", id: message.id, error: error?.message || String(error) }));
    }
  });
  const retry = (code) => {
    if (controlSocket === socket) {
      controlSocket = null;
    }
    if (code === 4410 || code === 1008) {
      log(`control closed (${code}); not reconnecting`);
      return;
    }
    setTimeout(connectControl, 3000).unref();
  };
  socket.on("close", (code) => retry(code));
  socket.on("error", () => {
    try { socket.terminate(); } catch { /* ignore */ }
  });
}

function sendControl(message) {
  if (controlSocket && controlSocket.readyState === WebSocket.OPEN) {
    controlSocket.send(JSON.stringify(message));
  }
}

async function handleRpc(method, params) {
  if (method === "close") {
    const project = projects.get(params.workspaceId);
    if (project) {
      project.stop();
      projects.delete(params.workspaceId);
      if (params.removeClone) {
        fs.rmSync(project.dir, { recursive: true, force: true });
      }
    }
    return { ok: true };
  }
  if (method === "repos") {
    return await listGithubRepos();
  }
  if (method === "models") {
    return await listModels();
  }
  if (method === "createRepo") {
    return await createGithubRepo(params);
  }
  if (method === "adopt") {
    // Before the project lookup below: a laptop moving agents may have just
    // opened this repo, so its project can still be cloning.
    return await adoptMovedAgents(params);
  }
  if (method === "deleteRuns") {
    // A laptop deleting agents it moved here (or a delete queued while this
    // machine was asleep): its dashboard deletes them like dd, which also
    // takes them off the phone. Queued, so a dashboard still starting gets it.
    const target = await waitForClone(String(params.workspaceId || ""), 8 * 60_000);
    const runIds = (Array.isArray(params.runIds) ? params.runIds : []).filter((id) => /^[A-Za-z0-9._-]+$/.test(String(id)));
    for (const runId of runIds) {
      queueSteer(target, { kind: "delete", taskId: runId, instruction: "delete" });
    }
    return { queued: runIds.length };
  }
  if (method === "runs") {
    // Which of these runs exist here: the laptop drops its rows for the rest.
    // Unknown (not "gone") while the project is not open, or still cloning.
    const target = projects.get(params.workspaceId);
    if (!target || target.status === "cloning" || !fs.existsSync(path.join(target.dir, ".git"))) {
      return { known: false };
    }
    const runIds = (Array.isArray(params.runIds) ? params.runIds : []).filter((id) => /^[A-Za-z0-9._-]+$/.test(String(id)));
    const existing = runIds.filter((id) => fs.existsSync(path.join(target.dir, ".rudder", "runs", id, "run.json")));
    return { known: true, existing };
  }
  const project = projects.get(params.workspaceId);
  if (!project) {
    throw new Error("that project is not open on the home machine");
  }
  if (method === "transcript") {
    return readTranscript(project.dir, String(params.runId || ""), { limit: Number(params.limit) || 300, home: HOME });
  }
  if (method === "send") {
    return await steer(project, { kind: "steer", taskId: String(params.runId || ""), instruction: String(params.text || "") });
  }
  if (method === "task") {
    return await steer(project, { kind: "task", taskId: "conductor", instruction: String(params.text || "") });
  }
  if (method === "keys") {
    return await steer(project, { kind: "keys", taskId: String(params.runId || ""), instruction: String(params.keys || "") });
  }
  if (method === "screen") {
    return readScreen(project, String(params.runId || ""));
  }
  if (method === "answer") {
    return await answerAsk(project, params);
  }
  if (method === "action") {
    const kind = params.action === "merge" ? "merge" : params.action === "stop" ? "cancel" : params.action === "delete" ? "delete" : null;
    if (!kind) {
      throw new Error("unknown action");
    }
    return await steer(project, { kind, taskId: String(params.runId || ""), instruction: kind });
  }
  throw new Error(`unknown method ${method}`);
}

// A laptop moving its agents here (`rudder cloud move`, H in its dashboard).
// The bundle carries each agent's jj change and conversation; `rudder
// __adopt-moved` stages them in this repo's clone (src/move-home.ts), and the
// repo's dashboard resumes each one when its "adopt" request reaches the steer
// inbox. Requests are queued rather than awaited, so a dashboard that is still
// starting picks them up on its first poll.
async function adoptMovedAgents(params) {
  const project = await waitForClone(String(params.workspaceId || ""), 8 * 60_000);
  const bundlePath = String(params.bundlePath || "");
  if (!/^\/api\/rudder\/home\/[^/]+\/moves\/[^/]+\/bundle$/.test(bundlePath)) {
    throw new Error("bad bundle path");
  }
  const response = await fetch(`${cloudUrl}${bundlePath}`, { headers: { authorization: `Bearer ${workerToken}` } });
  if (!response.ok) {
    throw new Error(`could not fetch the moved agents (HTTP ${response.status})`);
  }
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-move-"));
  try {
    const archive = path.join(dir, "move.tgz");
    fs.writeFileSync(archive, Buffer.from(await response.arrayBuffer()));
    const result = await captureAll("rudder", ["__adopt-moved", "--repo", project.dir, archive], {
      env: { ...baseChildEnv },
      cwd: project.dir,
      timeoutMs: 5 * 60_000,
    });
    const line = result.stdout.trim().split("\n").reverse().find((entry) => entry.trim().startsWith("{"));
    if (result.code !== 0 || !line) {
      throw new Error(firstLine(result.stderr || result.stdout) || "rudder __adopt-moved failed");
    }
    const adopted = JSON.parse(line);
    const agents = Array.isArray(adopted.adopted) ? adopted.adopted : [];
    acceptClaudeDialogs(agents.map((agent) => agent.workspacePath).filter(Boolean));
    // Every adopted run, retried ones included: the dashboard resumes a run at
    // most once, and a retry is exactly when the first resume may not have run.
    for (const agent of agents) {
      queueSteer(project, { kind: "adopt", taskId: agent.runId, instruction: "adopt" });
    }
    lastStateJson = "";
    log(`${project.name}: adopted ${agents.length} moved agent(s)${adopted.failed?.length ? `, ${adopted.failed.length} failed` : ""}`);
    return adopted;
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

async function waitForClone(workspaceId, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const project = projects.get(workspaceId);
    if (project?.status === "failed" && !project.term) {
      throw new Error(project.error || "the project failed to start on the home machine");
    }
    if (project && project.status !== "cloning" && fs.existsSync(path.join(project.dir, ".git"))) {
      return project;
    }
    if (Date.now() > deadline) {
      throw new Error(project ? `the clone is still ${project.status}` : "that project is not open on the home machine");
    }
    await sleep(1000);
  }
}

// Like capture, but resolves on "close", after the output streams end: the
// caller parses stdout, and "exit" can fire before the last chunk arrives.
function captureAll(command, args, { env, cwd, timeoutMs }) {
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    let child;
    try {
      child = spawn(command, args, { env, cwd, stdio: ["ignore", "pipe", "pipe"] });
    } catch (error) {
      resolve({ code: -1, stdout: "", stderr: error.message });
      return;
    }
    const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => { clearTimeout(timer); resolve({ code: -1, stdout, stderr: error.message }); });
    child.on("close", (code) => { clearTimeout(timer); resolve({ code: code ?? -1, stdout, stderr }); });
  });
}

// `/model` on the phone: what the dashboard's own picker would offer (the
// models.dev and Codex caches, read by `rudder __models`), plus the current
// default. Cached briefly: the lists move on the order of days.
let modelsCache = null;
async function listModels() {
  if (modelsCache && Date.now() - modelsCache.at < 10 * 60_000) {
    return modelsCache.value;
  }
  const result = await captureAll("rudder", ["__models"], { env: { ...baseChildEnv }, cwd: HOME, timeoutMs: 30_000 });
  const line = result.stdout.trim().split("\n").reverse().find((entry) => entry.trim().startsWith("{"));
  if (result.code !== 0 || !line) {
    throw new Error(firstLine(result.stderr || result.stdout) || "could not list models");
  }
  const value = JSON.parse(line);
  modelsCache = { at: Date.now(), value };
  return value;
}

// "Add project" on the phone: the repos this GitHub login can open (its own,
// collaborations and its orgs'), newest first, plus where a new one can go.
async function listGithubRepos() {
  const env = { ...baseChildEnv };
  const [user, orgs, repos] = await Promise.all([
    capture("gh", ["api", "user", "--jq", ".login"], { env, timeoutMs: 20_000 }),
    capture("gh", ["api", "user/orgs", "--jq", ".[].login"], { env, timeoutMs: 20_000 }),
    capture("gh", ["api", "--paginate", "user/repos?per_page=100&sort=updated&affiliation=owner,collaborator,organization_member",
      "--jq", ".[] | {repo: .full_name, description: .description, private: .private, updatedAt: .pushed_at, archived: .archived}"], { env, timeoutMs: 60_000 }),
  ]);
  if (user.code !== 0) {
    throw new Error(githubCredentials ? `GitHub said: ${firstLine(user.stderr)}` : "the cloud vault has no GitHub token; run `rudder cloud` in a repo on your laptop to add one");
  }
  const open = new Set(Array.from(projects.values()).map((project) => project.descriptor.repo.toLowerCase()));
  const list = repos.stdout.split("\n").filter(Boolean).map((line) => {
    try {
      return JSON.parse(line);
    } catch {
      return null;
    }
  }).filter((repo) => repo && !repo.archived).slice(0, 300).map((repo) => ({ ...repo, open: open.has(String(repo.repo).toLowerCase()) }));
  const owners = [user.stdout.trim(), ...orgs.stdout.split("\n").map((line) => line.trim()).filter(Boolean)];
  return { owners, repos: list };
}

async function createGithubRepo(params) {
  const owner = String(params.owner || "").trim();
  const name = String(params.name || "").trim();
  if (!/^[A-Za-z0-9-]{1,39}$/.test(owner) || !/^[A-Za-z0-9._-]{1,100}$/.test(name)) {
    throw new Error("use a plain repository name (letters, digits, . _ -)");
  }
  const args = ["repo", "create", `${owner}/${name}`, params.private === false ? "--public" : "--private", "--add-readme"];
  const description = String(params.description || "").trim().slice(0, 300);
  if (description) {
    args.push("--description", description);
  }
  const result = await capture("gh", args, { env: { ...baseChildEnv }, timeoutMs: 60_000 });
  if (result.code !== 0) {
    throw new Error(firstLine(result.stderr || result.stdout) || "GitHub refused to create the repository");
  }
  return { repo: `${owner}/${name}` };
}

// The phone's terminal view: mark the agent as watched (the dashboard mirrors
// its visible screen to screen.txt for ~20s after each touch) and return the
// latest mirror.
function readScreen(project, runId) {
  if (!/^[A-Za-z0-9._-]+$/.test(runId)) {
    throw new Error("agent not found");
  }
  const dir = path.join(project.dir, ".rudder", "runs", runId);
  if (!fs.existsSync(dir)) {
    throw new Error("agent not found");
  }
  const watch = path.join(dir, "screen.watch");
  fs.writeFileSync(watch, "");
  const now = new Date();
  fs.utimesSync(watch, now, now);
  try {
    const file = path.join(dir, "screen.txt");
    return { text: fs.readFileSync(file, "utf8"), updatedAt: fs.statSync(file).mtimeMs };
  } catch {
    return { text: null, updatedAt: null };
  }
}

// Drop one request into the dashboard's steer inbox. The dashboard consumes it
// on its next poll, so a request queued while it is still starting is not lost.
function queueSteer(project, { kind, taskId, instruction }) {
  const requestId = randomUUID();
  const inbox = path.join(project.dir, ".rudder", "steer");
  fs.mkdirSync(inbox, { recursive: true });
  const safeId = (taskId || "conductor").replace(/[^A-Za-z0-9_.-]/g, "-").slice(0, 64) || "conductor";
  const name = `${String(Date.now()).padStart(13, "0")}-${String(process.hrtime.bigint()).padStart(20, "0")}-${safeId}-${requestId}.json`;
  const file = path.join(inbox, name);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ requestId, kind, taskId, instruction, ts: new Date().toISOString() }));
  fs.renameSync(tmp, file);
  return requestId;
}

// Requests go through the dashboard's own steer inbox (.rudder/steer), the same
// channel the web board uses: the dashboard is the single writer of its agents,
// so the phone never touches a PTY or run.json directly. The receipt says
// whether it was delivered.
async function steer(project, { kind, taskId, instruction }) {
  if (!instruction.trim()) {
    throw new Error("message is empty");
  }
  if (!project.term) {
    throw new Error(`the ${project.name} dashboard is ${project.status}${project.error ? `: ${project.error}` : ""}`);
  }
  const requestId = queueSteer(project, { kind, taskId, instruction });
  const receipts = path.join(project.dir, ".rudder", "steer-receipts");
  const receiptPath = path.join(receipts, `${requestId}.json`);
  const deadline = Date.now() + 12_000;
  while (Date.now() < deadline) {
    await sleep(300);
    try {
      const receipt = JSON.parse(fs.readFileSync(receiptPath, "utf8"));
      if (receipt.status === "delivered") {
        lastStateJson = "";
        return { ok: true, status: "delivered" };
      }
      if (receipt.status === "failed") {
        throw new Error(receipt.error || "the dashboard could not deliver it");
      }
    } catch (error) {
      if (error && error.code !== "ENOENT" && !(error instanceof SyntaxError)) {
        throw error;
      }
    }
  }
  return { ok: true, status: "queued" };
}

// ---------------------------------------------------------------------------
// State: what the phone app shows.
// ---------------------------------------------------------------------------

async function publishState() {
  if (!controlSocket) {
    return;
  }
  const list = [];
  for (const project of projects.values()) {
    list.push({
      workspaceId: project.workspaceId,
      name: project.name,
      repo: project.descriptor.repo,
      path: project.dir,
      dashboard: project.status,
      error: project.error,
      agents: project.status === "cloning" ? [] : project.agents(),
      ports: ports
        .filter((entry) => entry.workspaceId === project.workspaceId)
        .map((entry) => ({ port: entry.port, label: entry.label, url: servedPorts.get(entry.port)?.url ?? null })),
    });
  }
  const unassigned = ports.filter((entry) => !entry.workspaceId);
  if (unassigned.length > 0 && list.length > 0) {
    // A server started outside any repo still deserves a link; it goes with
    // the first project rather than nowhere.
    list[0].ports.push(...unassigned.map((entry) => ({ port: entry.port, label: entry.label, url: servedPorts.get(entry.port)?.url ?? null })));
  }
  const state = { type: "state", projects: list, tailnet: { ...tailnet }, credentials };
  const json = JSON.stringify(state);
  if (json === lastStateJson) {
    return;
  }
  lastStateJson = json;
  sendControl(state);
}

function scheduleStateSoon() {
  setTimeout(() => void publishState().catch((error) => log(`state: ${error.message}`)), 150).unref();
}

function readAgents(repoDir) {
  const runsDir = path.join(repoDir, ".rudder", "runs");
  let entries = [];
  try {
    entries = fs.readdirSync(runsDir);
  } catch {
    return [];
  }
  const agents = [];
  for (const id of entries) {
    let record;
    try {
      record = JSON.parse(fs.readFileSync(path.join(runsDir, id, "run.json"), "utf8"));
    } catch {
      continue;
    }
    if (!record || typeof record !== "object" || record.id !== id) {
      continue;
    }
    agents.push(agentSummary(record));
  }
  // A task typed a moment ago has no run record until its workspace is named;
  // the dashboard lists those here so the phone shows them at once.
  try {
    const pending = JSON.parse(fs.readFileSync(path.join(repoDir, ".rudder", "pending-tasks.json"), "utf8"));
    for (const task of Array.isArray(pending?.tasks) ? pending.tasks : []) {
      if (!task || typeof task.id !== "string" || typeof task.task !== "string") {
        continue;
      }
      const at = toMillis(task.queuedAt) ?? Date.now();
      agents.push({
        id: task.id,
        title: userTask(task.task).slice(0, 200),
        task: userTask(task.task).slice(0, 2000),
        status: "starting",
        pending: true,
        createdAt: at,
        updatedAt: at,
        lastMessage: null,
      });
    }
  } catch {
    // no tasks waiting
  }
  agents.sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0));
  return agents.slice(0, 60);
}


function agentSummary(record) {
  const updatedAt = toMillis(record.updatedAt) ?? toMillis(record.createdAt);
  const status = phoneStatus(record);
  let lastMessage = null;
  let remoteUrl = null;
  try {
    const cached = lastMessageCache.get(record.id);
    const text = lastAssistantText(record, { home: HOME, cache: cached });
    if (text) {
      lastMessageCache.set(record.id, text.cacheEntry);
      lastMessage = text.text;
      remoteUrl = text.remoteUrl ?? null;
    }
  } catch {
    // a preview is a nicety
  }
  return {
    id: record.id,
    title: (record.taskSummary || userTask(record.task) || record.id || "").toString().slice(0, 200),
    task: userTask(record.task).slice(0, 2000),
    status,
    phase: record.lifecyclePhase ?? null,
    attention: record.attention ?? null,
    backend: record.backend ?? "claude",
    model: record.model ?? null,
    mode: record.mode ?? null,
    createdAt: toMillis(record.createdAt),
    updatedAt,
    lastMessage,
    summary: typeof record.doneSummary === "string" ? record.doneSummary.slice(0, 2000) : null,
    nodeId: record.nodeId ?? null,
    planId: record.planId ?? null,
    remoteUrl,
    asks: /^[A-Za-z0-9._-]+$/.test(String(record.id)) ? readAsks(asksDir(record.id)) : [],
  };
}

// ---------------------------------------------------------------------------
// Prompts the phone answers (permission_hook.mjs, written by the dashboard into
// the run's Claude settings). Each pending prompt is <signals>/<run>.asks/<id>.json,
// held open by a hook process; the phone's answer becomes <id>.answer.json,
// which the hook prints as Claude's decision.
// ---------------------------------------------------------------------------
function asksDir(runId) {
  return path.join(HOME, ".rudder", "signals", `${runId}.asks`);
}

async function answerAsk(project, params) {
  const runId = String(params.runId || "");
  const askId = String(params.askId || "");
  if (!/^[A-Za-z0-9._-]+$/.test(runId) || !isAskId(askId)) {
    throw new Error("unknown prompt");
  }
  const dir = asksDir(runId);
  let request;
  try {
    request = JSON.parse(fs.readFileSync(path.join(dir, `${askId}.json`), "utf8"));
  } catch {
    throw new Error("that prompt was already answered");
  }
  const decision = askDecision(request, params.answer);
  if (decision.keys) {
    // The hook withdraws the request when the plan's result lands.
    return await steer(project, { kind: "keys", taskId: runId, instruction: decision.keys });
  }
  const file = path.join(dir, `${askId}.answer.json`);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, decision.output);
  fs.renameSync(tmp, file);
  scheduleStateSoon();
  return { ok: true };
}

// The phone's vocabulary: working / waiting / review / done / paused / failed /
// stopped / merged / orphaned.
function phoneStatus(record) {
  const status = String(record.status || "");
  const phase = String(record.lifecyclePhase || "");
  if (status === "running" || status === "steering" || status === "verifying" || status === "created") {
    return record.attention ? "waiting" : "working";
  }
  if (status === "completed" || status === "merge-conflict") {
    if (phase === "review" || status === "merge-conflict" || phase.includes("needs merge") || phase.includes("proof")) {
      return "review";
    }
    return "done";
  }
  if (status === "cancelled") {
    return "stopped";
  }
  if (["merged", "failed", "paused", "orphaned", "migrated"].includes(status)) {
    return status === "migrated" ? "done" : status;
  }
  return status || "done";
}

// ---------------------------------------------------------------------------
// Ports: dev servers agents start become tailnet previews.
// ---------------------------------------------------------------------------

async function scanPorts() {
  const listening = listeningSockets();
  const inodeToPid = socketOwners(new Set(listening.map((entry) => entry.inode)));
  const found = new Map();
  for (const entry of listening) {
    const pid = inodeToPid.get(entry.inode);
    if (!pid || found.has(entry.port)) {
      continue;
    }
    const info = processInfo(pid);
    if (!info || ignoredProcess(info)) {
      continue;
    }
    found.set(entry.port, {
      port: entry.port,
      pid,
      label: info.label,
      workspaceId: projectForPath(info.cwd),
    });
  }
  ports = Array.from(found.values()).sort((a, b) => a.port - b.port);
  if (tailnet.online) {
    for (const entry of ports) {
      if (!servedPorts.has(entry.port)) {
        servePort(entry.port);
      }
    }
    for (const port of Array.from(servedPorts.keys())) {
      if (!found.has(port)) {
        unservePort(port);
      }
    }
  }
}

function listeningSockets() {
  const out = [];
  for (const file of ["/proc/net/tcp", "/proc/net/tcp6"]) {
    let text = "";
    try {
      text = fs.readFileSync(file, "utf8");
    } catch {
      continue;
    }
    for (const line of text.split("\n").slice(1)) {
      const cols = line.trim().split(/\s+/);
      if (cols.length < 10 || cols[3] !== "0A") {
        continue;
      }
      const [address, portHex] = cols[1].split(":");
      const port = parseInt(portHex, 16);
      // Loopback-only and wildcard listeners both count: tailscale serve
      // proxies to 127.0.0.1, so a localhost-bound dev server works too.
      if (!Number.isFinite(port) || port < 1024 || port === 41641) {
        continue;
      }
      out.push({ port, inode: cols[9], address });
    }
  }
  return out;
}

function socketOwners(inodes) {
  const owners = new Map();
  let pids = [];
  try {
    pids = fs.readdirSync("/proc").filter((name) => /^\d+$/.test(name));
  } catch {
    return owners;
  }
  for (const pid of pids) {
    let fds = [];
    try {
      fds = fs.readdirSync(`/proc/${pid}/fd`);
    } catch {
      continue;
    }
    for (const fd of fds) {
      let target = "";
      try {
        target = fs.readlinkSync(`/proc/${pid}/fd/${fd}`);
      } catch {
        continue;
      }
      const match = /^socket:\[(\d+)\]$/.exec(target);
      if (match && inodes.has(match[1]) && !owners.has(match[1])) {
        owners.set(match[1], Number(pid));
      }
    }
  }
  return owners;
}

function processInfo(pid) {
  try {
    const comm = fs.readFileSync(`/proc/${pid}/comm`, "utf8").trim();
    const cmdline = fs.readFileSync(`/proc/${pid}/cmdline`, "utf8").split("\0").filter(Boolean);
    let cwd = "";
    try {
      cwd = fs.readlinkSync(`/proc/${pid}/cwd`);
    } catch {
      // ignore
    }
    return { pid, comm, cmdline, cwd, label: portLabel(comm, cmdline) };
  } catch {
    return null;
  }
}

function ignoredProcess(info) {
  if (info.pid === process.pid) {
    return true;
  }
  if (["tailscaled", "tailscale", "rudder-native", "claude", "codex"].includes(info.comm)) {
    return true;
  }
  const joined = info.cmdline.join(" ");
  // Rudder's own processes: this supervisor, and the `rudder` CLI that hosts
  // each dashboard and its board server (`node /usr/local/bin/rudder`, or the
  // package path behind the npm link).
  const script = path.basename(info.cmdline[1] || "");
  return (
    /\/opt\/rudder(-worker)?\//.test(joined) ||
    /@viraatdas\/rudder|\/rudder\/dist\//.test(joined) ||
    (info.comm === "node" && (script === "rudder" || script === "rudder.js"))
  );
}

function portLabel(comm, cmdline) {
  const args = cmdline.slice(1).map((arg) => path.basename(arg));
  const interesting = args.find((arg) => /^(next|vite|astro|remix|nuxt|webpack|serve|http-server|uvicorn|gunicorn|flask|rails|django|manage\.py)/.test(arg));
  if (interesting) {
    const index = args.indexOf(interesting);
    return [interesting, args[index + 1]].filter(Boolean).join(" ").slice(0, 40);
  }
  return comm.slice(0, 40);
}

function projectForPath(cwd) {
  if (!cwd) {
    return null;
  }
  for (const project of projects.values()) {
    if (cwd === project.dir || cwd.startsWith(`${project.dir}/`)) {
      return project.workspaceId;
    }
    // jj workspaces live beside the repo: <code>/.rudder-workspaces/<name>-<hash>/...
    const workspaces = path.join(path.dirname(project.dir), ".rudder-workspaces", `${project.name}-`);
    if (cwd.startsWith(workspaces)) {
      return project.workspaceId;
    }
  }
  return null;
}

function servePort(port) {
  const host = tailnet.dnsName || TS_HOSTNAME;
  // userspace tailscaled forwards inbound tailnet connections to 127.0.0.1,
  // so the plain address reaches even a localhost-bound dev server. It is the
  // link until (and unless) HTTPS serving turns out to work.
  servedPorts.set(port, { url: `http://${host}:${port}`, https: false });
  if (tailnet.https === false || !tailnet.dnsName) {
    return;
  }
  // When Serve/HTTPS is not enabled on the tailnet, `tailscale serve` does not
  // fail: it prints an "enable it here" link and WAITS. So it runs detached
  // with a deadline, never on the event loop, and one refusal settles it.
  const child = spawn("tailscale", ["--socket", TS_SOCKET, "serve", "--bg", `--https=${port}`, `http://127.0.0.1:${port}`], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  child.stdout.on("data", (chunk) => { output += chunk; });
  child.stderr.on("data", (chunk) => { output += chunk; });
  const timer = setTimeout(() => child.kill("SIGKILL"), 10_000);
  child.on("exit", (code) => {
    clearTimeout(timer);
    if (code === 0) {
      tailnet.https = true;
      if (servedPorts.has(port)) {
        servedPorts.set(port, { url: `https://${tailnet.dnsName}:${port}`, https: true });
        lastStateJson = "";
      }
      return;
    }
    if (tailnet.https !== false) {
      tailnet.https = false;
      const link = /https:\/\/login\.tailscale\.com\/\S+/.exec(output)?.[0];
      tailnet.error = link
        ? `HTTPS previews are off on this tailnet; enable Serve at ${link}`
        : `HTTPS previews unavailable (${firstLine(output) || `exit ${code}`})`;
      log(tailnet.error);
      lastStateJson = "";
    }
  });
}

function unservePort(port) {
  const entry = servedPorts.get(port);
  servedPorts.delete(port);
  if (entry?.https) {
    spawnSync("tailscale", ["--socket", TS_SOCKET, "serve", `--https=${port}`, "off"], { timeout: 10_000 });
  }
}

// ---------------------------------------------------------------------------
// Credentials: can this machine act as the user, as the same accounts? A login
// file existing proves nothing (an expired Claude login still has one), so each
// tool makes one tiny real request, and reports which account it is.
// ---------------------------------------------------------------------------


async function checkCredentials() {
  if (credentialsRunning) {
    return;
  }
  credentialsRunning = true;
  try {
    const env = { ...baseChildEnv };
    const [github, claude, codex, aws] = await Promise.all([checkGithub(env), checkClaude(env), checkCodex(env), checkAws(env)]);
    credentials = { github, claude, codex, aws, checkedAt: new Date().toISOString() };
    // A CLI missing at boot is usually mid-update (entrypoint.sh), not absent:
    // look again shortly instead of reporting a broken login for 30 minutes.
    if ([github, claude, codex, aws].some((check) => /ENOENT/.test(check.detail || ""))) {
      setTimeout(() => void checkCredentials(), 60_000).unref();
    }
    lastStateJson = "";
    log(`credentials: github=${github.ok ? github.account : "no"} claude=${claude.ok ? claude.account : "no"} codex=${codex.ok ? codex.account : "no"}`);
  } catch (error) {
    log(`credential check: ${error.message}`);
  } finally {
    credentialsRunning = false;
  }
}

// Which AWS principal the vault's ~/.aws resolves to (default profile).
async function checkAws(env) {
  if (!fs.existsSync(path.join(HOME, ".aws", "credentials")) && !fs.existsSync(path.join(HOME, ".aws", "config"))) {
    return { ok: false, account: null, detail: "no ~/.aws in the vault" };
  }
  const result = await capture("aws", ["sts", "get-caller-identity", "--query", "Arn", "--output", "text"], { env, timeoutMs: 30_000 });
  return result.code === 0 && result.stdout.trim().startsWith("arn:")
    ? { ok: true, account: result.stdout.trim(), detail: null }
    : { ok: false, account: null, detail: firstLine(result.stderr || result.stdout) || `exit ${result.code}` };
}

async function checkGithub(env) {
  if (!env.GH_TOKEN && !env.GITHUB_TOKEN) {
    return { ok: false, account: null, detail: "no GitHub token in the vault" };
  }
  const result = await capture("gh", ["api", "user", "--jq", ".login"], { env, timeoutMs: 20_000 });
  return result.code === 0 && result.stdout.trim()
    ? { ok: true, account: result.stdout.trim(), detail: null }
    : { ok: false, account: null, detail: firstLine(result.stderr || result.stdout) || "GitHub rejected the token" };
}

async function checkClaude(env) {
  const status = await capture("claude", ["auth", "status", "--json"], { env, timeoutMs: 20_000 });
  let account = null;
  try {
    const parsed = JSON.parse(status.stdout);
    account = parsed.email || null;
    if (!parsed.loggedIn) {
      return { ok: false, account, detail: "not logged in" };
    }
  } catch {
    // fall through to the real request
  }
  const probe = await capture("claude", ["-p", "Reply with exactly: OK", "--model", "haiku"], { env, cwd: os.tmpdir(), timeoutMs: 90_000 });
  const text = `${probe.stdout}\n${probe.stderr}`;
  if (probe.code === 0 && /\bOK\b/.test(probe.stdout)) {
    return { ok: true, account, detail: null };
  }
  const detail = /login expired|please run \/login|invalid.*(token|api key)|not logged in|authentication/i.test(text)
    ? "login expired"
    : firstLine(text) || `exit ${probe.code}`;
  return { ok: false, account, detail };
}

async function checkCodex(env) {
  const account = codexAccount();
  const status = await capture("codex", ["login", "status"], { env, timeoutMs: 20_000 });
  if (status.code !== 0) {
    return { ok: false, account, detail: firstLine(status.stderr || status.stdout) || "not logged in" };
  }
  const probe = await capture("codex", ["exec", "--skip-git-repo-check", "Reply with exactly: OK"], { env, cwd: os.tmpdir(), timeoutMs: 120_000 });
  if (probe.code === 0 && /\bOK\b/.test(probe.stdout)) {
    return { ok: true, account, detail: null };
  }
  const text = `${probe.stdout}\n${probe.stderr}`;
  const detail = /refresh token|401|unauthorized|log ?in again|expired/i.test(text) ? "login expired" : firstLine(text) || `exit ${probe.code}`;
  return { ok: false, account, detail };
}

function codexAccount() {
  try {
    const auth = JSON.parse(fs.readFileSync(path.join(HOME, ".codex", "auth.json"), "utf8"));
    const payload = String(auth?.tokens?.id_token || "").split(".")[1];
    if (!payload) {
      return null;
    }
    return JSON.parse(Buffer.from(payload, "base64url").toString("utf8")).email || null;
  } catch {
    return null;
  }
}

function capture(command, args, { env, cwd, timeoutMs }) {
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    let child;
    try {
      child = spawn(command, args, { env, cwd, stdio: ["ignore", "pipe", "pipe"] });
    } catch (error) {
      resolve({ code: -1, stdout: "", stderr: error.message });
      return;
    }
    const timer = setTimeout(() => child.kill("SIGKILL"), timeoutMs);
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => { clearTimeout(timer); resolve({ code: -1, stdout, stderr: error.message }); });
    child.on("exit", (code) => { clearTimeout(timer); resolve({ code: code ?? -1, stdout, stderr }); });
  });
}

// ---------------------------------------------------------------------------
// Toolchains: Rust is not in the worker image (it would double its size), so
// the home machine installs it once onto the persistent volume, in the
// background, and every later boot finds it there.
// ---------------------------------------------------------------------------

async function ensureRustToolchain() {
  if (process.env.RUDDER_HOME_TOOLCHAINS === "0") {
    return;
  }
  if (fs.existsSync(path.join(HOME, ".cargo", "bin", "cargo"))) {
    return;
  }
  log("installing the Rust toolchain onto the volume (first boot)");
  const result = await capture("sh", ["-c", "curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path"], {
    env: { ...process.env, HOME },
    cwd: HOME,
    timeoutMs: 15 * 60 * 1000,
  });
  log(result.code === 0 ? "Rust toolchain installed" : `Rust toolchain install failed: ${firstLine(result.stderr || result.stdout)}`);
}

// AWS CLI v2 and the Session Manager plugin (SSH over SSM), so agents reach the
// same AWS the laptop does with the credentials from the vault (~/.aws).
async function ensureAwsTooling() {
  if (process.env.RUDDER_HOME_TOOLCHAINS === "0") {
    return;
  }
  fs.mkdirSync(TOOLS_BIN, { recursive: true });
  const env = { ...process.env, HOME, PATH: `${TOOLS_BIN}:${process.env.PATH || ""}` };
  if (!fs.existsSync(path.join(TOOLS_BIN, "aws"))) {
    log("installing the AWS CLI onto the volume");
    // No unzip in the image: python extracts, keeping the executable bits the
    // installer needs.
    const result = await capture("sh", ["-c", `set -e
      dir=$(mktemp -d); cd "$dir"
      curl -fsSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o aws.zip
      python3 -c 'import zipfile,os,sys
z=zipfile.ZipFile("aws.zip")
for i in z.infolist():
    p=z.extract(i)
    m=(i.external_attr>>16)&0o777
    if m: os.chmod(p,m)'
      ./aws/install -i "${TOOLS_DIR}/aws-cli" -b "${TOOLS_BIN}" --update
      cd /; rm -rf "$dir"`], { env, cwd: TOOLS_DIR, timeoutMs: 10 * 60 * 1000 });
    log(result.code === 0 ? "AWS CLI installed" : `AWS CLI install failed: ${firstLine(result.stderr || result.stdout)}`);
  }
  if (!fs.existsSync(path.join(TOOLS_BIN, "session-manager-plugin"))) {
    const result = await capture("sh", ["-c", `set -e
      dir=$(mktemp -d); cd "$dir"
      curl -fsSL https://s3.amazonaws.com/session-manager-downloads/plugin/latest/ubuntu_64bit/session-manager-plugin.deb -o ssm.deb
      dpkg-deb -x ssm.deb "${TOOLS_DIR}/ssm"
      ln -sf "${TOOLS_DIR}/ssm/usr/local/sessionmanagerplugin/bin/session-manager-plugin" "${TOOLS_BIN}/session-manager-plugin"
      cd /; rm -rf "$dir"`], { env, cwd: TOOLS_DIR, timeoutMs: 5 * 60 * 1000 });
    log(result.code === 0 ? "Session Manager plugin installed" : `Session Manager plugin install failed: ${firstLine(result.stderr || result.stdout)}`);
  }
  // Deploy CLIs agents reach for (logins: see persistHomeDirs and the vault).
  if (!fs.existsSync(path.join(TOOLS_BIN, "vercel")) || !fs.existsSync(path.join(TOOLS_BIN, "netlify"))) {
    const result = await capture("npm", ["install", "-g", "--prefix", TOOLS_DIR, "vercel", "netlify-cli"], { env, cwd: TOOLS_DIR, timeoutMs: 10 * 60 * 1000 });
    log(result.code === 0 ? "Vercel and Netlify CLIs installed" : `Vercel/Netlify CLI install failed: ${firstLine(result.stderr || result.stdout)}`);
  }
}

// ---------------------------------------------------------------------------
// Tailscale.
// ---------------------------------------------------------------------------

async function startTailscale(env) {
  const authKey = (env.TS_AUTHKEY || "").trim();
  if (!authKey) {
    tailnet.error = "no TS_AUTHKEY in the vault";
    log("tailscale: skipped (no TS_AUTHKEY in the vault)");
    return;
  }
  if (!commandExists("tailscaled")) {
    tailnet.error = "tailscale is not installed in this image";
    log("tailscale: not installed");
    return;
  }
  const stateDir = path.join(PERSIST_HOME, ".tailscale");
  fs.mkdirSync(stateDir, { recursive: true, mode: 0o700 });
  const daemon = spawn("tailscaled", [
    "--tun=userspace-networking",
    `--statedir=${stateDir}`,
    `--socket=${TS_SOCKET}`,
    "--port=41641",
  ], { stdio: ["ignore", "ignore", "pipe"], detached: false });
  daemon.stderr.on("data", () => undefined);
  daemon.on("exit", (code) => {
    tailnet.online = false;
    log(`tailscaled exited (${code})`);
  });
  for (let attempt = 0; attempt < 50 && !fs.existsSync(TS_SOCKET); attempt += 1) {
    await sleep(100);
  }
  // The node key persists on the volume, so after the first boot this rejoins
  // as the same device without spending the auth key again.
  const up = spawnSync("tailscale", [
    "--socket", TS_SOCKET, "up",
    `--authkey=${authKey}`,
    `--hostname=${TS_HOSTNAME}`,
    "--accept-dns=false",
  ], { encoding: "utf8", timeout: 60_000 });
  if (up.status !== 0) {
    tailnet.error = firstLine(up.stderr || up.stdout || "tailscale up failed");
    log(`tailscale up failed: ${tailnet.error}`);
    return;
  }
  const status = spawnSync("tailscale", ["--socket", TS_SOCKET, "status", "--json"], { encoding: "utf8", timeout: 10_000 });
  try {
    const parsed = JSON.parse(status.stdout);
    tailnet.dnsName = String(parsed?.Self?.DNSName || "").replace(/\.$/, "") || null;
    tailnet.hostname = parsed?.Self?.HostName || TS_HOSTNAME;
    tailnet.online = Boolean(parsed?.Self?.Online ?? true);
  } catch {
    tailnet.online = true;
  }
  tailnet.error = null;
  log(`tailscale up as ${tailnet.dnsName || tailnet.hostname}`);
}

// ---------------------------------------------------------------------------
// Boot helpers.
// ---------------------------------------------------------------------------

// $HOME is on the machine's ephemeral root disk. Conversations (~/.claude,
// ~/.codex) and Rudder's own state (~/.rudder) must survive a stop/start, or
// every agent would come back with no memory and nothing to resume.
function persistHomeDirs() {
  fs.mkdirSync(PERSIST_HOME, { recursive: true });
  // .cargo/.rustup: the Rust toolchain (installed on first boot, see
  // ensureRustToolchain) lives on the volume too, so it survives restarts.
  // com.vercel.cli: the cloud's own `vercel login` (its refresh token rotates,
  // so it cannot be a copy of the laptop's).
  for (const name of [".claude", ".codex", ".rudder", ".config", ".local/share/opencode", ".local/share/com.vercel.cli", ".cargo", ".rustup"]) {
    const live = path.join(HOME, name);
    const stored = path.join(PERSIST_HOME, name);
    fs.mkdirSync(stored, { recursive: true });
    let stat = null;
    try {
      stat = fs.lstatSync(live);
    } catch {
      stat = null;
    }
    if (stat?.isSymbolicLink()) {
      continue;
    }
    if (stat?.isDirectory()) {
      // Whatever the image put there (installers, defaults) joins the stored
      // copy without overwriting what a previous boot saved.
      fs.cpSync(live, stored, { recursive: true, force: false, errorOnExist: false });
      fs.rmSync(live, { recursive: true, force: true });
    }
    fs.mkdirSync(path.dirname(live), { recursive: true });
    fs.symlinkSync(stored, live);
  }
}

function linkCodeDir() {
  const link = path.join(HOME, "code");
  try {
    if (fs.lstatSync(link).isSymbolicLink()) {
      return;
    }
    fs.rmSync(link, { recursive: true, force: true });
  } catch {
    // absent
  }
  fs.symlinkSync(CODE_DIR, link);
}

async function fetchVaultSecrets() {
  const url = `${cloudUrl}/api/rudder/workspace/${encodeURIComponent(homeId)}/secrets`;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      const res = await fetch(url, { headers: { authorization: `Bearer ${workerToken}` } });
      if (res.ok) {
        const data = await res.json();
        const env = data && typeof data.env === "object" && data.env && !Array.isArray(data.env) ? data.env : {};
        const files = Array.isArray(data?.files) ? data.files : [];
        log(`vault: ${Object.keys(env).length} env and ${files.length} file secret(s)`);
        return { env, files };
      }
      log(`vault: HTTP ${res.status}`);
      if (res.status === 401 || res.status === 403 || res.status === 404) {
        break;
      }
    } catch (error) {
      log(`vault: ${error?.message || error}`);
    }
    await sleep(1000 * (attempt + 1));
  }
  log("vault: continuing without secrets");
  return null;
}

function writeVaultFiles(files, deferred) {
  for (const file of files || []) {
    if (typeof file?.path !== "string" || typeof file?.contentBase64 !== "string" || !file.path.startsWith("~/")) {
      continue;
    }
    const segments = file.path.slice(2).split("/");
    if (segments.some((part) => part === "" || part === "." || part === "..")) {
      continue;
    }
    // Repo-scoped files (~/code/<repo>/.env.local) can only be written once the
    // repo is cloned; writing first would make the clone target non-empty.
    if (segments[0] === "code" && segments.length >= 3) {
      deferred.push({ repo: segments[1], rest: segments.slice(2), file });
      continue;
    }
    const target = path.join(HOME, ...segments);
    if (keepFresherLogin(target, file)) {
      continue;
    }
    try {
      fs.mkdirSync(path.dirname(target), { recursive: true, mode: 0o700 });
      fs.writeFileSync(target, Buffer.from(file.contentBase64, "base64"), { mode: file.mode || 0o600 });
    } catch (error) {
      log(`vault file ${file.path}: ${error?.message || error}`);
    }
  }
  // settings.json is rewritten from the vault every boot, so its hooks are
  // fixed every boot too: laptop paths become this home, and hooks whose
  // program is still missing are dropped. Last, so ~/.claude/hooks is on disk.
  sanitizeHomeHookConfigs(HOME, log);
}

// Claude and Codex refresh their OAuth tokens in place, and a refresh can
// invalidate the old refresh token. These files live on the volume, so after the
// first boot the copy here is usually NEWER than the vault's: overwriting it on
// every boot would log the agents out. Keep whichever expires later.
function keepFresherLogin(target, file) {
  const kind = target.endsWith(path.join(".claude", ".credentials.json"))
    ? "claude"
    : target.endsWith(path.join(".codex", "auth.json"))
      ? "codex"
      : null;
  if (!kind || !fs.existsSync(target)) {
    return false;
  }
  try {
    const current = JSON.parse(fs.readFileSync(target, "utf8"));
    const incoming = JSON.parse(Buffer.from(file.contentBase64, "base64").toString("utf8"));
    const freshness = (value) =>
      kind === "claude"
        ? Number(value?.claudeAiOauth?.expiresAt ?? 0)
        : Date.parse(value?.last_refresh ?? "") || 0;
    return freshness(current) >= freshness(incoming);
  } catch {
    return false;
  }
}


async function refreshVaultRepoFiles() {
  if (Date.now() - vaultRefreshedAt < 5000) {
    return;
  }
  vaultRefreshedAt = Date.now();
  const fresh = await fetchVaultSecrets();
  if (!fresh) {
    return;
  }
  deferredRepoFiles.length = 0;
  writeVaultFiles(fresh.files, deferredRepoFiles);
  // A GitHub token added since boot takes effect now; failed clones retry when
  // the repo list is reconciled right after this.
  configureGitCredentials(fresh);
  // Same filtering as at boot (no machine credentials reach agents).
  Object.assign(baseChildEnv, buildChildEnv(fresh.env ?? {}));
  void checkCredentials();
  for (const project of projects.values()) {
    if (fs.existsSync(path.join(project.dir, ".git"))) {
      writeDeferredRepoFiles(project.name, project.dir);
    }
  }
}

function writeDeferredRepoFiles(repoName, repoDir) {
  for (const entry of deferredRepoFiles) {
    if (entry.repo !== repoName) {
      continue;
    }
    const target = path.join(repoDir, ...entry.rest);
    try {
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, Buffer.from(entry.file.contentBase64, "base64"), { mode: entry.file.mode || 0o600 });
    } catch (error) {
      log(`vault file ${entry.file.path}: ${error?.message || error}`);
    }
  }
}

// A token in the vault wins. A synced ~/.config/gh/hosts.yml usually has NO
// token in it (gh keeps it in the laptop's keychain), so trusting the file made
// every private clone die at git's username prompt.
function configureGitCredentials(secrets) {
  const token = secrets?.env?.GH_TOKEN || secrets?.env?.GITHUB_TOKEN || "";
  if (token) {
    fs.writeFileSync(path.join(HOME, ".git-credentials"), `https://x-access-token:${token}@github.com\n`, { mode: 0o600 });
    shSoft("git config --global --unset-all credential.https://github.com.helper 2>/dev/null; git config --global credential.helper store");
    // For the supervisor's own git/gh calls; agents get it through the vault env.
    process.env.GH_TOKEN = token;
    githubCredentials = "token";
    return;
  }
  const hosts = path.join(HOME, ".config", "gh", "hosts.yml");
  if (fs.existsSync(hosts) && /oauth_token:/.test(fs.readFileSync(hosts, "utf8"))) {
    shSoft("command -v gh >/dev/null 2>&1 && gh auth setup-git 2>/dev/null");
    githubCredentials = "gh";
    return;
  }
  githubCredentials = null;
  log("no GitHub token in the vault; private repos can't be cloned (run `rudder cloud` in a repo on your laptop)");
}


// Nobody is at this machine's keyboard: answer Claude's one-time questions up
// front. Every repo here came from the user's own GitHub; Remote Control is the
// point of the machine.
function acceptClaudeDialogs(paths) {
  const configPath = path.join(HOME, ".claude.json");
  let config = {};
  try {
    config = JSON.parse(fs.readFileSync(configPath, "utf8"));
  } catch {
    config = {};
  }
  config.remoteDialogSeen = true;
  config.hasCompletedOnboarding = true;
  if (!config.projects || typeof config.projects !== "object") {
    config.projects = {};
  }
  for (const projectPath of [CODE_DIR, ...paths]) {
    config.projects[projectPath] = { ...(config.projects[projectPath] || {}), hasTrustDialogAccepted: true };
  }
  try {
    fs.writeFileSync(configPath, JSON.stringify(config, null, 2));
  } catch (error) {
    log(`claude config: ${error.message}`);
  }
}

function buildChildEnv(vaultEnv) {
  const env = {
    ...process.env,
    ...vaultEnv,
    // Rust (installed onto the volume by ensureRustToolchain) for agents
    // building native code, Rudder's own included.
    PATH: `${TOOLS_BIN}:${path.join(HOME, ".cargo", "bin")}:${process.env.PATH || ""}`,
    HOME,
    TERM: "xterm-256color",
    COLORTERM: "truecolor",
    RUDDER_HEADLESS: "0",
    RUDDER_CLOUD_WORKER: "1",
    RUDDER_REMOTE_CONTROL: process.env.RUDDER_REMOTE_CONTROL ?? "1",
    RUDDER_DISABLE_AUTO_UPDATE: "1",
    RUDDER_DISABLE_UPDATE_CHECK: "1",
    RUDDER_DISABLE_ONBOARD_INSTALL: "1",
    RUDDER_CODEX_BIN: process.env.RUDDER_CODEX_BIN || "codex",
  };
  // Nothing an agent runs should be able to read the machine's own credentials.
  for (const secret of ["RUDDER_WORKER_TOKEN", "RUDDER_SNAPSHOT_URL", "RUDDER_CLOUD_TOKEN", "TS_AUTHKEY", "TS_API_KEY", "RUDDER_HOME"]) {
    delete env[secret];
  }
  return env;
}

function reportHeartbeat() {
  let activeAgents = 0;
  for (const project of projects.values()) {
    activeAgents += project.agents().filter((agent) => agent.status === "working").length;
  }
  fetch(`${cloudUrl}/api/rudder/workspace/${encodeURIComponent(homeId)}/heartbeat`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${workerToken}` },
    body: JSON.stringify({ state: "running", machineId: flyMachineId || undefined, activeAgents }),
  }).catch(() => undefined);
}

// ---------------------------------------------------------------------------
// Small utilities.
// ---------------------------------------------------------------------------

function run(command) {
  return new Promise((resolve, reject) => {
    const child = spawn("sh", ["-c", command], { stdio: ["ignore", "inherit", "pipe"] });
    let stderr = "";
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
      process.stderr.write(chunk);
    });
    child.on("exit", (code) => (code === 0 ? resolve() : reject(new Error(stderr.trim() || `exit ${code}`))));
    child.on("error", reject);
  });
}

function shSoft(command) {
  spawnSync("sh", ["-c", command], { stdio: "inherit" });
}

function commandExists(name) {
  return spawnSync("sh", ["-c", `command -v ${name}`], { stdio: "ignore" }).status === 0;
}

function q(value) {
  return `'${String(value).replace(/'/g, "'\\''")}'`;
}

function clamp(value, min, max) {
  return Math.max(min, Math.min(max, value));
}

function firstLine(text) {
  return String(text || "").trim().split("\n").filter(Boolean).pop()?.slice(0, 300) ?? "";
}

function toMillis(value) {
  if (value === null || value === undefined || value === "") {
    return null;
  }
  const number = Number(value);
  if (Number.isFinite(number)) {
    return number;
  }
  const parsed = Date.parse(String(value));
  return Number.isFinite(parsed) ? parsed : null;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function log(message) {
  console.log(`[home] ${message}`);
}
