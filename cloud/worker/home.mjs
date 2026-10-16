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

import { readTranscript, lastAssistantText } from "./transcripts.mjs";

const cloudUrl = (process.env.RUDDER_CLOUD_URL || "").trim().replace(/\/$/, "");
const homeId = (process.env.RUDDER_WORKSPACE_ID || "").trim();
const workerToken = (process.env.RUDDER_WORKER_TOKEN || "").trim();
const flyMachineId = (process.env.FLY_MACHINE_ID || "").trim();

const VOLUME = "/workspace";
const CODE_DIR = path.join(VOLUME, "code");
const PERSIST_HOME = path.join(VOLUME, "home");
const HOME = os.homedir() || process.env.HOME || "/home/rudder";
const TS_SOCKET = "/tmp/tailscaled.sock";
const TS_HOSTNAME = (process.env.RUDDER_TAILSCALE_HOSTNAME || "rudder-home").trim();
const STATE_INTERVAL_MS = 3000;
const PORT_SCAN_INTERVAL_MS = 5000;

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
      this.error = `git clone failed: ${firstLine(error.message)}`;
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
  if (method === "action") {
    const kind = params.action === "merge" ? "merge" : params.action === "stop" ? "cancel" : null;
    if (!kind) {
      throw new Error("unknown action");
    }
    return await steer(project, { kind, taskId: String(params.runId || ""), instruction: kind });
  }
  throw new Error(`unknown method ${method}`);
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
  const requestId = randomUUID();
  const inbox = path.join(project.dir, ".rudder", "steer");
  const receipts = path.join(project.dir, ".rudder", "steer-receipts");
  fs.mkdirSync(inbox, { recursive: true });
  const safeId = (taskId || "conductor").replace(/[^A-Za-z0-9_.-]/g, "-").slice(0, 64) || "conductor";
  const name = `${String(Date.now()).padStart(13, "0")}-${String(process.hrtime.bigint()).padStart(20, "0")}-${safeId}-${requestId}.json`;
  const file = path.join(inbox, name);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ requestId, kind, taskId, instruction, ts: new Date().toISOString() }));
  fs.renameSync(tmp, file);
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
  const state = { type: "state", projects: list, tailnet: { ...tailnet } };
  const json = JSON.stringify(state);
  if (json === lastStateJson) {
    return;
  }
  lastStateJson = json;
  sendControl(state);
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
  agents.sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0));
  return agents.slice(0, 60);
}

const lastMessageCache = new Map(); // runId -> { mtime, text }

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
    title: (record.taskSummary || record.task || record.id || "").toString().slice(0, 200),
    task: (record.task || "").toString().slice(0, 2000),
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
  };
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
  for (const name of [".claude", ".codex", ".rudder", ".config", ".local/share/opencode"]) {
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

let vaultRefreshedAt = 0;

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

function configureGitCredentials(secrets) {
  if (fs.existsSync(path.join(HOME, ".config", "gh", "hosts.yml"))) {
    shSoft("command -v gh >/dev/null 2>&1 && gh auth setup-git 2>/dev/null");
    return;
  }
  const token = secrets?.env?.GITHUB_TOKEN || secrets?.env?.GH_TOKEN || "";
  if (!token) {
    log("no GitHub credentials in the vault; private clones and pushes will fail (run `rudder cloud secrets sync`)");
    return;
  }
  fs.writeFileSync(path.join(HOME, ".git-credentials"), `https://x-access-token:${token}@github.com\n`, { mode: 0o600 });
  shSoft("git config --global credential.helper store");
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
