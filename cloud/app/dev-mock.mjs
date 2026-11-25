// Local development server for the Rudder phone app. Needs cloud's
// dependencies (npm --prefix cloud ci) for the app build.
//   PORT=4321 node cloud/app/dev-mock.mjs
// Serves /app/* built from this folder, as Cloud does, and mocks /api/app/*.
// Mock helpers: /app/__mock/signout (simulate signed out), /app/login (sign back in),
// /app/__mock/home/<running|stopped|failed|none> (machine states),
// /app/__mock/creds/<out|claude|both|expired|two|three> (agent sign-ins; two, three: more Claude accounts),
// /app/__mock/usage/<ok|old> (plan usage, or the error of a home too old to report it),
// /app/__mock/subagents/<ok|old> (the chat's subagent tray, or a home too old to list them),
// /app/__mock/compact/<live|done|off> (run_a1 compacting its context now, then the marker it leaves),
// /app/__mock/close/<ok|fail> (Close an agent, or the dashboard refusing it),
// /app/__mock/send/<ok|menu|prompt|queued|lost> (how the next messages go: delivered, refused for a
// menu or a prompt in the agent's pane, never taken by the dashboard, or taken but never in the transcript),
// /app/__mock/harness/<ok|old> (move a chat to another harness, or a home too old to),
// /app/__mock/notes/<ok|old> (a chat's notes, or a home too old to keep them),
// /app/__mock/search/<ok|old|slow> (Search everything's messages, a home too old to search them, or a slow one;
// run_a4 is a 900-message chat whose word "kestrel" is on its first, second and third pages, for Cmd+F),
// /app/__mock/file-ref/<ok|old> (files a chat message names, or a home too old to open them),
// /app/__mock/all/<ok|old|stuck|lost|codex> (the All agents chat and the ways it fails to start),
// /app/__mock/update/<ok|fail> (how the next Update now of an old machine goes: home-update.js),
// /app/__mock/fresh (a brand new account: no GitHub, no agent login, no repos),
// /app/__mock/computer (prints a code to approve, as `rudder login` would),
// /app/__mock/tailscale/<on|off|login|approval|connecting|error|expired> (the Tailscale step's states), and
// /app/__mock/tailscale-flow/<ok|approval|refused> (how the next Connect goes),
// /app/__mock/integrations/<on|off|unset>,
// /app/__mock/invite (prints a join link), /app/__mock/files/agent-edit (an agent rewrites README.md
// in run_a1's workspace, to see the Files panel's conflict banner). Orgs, invites, Slack and a plan are mocked too,
// and so is the team view (two teammates in Exla, one asleep; uses cloud/dist/team.js).
// The digest uses the real worker/digest.mjs and cloud/dist/digest.js (build cloud first).
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, randomUUID } from "node:crypto";
import { digestRun } from "../worker/digest.mjs";
import { readTranscript, showAttachments } from "../worker/transcripts.mjs";
import { buildShell, buildVendor } from "../scripts/build-app.mjs";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PORT || 4321);
const MIN = 60_000;
const now = () => Date.now();
const iso = (ms) => new Date(ms).toISOString();

let signedOut = process.env.MOCK_SIGNED_OUT === "1";
let homeMode = "running";
let usageMode = "ok";
let trayMode = "ok";
// /app/__mock/compact/live: run_a1 is compacting (transcript `compacting`,
// which a home reads off the pane), its context near the threshold; done: the
// marker it left and the context after it (compaction.js).
let compactMode = "off";
let compactSince = 0;
let closeMode = "ok";
let sendMode = "ok";
let harnessMode = "ok";
let notesMode = "ok";
let fileRefMode = "ok";
// /app/__mock/search/<ok|old|slow>: Search everything's messages, a home too
// old to search them, or one that takes a while (global-search.js).
let searchMode = "ok";
// Who the machine's agent accounts sign in as (each harness a list, "main"
// first, as home.mjs checkCredentials reports), and sign-ins in flight.
const PLANS = { claude: "max", codex: "pro" };
const signedIn = (provider, id = "main", email = "viraat@example.com") =>
  ({ id, default: id === "main", ok: true, account: email, plan: PLANS[provider] ?? null, detail: null });
const OUT = [{ id: "main", default: true, ok: false, signedIn: false, account: null, detail: "not logged in" }];
// OpenCode lists the providers it holds; Antigravity reports its Google account, no plan.
const OTHERS = () => ({
  opencode: [{ id: "main", default: true, ok: true, account: "ChatGPT, OpenCode Zen", plan: null, detail: null }],
  antigravity: [signedIn("antigravity", "main", "viraat.personal@example.com")],
});
const CREDS = {
  out: () => ({ claude: OUT, codex: OUT, opencode: [{ ...OUT[0], ok: true, account: "Free models only", detail: null }], antigravity: OUT }),
  claude: () => ({ claude: [signedIn("claude")], codex: OUT, ...OTHERS() }),
  both: () => ({ claude: [signedIn("claude")], codex: [signedIn("codex")], ...OTHERS() }),
  expired: () => ({ claude: [{ id: "main", default: true, ok: false, signedIn: false, account: "viraat@example.com", plan: "max", detail: "login expired" }], codex: [signedIn("codex")], ...OTHERS() }),
  two: () => ({ claude: [signedIn("claude"), { ...signedIn("claude", "mi6x2k3a", "work@example.com"), plan: "pro" }], codex: [signedIn("codex")], ...OTHERS() }),
  // Three Claude accounts: the default, one at its weekly limit, one whose sign-in expired.
  three: () => ({
    claude: [
      signedIn("claude"),
      { ...signedIn("claude", "mi6x2k3a", "work.account@example.com"), plan: "team", ok: false, signedIn: true, usageLimit: { window: "weekly", resetsAt: now() + 2 * 24 * 60 * MIN }, detail: "Reached the weekly usage limit" },
      { ...signedIn("claude", "mj7y3l4b", "side.project@example.com"), plan: "pro", ok: false, signedIn: false, detail: "login expired" },
    ],
    codex: [signedIn("codex")],
    ...OTHERS(),
  }),
};
// One account of a harness, signed in (or added) on the mock machine.
const putAccount = (provider, row) => {
  const list = creds[provider] ?? [];
  creds = { ...creds, [provider]: list.some((a) => a.id === row.id) ? list.map((a) => (a.id === row.id ? row : a)) : [...list, row] };
};
const accountRow = (provider, id) => (creds[provider] ?? []).find((a) => a.id === id);
let creds = CREDS.claude();
const logins = {};
// A sign-in that finished: main signs in as the user, an added one as work.
const mockSignedIn = (provider, account) => {
  const was = accountRow(provider, account);
  return { ...signedIn(provider, account, account === "main" ? "viraat@example.com" : "work@example.com"), default: was?.default ?? account === "main" };
};
// What handoff.mjs handoffRefusal says about an account a chat moves to.
const mockRefusal = (provider, account, label) => {
  const row = accountRow(provider, account);
  if (!creds[provider]) return null;
  if (!row) return `That ${label} account is no longer on your cloud machine.`;
  return row.ok ? null : `${label} isn't signed in on your cloud machine${row.account ? ` as ${row.account}` : ""}.`;
};
// GitHub in the vault, and the device flow the phone runs (approves itself after a few polls).
let githubConnected = true;
let githubFlow = null;
// Computers waiting on `rudder login`: code -> { deviceName, expiresAt }.
const computers = new Map();
let freshProjects = null;
// The "All agents" chat's hidden project: null until the app opens it.
let coordinator = null;
// /app/__mock/all/<ok|old|stuck|lost|codex>: old, a home too old for it (Cloud
// fails the project); stuck, its dashboard never starts; lost, the state never
// carries it (Cloud 5.0.5); codex, the chat runs on Codex, which calls the
// tools from its code tool. Switching keeps the chats already started.
let allMode = "ok";
// The machine's Rudder and Update now (home-update.js, POST /api/app/home/update).
// all/old puts it on 5.0.5 and usage/old on 4.4.2; an update that goes (update/ok)
// keeps the old machine connected a few seconds, then restarts it and brings
// it back on Cloud's version. One that fails (update/fail) reports why, as a
// host that could not pull the image does.
const CLOUD_VERSION = "5.0.10";
let homeVersion = CLOUD_VERSION;
let updateMode = "ok";
let updating = false;
let updateError = null;
let restarting = false;
const coordinatorHealth = () => ({
  dashboard: { old: "failed", stuck: "starting" }[allMode] ?? "running",
  failureReason: allMode === "old" ? `Your cloud machine runs Rudder ${homeVersion}. The All agents chat needs a newer version.` : null,
  needsUpdate: allMode === "old",
});
const T0 = now();

// Tailscale on the machine (tailscale-step.js, worker tailscale-login.mjs).
let tailscaleMode = "on";
let tailscaleFlow = "ok";
let tailscaleLogin = null;
const tailnet = () => {
  const off = { hostname: "rudder-home", dnsName: null, online: false, https: null, backendState: homeMode === "running" ? "NeedsLogin" : null, tailnetName: null, error: null, login: tailscaleLogin };
  if (homeMode !== "running") return off;
  if (tailscaleMode === "on") return { ...off, dnsName: "rudder-home.tail4c2e1.ts.net", online: true, https: true, backendState: "Running", tailnetName: "example.com" };
  if (tailscaleMode === "approval") return { ...off, backendState: "NeedsMachineAuth" };
  if (tailscaleMode === "connecting") return { ...off, backendState: "Starting" };
  if (tailscaleMode === "error") return { ...off, backendState: null, error: "tailscaled exited (1): listen udp4 :41641: bind: address already in use" };
  return off;
};
const tailscaleSession = (status, error = null) => ({ id: `ts-${now().toString(36)}`, status, error, startedAt: iso(now()), endedAt: status === "failed" ? iso(now()) : null });

const home = () => homeMode === "none" ? null : ({
  id: "ws_home_7f3a",
  status: homeMode,
  machineState: homeMode === "running" ? "started" : homeMode === "stopped" ? "suspended" : homeMode === "queued" ? "starting" : "failed",
  connected: homeMode === "running" && !restarting,
  stateLive: homeMode === "running" && !restarting,
  failureReason: homeMode === "failed" ? "Fly machine failed health checks after boot (exit 137, out of memory)." : null,
  hostImage: `registry.fly.io/rudder-workers:${homeVersion}`,
  version: homeVersion,
  updateTo: homeVersion === CLOUD_VERSION ? null : CLOUD_VERSION,
  updating,
  updateError,
  activeAgents: projects.reduce((n, p) => n + p.agents.filter((a) => a.status === "working").length, 0),
  lastActivityAt: iso(now() - 2 * MIN),
  tailnet: tailnet(),
  credentials: homeMode === "running" ? { ...creds, github: githubConnected ? { ok: true, account: "viraatdas", detail: null } : { ok: false, account: null, detail: "no GitHub token in the vault" }, checkedAt: iso(now()) } : null,
  logins,
});

// The names a home gives these models (src/models.ts modelName): agents, plan
// nodes and digest runs carry them as `modelName`.
const MODEL_NAMES = {
  opus: "Opus 5.5", sonnet: "Sonnet 5.5", "claude-opus-5-5": "Opus 5.5", "claude-sonnet-5-5": "Sonnet 5.5",
  "gpt-6.1-sol": "Sol 6.1", "gpt-5.5": "GPT-5.5", "anthropic/claude-sonnet": "Claude Sonnet",
};
const named = (o) => ({ ...o, modelName: MODEL_NAMES[o.model] ?? null });
const agent = (o) => named({
  phase: null, model: null, mode: "execute", lastMessage: null, summary: null, nodeId: null, planId: null, remoteUrl: null, visibility: "team",
  createdAt: T0 - 90 * MIN, updatedAt: T0 - 10 * MIN, ...o,
});

const projects = [
  {
    workspaceId: "wsp_rudder", name: "rudder", repo: "exla-ai/rudder", ref: "main", status: "running", failureReason: null,
    dashboard: "running", clients: 1, lastActivityAt: iso(T0 - 1 * MIN),
    warm: { status: "ready", key: "11eec25736c0", commit: "11eec25736c07beeba277325b1be8424e6926d6a", layers: 3, maxLayers: 8, builtAt: iso(T0 - 14 * MIN), forkCount: 5 },
    agents: [
      agent({ id: "run_a1", title: "Phone app for agent control", task: "Build the phone-first web app under cloud/app", status: "working", backend: "claude", model: "opus", mode: "main", workspaceMode: { kind: "overlay-fork", snapshotKey: "11eec25736c0", layers: 3 }, updatedAt: T0 - 1 * MIN, lastMessage: "Wiring the transcript poll to keep scroll pinned only when you are already at the bottom.", remoteUrl: "https://claude.ai/code/session_01Abc" }),
      agent({ id: "run_a2", title: "Fix flaky Ctrl+W chord test", task: "Stabilize the e2e chord tests", status: "waiting", backend: "codex", model: "gpt-6.1-sol", phase: "permission", workspaceMode: { kind: "overlay-fork", snapshotKey: "11eec25736c0", layers: 3 }, updatedAt: T0 - 4 * MIN, lastMessage: "I need permission to run `cargo test --release` with network access. Allow?" }),
      agent({ id: "run_a3", title: "Tailnet HTTPS for previews", task: "Expose dev servers over tailnet https", status: "review", visibility: "private", backend: "claude", model: "sonnet", nodeId: "n2", planId: "plan_1", workspaceMode: { kind: "overlay-fork", snapshotKey: "de4609ec9c06", layers: 2 }, updatedAt: T0 - 18 * MIN, lastMessage: "Done. Ports 3000 and 5173 are reachable at https://rudder-home.tail4c2e1.ts.net. Ready to merge." }),
      agent({ id: "run_a4", title: "Delete cloud workspaces safely", task: "Request + reaper for deletes", status: "done", backend: "claude", model: "opus", workspaceMode: { kind: "plain" }, updatedAt: T0 - 3 * 60 * MIN, lastMessage: "Added the deletion reaper with a 30s sweep and 410s for dead rows." }),
      agent({ id: "run_a5", title: "Bump jj to 0.40 flags", task: "jj rebase -o", status: "merged", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 26 * 60 * MIN, lastMessage: "Merged into main." }),
      agent({ id: "run_o1", title: "Plan: preview links everywhere", task: "Make every dev server reachable from the phone", status: "waiting", backend: "claude", model: "opus", mode: "rudder-plan", planId: "plan_1", updatedAt: T0 - 3 * MIN, lastMessage: "Here is the plan: 5 tasks. Approve it to launch, or tell me what to change." }),
      agent({ id: "run_a7", title: "Port scanner for the home", task: "List listening ports per project", status: "working", backend: "codex", model: "gpt-6.1-sol", nodeId: "n1", planId: "plan_1", updatedAt: T0 - 2 * MIN, lastMessage: "Reading /proc/net/tcp6 for owner pids." }),
      agent({ id: "run_a6", title: "Explore opencode resume", task: "opencode session resume", status: "stopped", backend: "opencode", model: "anthropic/claude-sonnet", updatedAt: T0 - 50 * 60 * MIN, lastMessage: "Stopped by you." }),
    ],
    // Show closed: chats deleted in the last day (worker closed-chats.mjs).
    closed: [
      named({ id: "run_c2", title: "Try the new search ranking", backend: "codex", model: "gpt-6.1-sol", effort: null, closedAt: T0 - 25 * MIN }),
      named({ id: "run_c1", title: "general", backend: "claude", model: "opus", effort: null, closedAt: T0 - 2 * 60 * MIN }),
    ],
    plan: {
      updatedAt: iso(T0 - 3 * MIN),
      awaitingApproval: true,
      plans: [{ id: "plan_1", awaitingApproval: true, request: "Make every dev server on the home reachable from the phone" }],
      nodes: [
        { id: "n0", title: "Tailscale in the worker image", status: "merged", runId: "run_a5", backend: "codex", model: "gpt-6.1-sol", deps: [] },
        { id: "n1", title: "Port scanner for the home", status: "running", runId: "run_a7", backend: "codex", model: "gpt-6.1-sol", deps: [{ on: "n0", type: "hard" }] },
        { id: "n2", title: "Tailnet HTTPS for previews", status: "review", runId: "run_a3", backend: "claude", model: "sonnet", deps: [{ on: "n0", type: "hard" }] },
        { id: "n3", title: "Preview links in the phone app", status: "planned", runId: null, backend: "claude", model: "opus", deps: [{ on: "n1", type: "hard" }, { on: "n2", type: "hard" }] },
        { id: "n4", title: "Docs for preview links", status: "ready", runId: null, backend: "claude", model: "sonnet", deps: [{ on: "n2", type: "soft" }] },
        { id: "n5", title: "Plain http fallback", status: "failed", runId: "run_a4", backend: "claude", model: "opus", deps: [{ on: "n2", type: "hard" }] },
      ],
    },
    ports: [
      { port: 3000, url: "https://rudder-home.tail4c2e1.ts.net:3000", label: "board" },
      { port: 5173, url: "https://rudder-home.tail4c2e1.ts.net:5173", label: "vite dev" },
    ],
  },
  {
    workspaceId: "wsp_wager", name: "wagerpals", repo: "viraatdas/wagerpals", ref: null, status: "running", failureReason: null,
    dashboard: "running", clients: 0, lastActivityAt: iso(T0 - 12 * MIN),
    warm: { status: "building", key: "5534bd3ca103", commit: "5534bd3ca103fea927302f18943a36734399a0e1", layers: 7, maxLayers: 8, builtAt: iso(T0 - 42 * MIN), forkCount: 3 },
    agents: [
      agent({ id: "run_b1", title: "Settle bets on resolution", task: "Auto-settle when a market resolves", status: "working", backend: "claude", model: "opus", updatedAt: T0 - 2 * MIN, lastMessage: "Running the settlement test suite (42 of 118 passing so far)." }),
      agent({ id: "run_b2", title: "Stripe payout webhook", task: "Handle payout.failed", status: "failed", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 35 * MIN, lastMessage: "Error: STRIPE_WEBHOOK_SECRET is not set in this environment." }),
      agent({ id: "run_b3", title: "Leaderboard query perf", task: "Speed up leaderboard", status: "done", backend: "claude", model: "sonnet", updatedAt: T0 - 5 * 60 * MIN, lastMessage: "Leaderboard p95 went from 840ms to 62ms with a covering index." }),
    ],
    ports: [{ port: 3000, url: "https://rudder-home.tail4c2e1.ts.net:3001", label: "next dev" }],
  },
  {
    workspaceId: "wsp_site", name: "rudder-site", repo: "viraatdas/rudder-site", ref: "launch", status: "running", failureReason: null,
    dashboard: "cloning", clients: 0, lastActivityAt: iso(T0 - 3 * 24 * 60 * MIN),
    agents: [],
    ports: [],
  },
];
// No project has been moved yet (POST /api/app/project-order).
for (const p of projects) p.sortOrder = null;
for (const p of projects) if (p.plan) p.plan.nodes = p.plan.nodes.map(named);

const GB = 1024 ** 3;
const MB = 1024 ** 2;
const mockMachineState = {
  sccacheLocalHits: 1284,
  sccacheRemoteHits: 312,
  sccacheMisses: 184,
  sccacheWrites: 179,
  sccacheBytesRead: 612 * MB,
  sccacheBytesWritten: 418 * MB,
  turboHits: 84,
  turboMisses: 14,
  turboWrites: 14,
  turboBytesRead: 295 * MB,
  turboBytesWritten: 112 * MB,
  toolCacheSizes: {
    sccache: 14.2 * GB,
    pnpm: 4.1 * GB,
    npm: 3.6 * GB,
    uv: 1.1 * GB,
    pip: 140 * MB,
    cargo: 1.05 * GB,
    turbo: 420 * MB,
    nx: 16 * MB,
  },
  recent: [
    { at: T0 - 4_000, tool: "sccache", op: "hit", key: "a/4/a4f891bc3d209e1f0882", bytes: 4.2 * MB, ms: 38 },
    { at: T0 - 9_000, tool: "sccache", op: "hit", key: "c/9/c91d02fa7481bc331029", bytes: 1.8 * MB, ms: 29 },
    { at: T0 - 15_000, tool: "turbo", op: "hit", key: "8f41c09e2a7d104b", bytes: 6.4 * MB, ms: 54 },
    { at: T0 - 22_000, tool: "sccache", op: "put", key: "7/e/7e3301ab9012f7cc4a18", bytes: 2.9 * MB, ms: 81 },
    { at: T0 - 31_000, tool: "sccache", op: "miss", key: "7/e/7e3301ab9012f7cc4a18", bytes: 0, ms: 19 },
  ],
};

function mockMachineTick() {
  if (homeMode !== "running") return;
  const ts = now();
  mockMachineState.sccacheRemoteHits += 1;
  mockMachineState.sccacheLocalHits += 2;
  mockMachineState.sccacheBytesRead += Math.round(1.6 * MB);
  const key = `${Math.floor(Math.random() * 16).toString(16)}/${Math.floor(Math.random() * 16).toString(16)}/${ts.toString(16)}9a4b`;
  mockMachineState.recent.unshift({ at: ts, tool: "sccache", op: "hit", key, bytes: Math.round(1.6 * MB), ms: 34 });
  if (mockMachineState.recent.length > 20) mockMachineState.recent.length = 20;
}

function mockMachinePrune() {
  const before = Object.values(mockMachineState.toolCacheSizes).reduce((a, b) => a + b, 0);
  mockMachineState.toolCacheSizes.npm = Math.min(mockMachineState.toolCacheSizes.npm, 1.8 * GB);
  mockMachineState.toolCacheSizes.pnpm = Math.min(mockMachineState.toolCacheSizes.pnpm, 2.4 * GB);
  const after = Object.values(mockMachineState.toolCacheSizes).reduce((a, b) => a + b, 0);
  const toolCacheFreedBytes = Math.max(0, Math.round(before - after));
  return { ok: true, toolCacheFreedBytes, warmFreedBytes: 0, freedBytes: toolCacheFreedBytes };
}

function mockMachinePayload(running) {
  const ts = now();
  const s = mockMachineState;
  const totalHits = s.sccacheLocalHits + s.sccacheRemoteHits;
  const hitRate = Math.round((totalHits / Math.max(1, totalHits + s.sccacheMisses)) * 1000) / 10;
  const limits = { sccache: 20 * GB, pnpm: 10 * GB, npm: 5 * GB, uv: 10 * GB, pip: 5 * GB, cargo: 5 * GB, turbo: 5 * GB, nx: 5 * GB };
  const entries = Object.entries(limits).map(([name, limitBytes]) => ({
    name,
    bytes: Math.round(s.toolCacheSizes[name] ?? 0),
    limitBytes,
    fileCount: name === "sccache" ? 28410 : name === "pnpm" ? 14200 : 1240,
    recentFiles24h: name === "sccache" ? 4120 : name === "npm" ? 310 : 42,
    lastWriteAt: ts - (name === "sccache" ? 5_000 : 18 * MIN),
  }));
  const remoteSccache = {
    hits: s.sccacheRemoteHits,
    misses: s.sccacheMisses,
    writes: s.sccacheWrites,
    errors: 0,
    bytesRead: s.sccacheBytesRead,
    bytesWritten: s.sccacheBytesWritten,
    lastActivityAt: s.recent[0]?.at ?? ts,
  };
  const remoteTurbo = {
    hits: s.turboHits,
    misses: s.turboMisses,
    writes: s.turboWrites,
    errors: 0,
    bytesRead: s.turboBytesRead,
    bytesWritten: s.turboBytesWritten,
    lastActivityAt: ts - 15_000,
  };
  return {
    homeConnected: running,
    homeStatus: homeMode,
    hostKind: "external",
    version: homeVersion,
    collectedAt: ts,
    host: running ? {
      memTotalBytes: 64 * GB,
      memAvailableBytes: 41.2 * GB,
      swapTotalBytes: 4 * GB,
      swapFreeBytes: 3.9 * GB,
      cpus: 16,
      load1: 2.14,
      load5: 1.85,
      load15: 1.62,
      disks: [
        { mount: "/workspace", totalBytes: 492 * GB, freeBytes: 218 * GB },
        { mount: "/", totalBytes: 77 * GB, freeBytes: 41 * GB },
      ],
      pressure: { memorySome: 0.2, memoryFull: 0, ioSome: 1.1, cpuSome: 2.4 },
    } : null,
    vfs: running ? {
      activeMode: "overlayfs",
      fuseReady: true,
      fuseMounts: 0,
      overlayMounts: 8,
      totalForks: 8,
      totalPlainWorkspaces: 1,
      totalWarmBytes: Math.round(14.8 * GB),
      projects: [
        {
          workspaceId: "wsp_rudder", name: "rudder", repo: "exla-ai/rudder",
          baseCommit: "11eec25736c0", status: "ready", latestKey: "11eec25736c0",
          latestCommit: "11eec25736c07beeba277325b1be8424e6926d6a",
          builtAt: iso(ts - 14 * MIN), buildSeconds: 20.4,
          layers: 3, maxLayers: 8, readyCount: 2, failedCount: 0, lastError: null,
          warmBytes: Math.round(8.4 * GB),
          forks: [
            { runId: "run_a1", snapshotKey: "11eec25736c0", layers: 3, upperFiles: 18, active: true },
            { runId: "run_a2", snapshotKey: "11eec25736c0", layers: 3, upperFiles: 7, active: true },
            { runId: "run_a3", snapshotKey: "de4609ec9c06", layers: 2, upperFiles: 31, active: false },
          ],
          plainWorkspaces: ["run_a4"],
        },
        {
          workspaceId: "wsp_wager", name: "wagerpals", repo: "viraatdas/wagerpals",
          baseCommit: "7b2a910c44e1", status: "building", latestKey: "5534bd3ca103",
          latestCommit: "5534bd3ca103fea927302f18943a36734399a0e1",
          builtAt: iso(ts - 42 * MIN), buildSeconds: 11.2,
          layers: 7, maxLayers: 8, readyCount: 3, failedCount: 0, lastError: null,
          warmBytes: Math.round(6.4 * GB),
          forks: [
            { runId: "run_b1", snapshotKey: "5534bd3ca103", layers: 7, upperFiles: 44, active: true },
            { runId: "run_b2", snapshotKey: "5534bd3ca103", layers: 7, upperFiles: 9, active: false },
          ],
          plainWorkspaces: [],
        },
      ],
    } : null,
    toolCache: running ? {
      root: "/home/rudder/.cache/rudder/tool-cache",
      totalBytes: entries.reduce((sum, e) => sum + e.bytes, 0),
      entries,
      sccache: {
        available: true,
        version: "0.18.0",
        mode: "multilevel",
        cacheLocation: "Local disk + Cloud S3 WebDAV",
        activeDaemons: 3,
        compileRequests: totalHits + s.sccacheMisses,
        cacheHits: totalHits,
        cacheMisses: s.sccacheMisses,
        hitRate,
        nonCacheable: 12,
        errors: 0,
        avgReadHitMs: 18,
        avgCompilerMs: 1840,
        levels: [
          { name: "Local NVMe (L0)", hits: s.sccacheLocalHits, misses: s.sccacheRemoteHits + s.sccacheMisses, writes: s.sccacheWrites, backfills: s.sccacheRemoteHits },
          { name: "Cloud S3 WebDAV (L1)", hits: s.sccacheRemoteHits, misses: s.sccacheMisses, writes: s.sccacheWrites, backfills: 0 },
        ],
      },
    } : null,
    remoteCache: running ? {
      proxyActive: true,
      proxyUrl: "http://127.0.0.1:41289",
      upstreamConnected: true,
      sccache: remoteSccache,
      turbo: remoteTurbo,
      recent: s.recent.slice(0, 15),
    } : null,
    cloud: {
      remoteCache: {
        enabled: true,
        bucket: "rudder-fabric-v1-597088032164-us-east-1",
        sccache: remoteSccache,
        turbo: remoteTurbo,
        recent: s.recent.slice(0, 15),
      },
      fabricCas: {
        enabled: true,
        bucket: "rudder-fabric-v1-597088032164-us-east-1",
        requests: 0,
        gets: 0,
        puts: 0,
        rootsPublished: 0,
        bytesRead: 0,
        bytesWritten: 0,
        errors: 0,
        lastActivityAt: null,
        proofCache: { entries: 0, estimated_bytes: 0, max_bytes: 32 * MB, hits: 0, misses: 0 },
      },
      burstPool: {
        enabled: false,
        policy: null,
        builders: [],
      },
    },
  };
}


// Morning digests: run records as the dashboard writes them, through the real
// worker reader and the control plane's composer.
const digestLib = await import("../dist/digest.js").catch(() => null);
const HOUR = 60 * MIN;
const digestRecords = (end) => ({
  wsp_rudder: [
    { id: "run_a1", taskSummary: "Phone app for agent control", task: "Build the phone-first web app under cloud/app", status: "running", backend: "claude", model: "opus", createdAt: end - 14 * HOUR, updatedAt: end - 20 * MIN,
      turns: [{ prompt: "Build the phone-first web app", ts: end - 14 * HOUR }, { prompt: "make the transcript sticky at the bottom", ts: end - 9 * HOUR }, { prompt: "also show tool calls collapsed", ts: end - 3 * HOUR }] },
    { id: "run_a3", taskSummary: "Tailnet HTTPS for previews", task: "Expose dev servers over tailnet https", status: "completed", lifecyclePhase: "review", backend: "claude", model: "sonnet", nodeId: "n2", createdAt: end - 11 * HOUR, updatedAt: end - 7 * HOUR,
      doneSummary: "Listening ports on the home machine get `tailscale serve --https`; falls back to plain http when tailnet certs are off.", publish: { url: "https://github.com/exla-ai/rudder/pull/212", number: 212 }, turns: [] },
    { id: "run_a4", taskSummary: "Delete cloud workspaces safely", task: "Request + reaper for deletes", status: "completed", lifecyclePhase: "deployed", backend: "claude", model: "opus", createdAt: end - 20 * HOUR, updatedAt: end - 12 * HOUR,
      doneSummary: "Added the deletion reaper with a 30s sweep; dead rows answer 410 to heartbeats.", delivery: { status: "deployed", target: "rudder-cloud-control (Fly)", verifiedAt: iso(end - 12 * HOUR + 5 * MIN) }, turns: [] },
    { id: "run_a5", taskSummary: "Bump jj to 0.40 flags", task: "jj rebase -o", status: "merged", backend: "codex", model: "gpt-6.1-sol", createdAt: end - 6 * HOUR, updatedAt: end - 5 * HOUR,
      doneSummary: "Switched rebase to -o/--onto with a fallback for older jj.", merge: { status: "merged", pushed: true, pushedAt: end - 5 * HOUR + 2 * MIN, targetBranch: "main" }, turns: [] },
  ],
  wsp_wager: [
    { id: "run_b2", taskSummary: "Stripe payout webhook", task: "Handle payout.failed", status: "failed", backend: "codex", model: "gpt-6.1-sol", createdAt: end - 4 * HOUR, updatedAt: end - 3.5 * HOUR, turns: [] },
    { id: "run_b3", taskSummary: "Leaderboard query perf", task: "Speed up leaderboard", status: "completed", backend: "claude", model: "sonnet", createdAt: end - 10 * HOUR, updatedAt: end - 8 * HOUR,
      doneSummary: "Leaderboard p95 went from 840ms to 62ms with a covering index on (season, points).", turns: [] },
    { id: "run_b4", taskSummary: "Ask before placing a bet", task: "Confirm step for bets", status: "running", attention: "permission", backend: "claude", model: "opus", createdAt: end - 50 * MIN, updatedAt: end - 30 * MIN, turns: [] },
  ],
});
function mockDigest(day, end, live = false) {
  if (!digestLib) return null;
  const since = end - 24 * HOUR;
  const records = digestRecords(end);
  const raw = projects.filter((p) => records[p.workspaceId]).map((p) => ({
    workspaceId: p.workspaceId, name: p.name, repo: p.repo,
    runs: records[p.workspaceId].map((r) => digestRun(r, { since, until: end })).filter(Boolean).map(named),
  }));
  return digestLib.composeDigest(raw, { day, timeZone: "America/Los_Angeles", since, until: end, generatedAt: end });
}
const digestDay = (ms) => new Date(ms - 7 * HOUR).toISOString().slice(0, 10);
const morning = (() => { const d = new Date(); d.setHours(9, 1, 0, 0); return d.getTime() > now() ? d.getTime() - 24 * HOUR : d.getTime(); })();
const digestStore = [0, 1, 2].map((k) => {
  const end = morning - k * 24 * HOUR;
  return { id: `dg_${k}`, readAt: k ? iso(end + HOUR) : null, createdAt: iso(end), body: mockDigest(digestDay(end), end) };
}).filter((d) => d.body);
const digestPreviewOf = (d) => d && ({ id: d.id, readAt: d.readAt, createdAt: d.createdAt, since: d.body.since, until: d.body.until, ...digestLib.digestPreview(d.body) });

const LONG_OUTPUT = Array.from({ length: 140 }, (_, i) => `test app_tests::case_${String(i).padStart(3, "0")}_${["scroll", "merge", "leader", "render", "signals"][i % 5]} ... ok`).join("\n") + "\n\ntest result: ok. 528 passed; 0 failed; 1 ignored; finished in 41.82s";

// The prompts a transcript offers to edit (worker/session-lineage.mjs
// editablePrompts): here every user message.
const mockEdits = (msgs) => msgs.filter((m) => m.role === "user").map((m) => ({ messageId: m.id, entry: m.id, text: m.text }));
// Request id -> { status, error }: what the dashboard did with a queued edit.
const steerReceipts = new Map();

function baseTranscript(a) {
  const t = a.createdAt;
  let k = 0;
  const id = () => `m${++k}`;
  const msgs = [
    { id: id(), role: "user", ts: t, text: a.task + ". Keep it phone-first and make transcripts render properly: markdown, code, tool calls." },
    { id: id(), role: "assistant", ts: t + 20_000, text: "I'll start by reading how the control plane exposes workspaces, then sketch the screens.\n\n**Plan**\n1. Read `cloud/src/server.ts` for the existing routes\n2. Define the `/api/app/*` contract\n3. Build the shell, then the three screens\n4. Verify on a 393px viewport in light and dark" },
    // Tool cards name their files (worker/tool-files.mjs `files` and `found`), which open beside the chat (tool-files.js).
    { id: id(), role: "tool", ts: t + 30_000, text: "", tool: { name: "Read", input: "cloud/app/render.js", output: "  48  /** Render markdown into `el`. Falls back to escaped text until libraries load.\n  49   * `fileRefs`: chat messages, where file paths open beside the chat (file-refs.js). */\n  50  export function renderMarkdown(el, text, { fileRefs = false } = {}) {\n  51    const src = String(text ?? \"\");", isError: false,
      files: [{ path: "cloud/app/render.js", line: 48, end: 0 }] } },
    { id: id(), role: "tool", ts: t + 34_000, text: "", tool: { name: "Grep", input: "\"/api/rudder/workspace\" in cloud/src", output: "cloud/src/server.ts:1180:  if (url.pathname === \"/api/rudder/workspace\" && req.method === \"POST\") {\ncloud/src/server.ts:1244:  if (url.pathname.startsWith(\"/api/rudder/workspace/\")) {", isError: false,
      found: [{ row: 0, text: "cloud/src/server.ts:1180", path: "cloud/src/server.ts", line: 1180 }, { row: 1, text: "cloud/src/server.ts:1244", path: "cloud/src/server.ts", line: 1244 }] } },
    { id: id(), role: "assistant", ts: t + 60_000, text: "The workspace routes are keyed by `account_id`, so the app API can reuse the same session lookup. Here is the shape I'm going with:\n\n| Route | Method | Returns |\n|---|---|---|\n| `/api/app/state` | GET | home machine, projects, agents, ports |\n| `/api/app/transcript` | GET | messages for one run |\n| `/api/app/send` | POST | `{ ok: true }` |\n| `/api/app/action` | POST | stop or merge a run |\n\nThe keyed list reconcile keeps scroll and expanded tool rows stable across polls:\n\n```js\nexport function reconcile(container, items, keyOf, create, update) {\n  const existing = new Map();\n  for (const child of container.children) existing.set(child.dataset.key, child);\n  let cursor = container.firstElementChild;\n  for (const item of items) {\n    const key = String(keyOf(item));\n    let el = existing.get(key) ?? create(item);\n    update(el, item);\n    if (el !== cursor) container.insertBefore(el, cursor);\n    else cursor = cursor.nextElementSibling;\n  }\n}\n```\n\n> Note: tool output is collapsed by default and truncated past a few KB, so a 10k-line test log never stalls the phone." },
    { id: id(), role: "tool", ts: t + 90_000, text: "", tool: { name: "Bash", input: "npm run check", output: "> @exla/rudder@2.15.4 check\n> tsc --noEmit\n\nsrc/cloud.ts(418,7): error TS2322: Type 'string | undefined' is not assignable to type 'string'.\n  Type 'undefined' is not assignable to type 'string'.", isError: true } },
    { id: id(), role: "assistant", ts: t + 100_000, text: "Typecheck caught a missing fallback in `cloud.ts`. Fixing it:" },
    { id: id(), role: "tool", ts: t + 110_000, text: "", tool: { name: "Edit", input: "cloud/src/home-host.ts", output: "     RUDDER_WORKER_TOKEN: input.workerToken,\n-    RUDDER_CLOUD_URL: input.cloudUrl,\n+    RUDDER_ACCOUNT_ID: input.accountId,\n+    RUDDER_CLOUD_URL: input.cloudUrl,\n     RUDDER_HOME: \"1\",", isError: false,
      files: [{ path: "cloud/src/home-host.ts", line: 30, end: 31 }] } },
    { id: id(), role: "tool", ts: t + 150_000, text: "", tool: { name: "Bash", input: "cargo test --manifest-path native/Cargo.toml", output: LONG_OUTPUT, isError: false } },
    compactMarker(id(), t + 160_000),
    { id: id(), role: "user", ts: t + 30 * MIN, text: "Looks good. Can you make the composer stay above the keyboard on iPhone?" },
    { id: id(), role: "assistant", ts: t + 31 * MIN, text: "Yes. The shell now sizes itself to `visualViewport.height`, so the composer rides above the keyboard instead of being covered:\n\n```css\n.shell {\n  position: fixed;\n  top: var(--vvtop);\n  height: var(--vvh);\n}\n```\n\n```swift\n// The native shell injects the token before the page runs.\nlet script = WKUserScript(\n    source: \"window.RUDDER_NATIVE = { token: \\\"\\(token)\\\", platform: \\\"ios\\\" }\",\n    injectionTime: .atDocumentStart,\n    forMainFrameOnly: true\n)\n```\n\nI also check `window.innerHeight - visualViewport.height` to drop the bottom safe-area padding while the keyboard is open. See the [visualViewport docs](https://developer.mozilla.org/en-US/docs/Web/API/VisualViewport) for the event model.\n\n- [x] Composer above keyboard\n- [x] Cmd+Enter sends\n- [ ] Haptics on send (needs the native shell)" },
    { id: id(), role: "tool", ts: t + 33 * MIN, text: "", tool: { name: "WebFetch", input: "https://rudder-home.tail4c2e1.ts.net:3000/health", output: null, isError: false } },
    { id: id(), role: "assistant", ts: t + 34 * MIN, text: "Wrote it up. [Open the full Offstage snapshot plan](/home/rudder/.claude/plans/dreamy-launching-firefly.md), or the [Full Opus/Fable design](file:///home/rudder/.claude/plans/dreamy-launching-firefly.md#L9) section.\n\nThe viewer change is in `cloud/app/file-view.js:120`, the plan sits at `/home/rudder/.claude/plans/dreamy-launching-firefly.md`, and `src/missing.ts:5` is gone." },
  ];
  if (a.status === "waiting") msgs.push({ id: id(), role: "assistant", ts: a.updatedAt, text: a.lastMessage });
  else if (a.lastMessage) msgs.push({ id: id(), role: "assistant", ts: a.updatedAt, text: a.lastMessage });
  return msgs;
}

// What a working agent has running, for its Running strip (running-strip.js),
// as the worker marks it (worker/transcripts.mjs markRunning). Claude: a
// background deploy its call described, then a command with no description.
// Codex: a build that outlived its call, and the poll reading it, which the
// strip leaves out.
function runningRows(a) {
  if (a.status !== "working") return [];
  const row = (key, ts, tool) => ({ id: `${a.id}-running-${key}`, role: "tool", ts, text: "", tool: { isError: false, running: true, ...tool } });
  if (a.backend === "codex") {
    return [
      row("build", now() - 312_000, { name: "exec", input: "cd native && CARGO_TARGET_DIR=/workspace/rudder-scratch/port-scan/target cargo build --release -p rudder-native", output: "Compiling rudder-native v5.0.49", open: true }),
      row("poll", now() - 4_000, { name: "exec", input: "write_stdin", output: null, poll: true }),
    ];
  }
  return [
    row("deploy", now() - 134_000, { name: "Bash", input: "cd cloud && fly deploy --app rudder-cloud-control --image registry.fly.io/rudder-cloud-control:5.0.49 --strategy rolling", description: "Deploying Cloud", output: "Command running in background with ID: b4e66erp9.", background: { id: "b4e66erp9", status: null } }),
    row("test", now() - 9_000, { name: "Bash", input: "unset $(env | grep -o '^RUDDER_[A-Z_]*') && npm test -- --test-name-pattern transcript", output: null }),
  ];
}

// A chat three pages long (Cmd+F past the loaded page, chat-find.js): "kestrel"
// in messages 40, 41, 455 and 880 of 900, so the newest page has one, the one
// before has one and the first page two.
function longTranscript(a) {
  const start = toMsMock(a.updatedAt) - 900 * 60_000;
  const topics = ["the reaper sweep", "heartbeat 410s", "a soft delete flag", "the deletion queue", "volume snapshots", "the audit log"];
  return Array.from({ length: 900 }, (_, i) => {
    const user = i % 2 === 0;
    const topic = topics[Math.floor(i / 2) % topics.length];
    const bird = [40, 41, 455, 880].includes(i) ? " The kestrel test fixture covers it." : "";
    return {
      id: `l${i}`, role: user ? "user" : "assistant", ts: start + i * 60_000,
      text: user ? `Step ${i / 2 + 1}: what about ${topic}?${bird}` : `For ${topic}, I checked the worker and the control plane. Nothing else changes.${bird}`,
    };
  });
}

// Search everything's messages (worker/transcript-search.mjs searchChats):
// every mock chat, newest first, five a chat.
function mockSearch(q) {
  const needle = q.toLowerCase();
  const results = [];
  for (const p of [...projects, ...(coordinator ? [coordinator] : [])]) {
    for (const a of p.agents) {
      const t = transcriptFor(p.workspaceId, a.id);
      const hits = (t?.msgs || []).filter((m) => (m.role === "user" || m.role === "assistant") && m.text?.toLowerCase().includes(needle)).reverse().slice(0, 5);
      for (const m of hits) {
        const flat = m.text.replace(/\s+/g, " ").trim();
        const at = flat.toLowerCase().indexOf(needle);
        const from = Math.max(0, at - 50);
        const to = Math.min(flat.length, at + needle.length + 110);
        results.push({ workspaceId: p.workspaceId, runId: a.id, messageId: m.id, role: m.role, ts: m.ts, snippet: `${from ? "…" : ""}${flat.slice(from, to)}${to < flat.length ? "…" : ""}` });
      }
    }
  }
  return { results: results.sort((x, y) => y.ts - x.ts).slice(0, 50), partial: false };
}

// A compaction as worker/transcripts.mjs reads Claude Code's (compactMessage).
function compactMarker(id, ts, { pre = 367_082, post = 24_989, ms = 63_301 } = {}) {
  return { id, role: "system", kind: "compact", ts, text: "Context compacted", compact: { trigger: "auto", preTokens: pre, postTokens: post, durationMs: ms,
    summary: "## Primary request\nBuild the phone-first web app under `cloud/app`: transcripts render markdown, code and tool calls, and scroll stays pinned at the bottom.\n\n## Done so far\n1. Read the workspace routes in `cloud/src/server.ts` and settled the `/api/app/*` contract\n2. Built the shell and the three screens\n3. Fixed the `cloud.ts` typecheck error (`state.cloudUrl` can be undefined)\n\n## Next\n- Keep the composer above the keyboard on iPhone\n- Verify on a 393px viewport in light and dark" } };
}

// Subagents (worker/transcripts.mjs `subagent`, cloud/app/subagents.js): a
// Claude agent (run_b1) and a Codex agent (run_a7) with subagents done, failed
// and running. A running one gains a step every few seconds, as a live one would.
const subTool = (id, ts, name, input, output, isError = false, files = {}) => ({ id, role: "tool", ts, text: "", tool: { name, input, output, isError, ...files } });
const subSay = (id, ts, text, role = "assistant") => ({ id, role, ts, text });
function liveSubagent(fixed, steps, { every = 5_000, start }) {
  const shown = Math.min(steps.length, 2 + Math.floor((now() - T0) / every));
  const messages = steps.slice(0, shown);
  const done = shown === steps.length && fixed.finishes;
  return {
    ...fixed,
    status: done ? "done" : "running",
    startedAt: start,
    endedAt: done ? messages[messages.length - 1].ts : null,
    toolCalls: messages.filter((m) => m.role === "tool").length,
    result: done ? messages[messages.length - 1].text : null,
    messages,
    earlier: 0,
  };
}
function subagentTranscript(a) {
  const t = now() - 6 * MIN;
  if (a.backend === "codex") {
    const procSteps = [
      subTool("p1", t + 12_000, "exec_command", "sed -n 1,40p /proc/net/tcp6", "  sl  local_address                         rem_address                         st tx_queue rx_queue\n   0: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000"),
      subTool("p2", t + 20_000, "exec_command", "rg -n \"inode\" src/ports.rs", "src/ports.rs:41:    let inode = fields[9].parse::<u64>()?;"),
      subSay("p3", t + 31_000, "Listening sockets are state `0A`. The owning pid comes from matching the socket inode against `/proc/<pid>/fd/*` links."),
      subTool("p4", t + 40_000, "exec_command", "cargo test -p ports listen", "running 3 tests\ntest listen_v4 ... ok\ntest listen_v6 ... ok\ntest owner_pid ... ok\n\ntest result: ok. 3 passed"),
      subTool("p5", t + 47_000, "apply_patch", "src/ports.rs", "Success. Updated src/ports.rs"),
      subSay("p6", t + 58_000, "**Parsing works for IPv4 and IPv6.** State `0A` is LISTEN; the inode maps to a pid through `/proc/<pid>/fd`. Three tests pass, including the owner lookup."),
    ];
    const serveSteps = [
      subTool("s1", t + 15_000, "exec_command", "tailscale --socket /tmp/tailscaled.sock serve status --json", "{\n  \"TCP\": { \"3000\": { \"HTTPS\": true } },\n  \"Web\": { \"rudder-home.tail4c2e1.ts.net:3000\": {} }\n}"),
      subSay("s2", t + 26_000, "Serve config is keyed by port, so each scanned port maps to one HTTPS entry."),
      subTool("s3", t + 35_000, "exec_command", "rg -n \"serve\" cloud/worker/tailscale-previews.mjs", "cloud/worker/tailscale-previews.mjs:88:  await runTailnetCommand([\"serve\", \"--bg\", `--https=${port}`, String(port)]);"),
      subTool("s4", t + 50_000, "exec_command", "node --test tests/tailscale-previews.test.mjs", "✔ maps a listening port to its serve URL (4.1ms)\n✔ drops a port that stopped listening (1.9ms)"),
      subTool("s5", t + 61_000, "apply_patch", "cloud/worker/tailscale-previews.mjs", "Success. Updated cloud/worker/tailscale-previews.mjs", false,
        { files: [{ path: "/workspace/code/rudder/cloud/worker/tailscale-previews.mjs", line: 88, end: 91 }] }),
      subTool("s6", t + 70_000, "exec_command", "node --test tests/tailscale-previews.test.mjs", "✔ maps a listening port to its serve URL (3.8ms)\n✔ drops a port that stopped listening (2.0ms)\n✔ keeps HTTPS for a port that comes back (2.4ms)"),
    ];
    return [
      subSay("x1", t, a.task + ". Use subagents for the two halves.", "user"),
      subSay("x2", t + 5_000, "I'll have one subagent parse `/proc/net` and another map ports to `tailscale serve`, then join the two."),
      { ...subTool("call_proc", t + 8_000, "spawn_agent", "proc_net_parsing", "{\n  \"task_name\": \"/root/proc_net_parsing\"\n}"),
        subagent: { id: "01a1ee20-0000-7000-8000-000000000001", backend: "codex", kind: null, name: "Turing", description: "proc net parsing", prompt: null, status: "done", startedAt: t + 8_000, endedAt: t + 58_000, toolCalls: 4, result: procSteps[5].text, messages: procSteps, earlier: 0 } },
      { ...subTool("call_serve", t + 9_000, "spawn_agent", "serve_mapping", "{\n  \"task_name\": \"/root/serve_mapping\"\n}"),
        get subagent() { return liveSubagent({ id: "01a1ee20-0000-7000-8000-000000000002", backend: "codex", kind: null, name: "Hopper", description: "serve mapping", prompt: null }, serveSteps, { start: t + 9_000, every: 6_000 }); } },
      { ...subTool("call_docker", t + 10_000, "spawn_agent", "docker_ports", "agent limit reached: at most 2 subagents can run at once", true),
        subagent: { id: null, backend: "codex", kind: null, name: null, description: "docker ports", prompt: null, status: "failed", startedAt: t + 10_000, endedAt: null, toolCalls: 0, result: "agent limit reached: at most 2 subagents can run at once", messages: [], earlier: 0 } },
      subTool("x6", t + 62_000, "wait", "/root/proc_net_parsing", "{\n  \"status\": \"completed\"\n}"),
      subSay("x7", t + 70_000, "The parser is done. Waiting on the serve mapping before I wire them together."),
      // A code-mode script's patch of three files (Codex FileChange items).
      subTool("x8", t + 75_000, "exec", "apply_patch", "Script completed\nWall time 0.0 seconds\nOutput:\n{}", false,
        { files: [
          { path: "/workspace/code/rudder/cloud/worker/tailscale-previews.mjs", line: 88, end: 91 },
          { path: "/workspace/code/rudder/cloud/app/render.js", line: 0, end: 0 },
          { path: "/workspace/code/rudder/cloud/app/subagents.js", line: 21, end: 23 },
        ] }),
    ];
  }
  const mapSteps = [
    subTool("m1", t + 14_000, "Grep", "resolveMarket  in src", "src/markets/resolve.ts:12:export async function resolveMarket(id: string, outcome: Outcome) {\nsrc/jobs/poll-oracles.ts:57:      await resolveMarket(market.id, result.outcome);"),
    subTool("m2", t + 18_000, "Read", "src/markets/resolve.ts", "  12  export async function resolveMarket(id: string, outcome: Outcome) {\n  13    const market = await db.market.update({ where: { id }, data: { status: \"resolved\", outcome } });\n  14    await emit(\"market.resolved\", market);\n  15  }"),
    subTool("m3", t + 22_000, "Grep", "\"market.resolved\"  in src", "src/markets/resolve.ts:14:    await emit(\"market.resolved\", market);"),
    subSay("m4", t + 30_000, "Nothing listens for `market.resolved` yet, so bets stay open after a market resolves."),
    subTool("m5", t + 34_000, "Read", "src/bets/model.ts", "   1  export type Bet = { id: string; marketId: string; side: Side; stakeCents: number; status: \"open\" | \"won\" | \"lost\" | \"void\" };"),
    subSay("m6", t + 41_000, "**Resolution never reaches bets.** `resolveMarket` (src/markets/resolve.ts:12) marks the market resolved and emits `market.resolved`, but nothing subscribes to it.\n\n- Oracles call it from `src/jobs/poll-oracles.ts:57`\n- A bet's `status` is `open | won | lost | void`; settlement only needs to flip it and credit `stakeCents` times the odds\n\nThe natural hook is a `market.resolved` subscriber."),
  ];
  const payoutSteps = [
    subTool("r1", t + 16_000, "Read", "src/bets/payout.ts", "  8  export const payout = (stakeCents: number, odds: number) => stakeCents * odds;"),
    subSay("r2", t + 24_000, "`payout` returns fractional cents for decimal odds. Checking how Stripe expects amounts before choosing a rounding rule."),
    { ...subTool("toolu_fetch", t + 27_000, "Agent", "Stripe amount rounding", "Async agent launched successfully."),
      subagent: { id: "a51c2", backend: "claude", kind: "web-fetch", name: null, description: "Stripe amount rounding", prompt: "Find how Stripe expects payout amounts: integer or decimal, and any rounding guidance.", status: "done", startedAt: t + 27_000, endedAt: t + 49_000, toolCalls: 1,
        result: "Stripe amounts are integers in the smallest currency unit (cents). Round before sending; Stripe does not round for you.",
        messages: [subTool("w1", t + 30_000, "WebFetch", "https://docs.stripe.com/currencies#minor-units", "Amounts are integers in the currency's smallest unit."), subSay("w2", t + 49_000, "Stripe amounts are integers in the smallest currency unit (cents). Round before sending; Stripe does not round for you.")], earlier: 0 } },
    subTool("r4", t + 52_000, "Bash", "npx vitest run src/bets/payout.test.ts", " ✓ src/bets/payout.test.ts (4 tests) 6ms\n\n Test Files  1 passed (1)\n      Tests  4 passed (4)"),
    subTool("r5", t + 63_000, "Edit", "src/bets/payout.ts", "-export const payout = (stakeCents: number, odds: number) => stakeCents * odds;\n+export const payout = (stakeCents: number, odds: number) => Math.floor(stakeCents * odds);"),
    subTool("r6", t + 71_000, "Bash", "npx vitest run src/bets", " ✓ src/bets/payout.test.ts (5 tests) 7ms\n ✓ src/bets/settle.test.ts (9 tests) 15ms"),
    subSay("r7", t + 80_000, "Payouts now round down to whole cents; the house keeps the fraction. All 14 bet tests pass."),
  ];
  return [
    subSay("b1", t, a.task + ". Use subagents to research while you build.", "user"),
    subSay("b2", t + 6_000, "I'll send one subagent to map how markets resolve and another to check the payout math, and write the settlement job meanwhile."),
    { ...subTool("toolu_map", t + 10_000, "Agent", "Map market resolution flow", "Async agent launched successfully."),
      subagent: { id: "a8f03", backend: "claude", kind: "Explore", name: null, description: "Map market resolution flow", prompt: "Read-only. Find where a market becomes resolved in this repo and what, if anything, settles the bets on it. Report file:line references and the bet model's fields.", status: "done", startedAt: t + 10_000, endedAt: t + 41_000, toolCalls: 4, result: mapSteps[5].text, messages: mapSteps, earlier: 0 } },
    { ...subTool("toolu_payout", t + 12_000, "Agent", "Review payout rounding", "Async agent launched successfully."),
      get subagent() { return liveSubagent({ id: "a2d71", backend: "claude", kind: "general-purpose", name: null, description: "Review payout rounding", prompt: "Check `src/bets/payout.ts` for rounding errors with decimal odds. Fix them with a test.", finishes: false }, payoutSteps, { start: t + 12_000, every: 5_000 }); } },
    { ...subTool("toolu_old", t + 13_000, "Agent", "Backfill settled bets", "Async agent launched successfully."),
      subagent: { id: "a77e0", backend: "claude", kind: "general-purpose", name: "backfill", description: "Backfill settled bets", prompt: "Write a one-off script that settles bets on markets resolved before today.", status: "stopped", startedAt: t + 13_000, endedAt: t + 33_000, toolCalls: 2, result: null,
        messages: [subTool("k1", t + 20_000, "Bash", "psql $DATABASE_URL -c \"select count(*) from markets where status = 'resolved'\"", " count\n-------\n   318"), subTool("k2", t + 33_000, "Write", "scripts/backfill-settlement.ts", null)], earlier: 0 } },
    subTool("b6", t + 50_000, "Write", "src/jobs/settle.ts", "Wrote 46 lines to src/jobs/settle.ts"),
    // What teammates send the lead (worker/transcripts.mjs peerMessages), and a scheduled task's turn.
    { id: "b8", role: "system", kind: "peer", ts: t + 51_000, text: "Settlement rounding is in: payouts round down to whole cents. Nothing is committed.", peer: { from: "payout-review", summary: "Payout rounding fixed, not committed", idle: false, failure: null } },
    { id: "b9", role: "system", kind: "peer", ts: t + 52_000, text: "The subagent tray now shows the word \"Subagents\" and how many are running, and the cards no longer show the last command. Nothing is committed.\n\n**Collapsed bar:** it reads \"Subagents\" followed by a \"2 running\" pill. When none are running there's no count at all.\n\n**Checks:** `node --check` on `cloud/app/subagents.js` passes, and the page had no errors.\n\nFiles changed: `cloud/app/subagent-tray.js`, `cloud/app/subagent-tray.css`.", peer: { from: "tray-polish", summary: null, idle: true, failure: null } },
    { id: "b10", role: "system", kind: "peer", ts: t + 53_000, text: "", peer: { from: "backfill", summary: null, idle: true, failure: null } },
    { id: "b11", role: "system", kind: "scheduled", ts: t + 54_000, text: "Scheduled check", scheduled: { prompt: "5-minute monitor (the user's goal: \"keep the settlement work moving\").\n\n1. Run ListAgents and look at each teammate.\n2. Fix anything stuck." } },
    subSay("b7", t + 55_000, a.lastMessage),
  ];
}
const SUBAGENT_DEMO = new Set(["run_b1", "run_a7"]);

// The subagent tray (worker/subagents.mjs, cloud/app/subagent-tray.js): every
// subagent of a demo agent. Its chat's cards, plus for the Claude agent its
// teammates (one running, one done, one stopped) and subagents from earlier
// in the day whose cards scrolled out of the chat.
const trayCache = new Map();
function mockTray(a, msgs) {
  const fromChat = msgs.filter((m) => m.subagent?.id).map((m) => m.subagent);
  if (a.backend !== "claude") return fromChat;
  if (!trayCache.has(a.id)) trayCache.set(a.id, mockTeammates(now() - 6 * MIN));
  return [...fromChat, ...trayCache.get(a.id).map((s) => (typeof s === "function" ? s() : s))];
}
function mockTeammates(t) {
  const mate = (o) => ({ backend: "claude", type: "teammate", kind: null, description: null, earlier: 0, ...o });
  const settleSteps = [
    subTool("e1", t + 20_000, "Bash", "npx vitest run src/jobs/settle.test.ts", " ✓ settles a won bet\n ✓ settles a lost bet\n × voids bets on a voided market\n × ignores a second resolution\n\n Tests  2 failed | 2 passed (4)", true),
    subSay("e2", t + 34_000, "Two cases fail: a voided market should refund stakes, and a market resolved twice must not pay out twice."),
    subTool("e3", t + 41_000, "Read", "src/jobs/settle.ts", "  12  export async function settleMarket(market: Market) {\n  13    for (const bet of await openBets(market.id)) await pay(bet, market.outcome);\n  14  }"),
    subTool("e4", t + 52_000, "Edit", "src/jobs/settle.ts", "+  if (market.status === \"void\") return refundAll(market.id);\n+  if (market.settledAt) return;"),
    subTool("e5", t + 66_000, "Bash", "npx vitest run src/jobs", " ✓ src/jobs/settle.test.ts (4 tests) 9ms\n\n Tests  4 passed (4)"),
    subTool("e6", t + 74_000, "SendMessage", "team-lead", "Message sent to team-lead's inbox"),
  ];
  const docSteps = [
    subTool("d1", t + 25_000, "Read", "docs/payouts.md", "# Payouts\n\nPayouts are sent through Stripe Connect."),
    subTool("d2", t + 47_000, "Edit", "docs/payouts.md", "+## Settlement\n+\n+A market's bets settle the moment it resolves."),
    subSay("d3", t + 63_000, "Documented settlement and the rounding rule in docs/payouts.md."),
  ];
  return [
    () => liveSubagent(mate({ id: "7ea77a7e-0000-4000-8000-0000000000a1", name: "settle-tests", prompt: "Run the settlement tests, fix what fails, and message me when they pass.", finishes: false }), settleSteps, { start: t + 15_000, every: 7_000 }),
    mate({ id: "7ea77a7e-0000-4000-8000-0000000000a2", name: "payout-docs", prompt: "Document how settlement and payout rounding work in docs/payouts.md.", status: "done", startedAt: t + 16_000, endedAt: t + 63_000, toolCalls: 2, result: docSteps[2].text, messages: docSteps }),
    mate({ id: "7ea77a7e-0000-4000-8000-0000000000a3", name: "odds-feed", prompt: "Switch the odds poller to the streaming feed.", status: "stopped", startedAt: t + 18_000, endedAt: t + 29_000, toolCalls: 1, result: null,
      messages: [subTool("o1", t + 26_000, "Bash", "curl -sN https://odds.example.test/stream | head -3", null), subSay("o2", t + 29_000, "[Request interrupted by user for tool use]", "user")] }),
    { id: "a3c90", backend: "claude", type: "subagent", kind: "Explore", name: null, description: "Find the odds feed client", prompt: "Read-only. Where does the app poll for odds, and how often?", status: "done", startedAt: t - 3 * 60 * MIN, endedAt: t - 3 * 60 * MIN + 95_000, toolCalls: 3,
      result: "Odds are polled every 30s by `src/jobs/poll-oracles.ts`, through `src/odds/client.ts`.", earlier: 0,
      messages: [subTool("q1", t - 3 * 60 * MIN + 20_000, "Grep", "pollOdds  in src", "src/jobs/poll-oracles.ts:9:export async function pollOdds() {"), subSay("q2", t - 3 * 60 * MIN + 95_000, "Odds are polled every 30s by `src/jobs/poll-oracles.ts`, through `src/odds/client.ts`.")] },
    { id: "a1b44", backend: "claude", type: "subagent", kind: "general-purpose", name: null, description: "Add the bets table migration", prompt: "Add a migration for the bets table with a status column.", status: "done", startedAt: t - 4 * 60 * MIN, endedAt: t - 4 * 60 * MIN + 260_000, toolCalls: 6,
      result: "Added `migrations/0012_bets.sql` with `status` as an enum; it applies cleanly.", earlier: 4,
      messages: [subTool("g1", t - 4 * 60 * MIN + 200_000, "Bash", "npm run db:migrate", "Applied 0012_bets.sql"), subSay("g2", t - 4 * 60 * MIN + 260_000, "Added `migrations/0012_bets.sql` with `status` as an enum; it applies cleanly.")] },
  ];
}
// One line per subagent, as worker/subagents.mjs sends it.
function trayLine(s) {
  const last = s.messages.filter((m) => !(m.role === "user" && m.text.startsWith("[Request interrupted by user"))).at(-1);
  const step = (m) => !m ? null : m.role === "tool" ? `${m.tool.name} ${m.tool.input || ""}`.trim() : m.text.replace(/\s+/g, " ").trim();
  return {
    id: s.id, backend: s.backend, type: s.type || "subagent", kind: s.kind, name: s.name, description: s.description,
    status: s.status, startedAt: s.startedAt, endedAt: s.endedAt, toolCalls: s.toolCalls,
    step: s.status !== "running" && s.result ? s.result.replace(/\s+/g, " ").trim() : step(last),
  };
}

// MOCK_TRANSCRIPT=<file>: a real /api/app/transcript response (readTranscript
// output) shown as the first agent's conversation.
const realTranscript = process.env.MOCK_TRANSCRIPT ? JSON.parse(fs.readFileSync(process.env.MOCK_TRANSCRIPT, "utf8")) : null;
// MOCK_SESSION=<dir>/<uuid>.jsonl: a Claude session read by the real
// worker/transcripts.mjs as the first agent's conversation, pages and all
// ("Load earlier messages"). Its run record and Claude folder go in
// <dir>/mock-session.
const mockSession = process.env.MOCK_SESSION ? (() => {
  const file = path.resolve(process.env.MOCK_SESSION);
  const dir = path.join(path.dirname(file), "mock-session");
  const sessionId = path.basename(file, ".jsonl");
  const cwd = "/workspace/code/mock";
  const projectDir = path.join(dir, "home", ".claude", "projects", cwd.replace(/[^A-Za-z0-9-]/g, "-"));
  fs.mkdirSync(projectDir, { recursive: true });
  fs.rmSync(path.join(projectDir, `${sessionId}.jsonl`), { force: true });
  fs.symlinkSync(file, path.join(projectDir, `${sessionId}.jsonl`));
  return { dir, sessionId, cwd };
})() : null;
function sessionTranscript(a, url) {
  const repo = path.join(mockSession.dir, "repo");
  fs.mkdirSync(path.join(repo, ".rudder", "runs", a.id), { recursive: true });
  fs.writeFileSync(path.join(repo, ".rudder", "runs", a.id, "run.json"), JSON.stringify({
    id: a.id, backend: "claude", task: a.task, taskSummary: a.title, status: a.status,
    session: { nativeSessionId: mockSession.sessionId }, workspace: { path: mockSession.cwd },
  }));
  return readTranscript(repo, a.id, {
    limit: Number(url.searchParams.get("limit") || 300),
    home: path.join(mockSession.dir, "home"),
    before: url.searchParams.get("before"),
    after: url.searchParams.get("after"),
  });
}
// MOCK_SLASH_CLAUDE=<file>, MOCK_SLASH_CODEX=<file>: real /api/app/slash-commands
// responses (worker/slash-commands.mjs forRun) for the chat's "/" menu.
// MOCK_SLASH_DELAY_MS: how long each answer takes, as a home building the list would.
const slashLists = Object.fromEntries(["claude", "codex"].filter((b) => process.env[`MOCK_SLASH_${b.toUpperCase()}`])
  .map((b) => [b, JSON.parse(fs.readFileSync(process.env[`MOCK_SLASH_${b.toUpperCase()}`], "utf8"))]));
const slashDelayMs = Number(process.env.MOCK_SLASH_DELAY_MS || 0);
const transcripts = new Map();
// The agent page's speed and context line (worker/transcripts.mjs `stats`).
function mockStats(a) {
  if (a.fresh || !["claude", "codex"].includes(a.backend)) return null;
  const end = toMsMock(a.updatedAt);
  const rows = a.backend === "codex" ? [[390, 5200], [1210, 16400], [156, 4100], [702, 9800], [1880, 22600]] : [[412, 5100], [1310, 14200], [236, 3900], [884, 9600], [1502, 16800]];
  const responses = rows.map(([tokens, ms], i) => ({ tokens, ms, at: end - (rows.length - 1 - i) * 40_000 }));
  const tokens = responses.reduce((n, r) => n + r.tokens, 0);
  const ms = responses.reduce((n, r) => n + r.ms, 0);
  const speed = { tokensPerSecond: Math.round((tokens / ms) * 10_000) / 10, responses };
  return a.backend === "codex"
    ? { model: a.model, speed, context: { used: 391_400, window: 475_000, modelWindow: 475_000, autoCompactWindow: null, at: end, compactedAt: end - 35 * MIN } }
    : { model: "claude-opus-5-5", speed, context: { used: 153_502, window: 400_000, modelWindow: 1_000_000, autoCompactWindow: 400_000, at: end, compactedAt: null } };
}
const toMsMock = (t) => (typeof t === "number" ? t : Date.parse(t));
// The composer's context meter (worker/context-usage.mjs). run_a1 nears its
// threshold while it compacts and drops after (/app/__mock/compact).
function mockContext(a) {
  if (a.fresh || a.backend !== "claude") return null;
  const used = a.id !== "run_a1" ? 153_502 : compactMode === "live" ? 366_812 : compactMode === "done" ? 26_140 : 241_930;
  return { used, window: 400_000, autoCompactAt: 367_000, source: "autoCompactWindow 400000 in ~/.claude/settings.json", at: toMsMock(a.updatedAt) };
}
// Claude Code's suggested next prompt (worker/suggestion.mjs) for the idle
// Claude agent; its plugin clears it when the next turn starts.
const suggestions = new Map([["run_a3", { text: "Merge it, then add the preview links to the README", at: T0 - 17 * MIN }]]);
// The standing goal `/goal` set, while the agent runs (worker/chat-goal.mjs).
const goals = new Map([["run_a1", { text: "Ship the phone app: transcripts render markdown and code, scroll stays pinned at the bottom, and tool calls collapse" }]]);
// The chats runningRows has been added to.
const runningAdded = new Set();
function transcriptFor(ws, run) {
  const p = projects.find((x) => x.workspaceId === ws) || (coordinator?.workspaceId === ws ? coordinator : null);
  const a = p?.agents.find((x) => x.id === run);
  if (!a) return null;
  if (!transcripts.has(run) && realTranscript && a === projects[0].agents[0]) transcripts.set(run, realTranscript.messages);
  if (!transcripts.has(run) && SUBAGENT_DEMO.has(run)) transcripts.set(run, subagentTranscript(a));
  if (!transcripts.has(run) && run === "run_a4") transcripts.set(run, longTranscript(a));
  if (!transcripts.has(run)) transcripts.set(run, a.fresh ? [{ id: "m1", role: "user", ts: a.createdAt, text: a.task }] : baseTranscript(a));
  if (!runningAdded.has(run)) {
    runningAdded.add(run);
    transcripts.get(run).push(...runningRows(a));
  }
  return { a, msgs: transcripts.get(run) };
}

function json(res, status, body) {
  res.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
  res.end(JSON.stringify(body));
}
function readBody(req) {
  return new Promise((resolve) => {
    let d = "";
    req.on("data", (c) => { d += c; });
    req.on("end", () => { try { resolve(JSON.parse(d || "{}")); } catch { resolve({}); } });
  });
}
const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".map": "application/json; charset=utf-8", ".svg": "image/svg+xml", ".png": "image/png", ".woff2": "font/woff2", ".webmanifest": "application/manifest+json" };

// The app as Cloud serves it (scripts/build-app.mjs), built in memory. The
// shell is built again on every page load, so an edit under cloud/app shows
// on reload; CodeMirror and xterm.js are built once, when first asked for.
let shell = null; // Promise of published path -> bytes
let vendor = null;
async function serveStatic(req, res, url) {
  const rel = url.pathname.replace(/^\/app\/?/, "") || "index.html";
  let files;
  try {
    if (rel.startsWith("vendor/")) {
      files = await (vendor ??= buildVendor());
    } else {
      if (!shell || req.headers["sec-fetch-mode"] === "navigate") shell = buildShell();
      files = await shell;
    }
  } catch (error) {
    if (rel.startsWith("vendor/")) vendor = null; else shell = null;
    res.writeHead(500, { "Content-Type": "text/plain; charset=utf-8" });
    return res.end(`The app did not build: ${error.message}`);
  }
  const asset = rel.startsWith("assets/");
  if (!files.has(rel) && (asset || rel.startsWith("vendor/"))) {
    res.writeHead(404, { "Content-Type": "text/plain; charset=utf-8" });
    return res.end(`${rel} is not in this build`);
  }
  const name = files.has(rel) ? rel : "index.html";
  const headers = { "Content-Type": TYPES[path.extname(name)] || "application/octet-stream", "Cache-Control": asset ? "public, max-age=31536000, immutable" : "no-cache" };
  if (name === "sw.js") headers["Service-Worker-Allowed"] = "/app";
  res.writeHead(200, headers);
  res.end(files.get(name));
}

// ---------- Team view (cloud/src/team.ts, through its built module) ----------
const teamLib = await import("../dist/team.js").catch(() => null);
const HR = 60 * MIN;
const teammates = {
  "better-auth:sam": { machine: "awake", projects: [
    { workspaceId: "wsp_sam_billing", name: "billing-api", repo: "exla-ai/billing-api", agents: [
      agent({ id: "run_s1", title: "Retry failed invoices with backoff", task: "Retry failed invoices", status: "working", backend: "claude", model: "opus", updatedAt: T0 - 1 * MIN }),
      agent({ id: "run_s2", title: "Salary bands import", task: "secret", status: "waiting", backend: "claude", model: "opus", asks: [{ id: "1-aaaaaaaa", kind: "question" }], visibility: "private", updatedAt: T0 - 6 * MIN }),
      agent({ id: "run_s3", title: "Stripe tax IDs on invoices", task: "Tax IDs", status: "review", backend: "codex", model: "gpt-6.1-sol", updatedAt: T0 - 40 * MIN }),
    ] },
    { workspaceId: "wsp_sam_rudder", name: "rudder", repo: "exla-ai/rudder", agents: [
      agent({ id: "run_s4", title: "Team view in the web app", task: "Show teammates' agents", status: "working", backend: "claude", model: "sonnet", phase: "testing", updatedAt: T0 - 2 * MIN }),
    ] },
  ] },
  "github:991": { machine: "asleep", projects: [
    { workspaceId: "wsp_ava_docs", name: "docs", repo: "exla-ai/docs", agents: [
      agent({ id: "run_v1", title: "Rewrite the quickstart for the Mac app", task: "Quickstart for Mac", status: "done", backend: "claude", model: "opus", updatedAt: T0 - 3 * HR }),
      agent({ id: "run_v2", title: "secret", task: "secret", status: "failed", visibility: "private", updatedAt: T0 - 5 * HR }),
    ] },
  ] },
};
const teamDeps = () => ({
  orgsFor: () => orgList().map((o) => ({ id: o.id, name: o.name })),
  membersOf: (_viewer, orgId) => {
    const org = orgs.get(orgId);
    if (!org || !mine(org)) throw Object.assign(new Error("org not found"), { status: 404 });
    return org.members.map((m) => ({ ...m, you: m.accountId === ME }));
  },
  projectsOf: (id) => teammates[id]?.projects ?? [],
  machineOf: (id) => teammates[id]?.machine ?? "none",
});
function teamRoutes(req, res, p, url, body) {
  if (p === "/api/app/team" && req.method === "GET") {
    if (!teamLib) return json(res, 404, { error: "build cloud first (npm --prefix cloud run build)" }), true;
    try { return json(res, 200, teamLib.buildTeamView({ accountId: ME }, url.searchParams.get("org") || null, teamDeps())), true; }
    catch (err) { return json(res, err.status || 500, { error: err.message }), true; }
  }
  if (p === "/api/app/team/transcript" && req.method === "GET") {
    const mate = teammates[url.searchParams.get("member")];
    const proj = mate?.projects.find((x) => x.workspaceId === url.searchParams.get("workspace"));
    const a = proj?.agents.find((x) => x.id === url.searchParams.get("run"));
    if (!a || a.visibility === "private") return json(res, 404, { error: "not found" }), true;
    if (mate.machine !== "awake") return json(res, 503, { error: "Ava's cloud machine is asleep. Their agents show here again when it wakes.", asleep: true }), true;
    if (!transcripts.has(a.id)) transcripts.set(a.id, baseTranscript(a));
    return json(res, 200, { runId: a.id, title: a.title, backend: a.backend, status: a.status, messages: transcripts.get(a.id), readOnly: true }), true;
  }
  if (p === "/api/app/visibility" && req.method === "POST") {
    const proj = projects.find((x) => x.workspaceId === body.workspace);
    if (!proj) return json(res, 404, { error: "project not found" }), true;
    if (typeof body.newAgentsPrivate === "boolean") { proj.newAgentsPrivate = body.newAgentsPrivate; return json(res, 200, { workspaceId: proj.workspaceId, newAgentsPrivate: proj.newAgentsPrivate }), true; }
    const a = proj.agents.find((x) => x.id === body.run);
    if (!a) return json(res, 404, { error: "agent not found" }), true;
    if (body.visibility !== "team" && body.visibility !== "private") return json(res, 400, { error: "visibility must be team or private" }), true;
    a.visibility = body.visibility;
    return json(res, 200, { workspaceId: proj.workspaceId, run: a.id, visibility: a.visibility }), true;
  }
  return undefined;
}

// ---------- Orgs, invites, integrations (CONTRACT.md) ----------
const ME = "better-auth:me";
const orgs = new Map([["org_exla", {
  id: "org_exla", name: "Exla", createdAt: iso(now() - 40 * 24 * 60 * MIN),
  members: [
    { accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: "admin", joinedAt: iso(now() - 40 * 24 * 60 * MIN) },
    { accountId: "better-auth:sam", email: "sam@exla.ai", name: "Sam Lee", role: "admin", joinedAt: iso(now() - 30 * 24 * 60 * MIN) },
    { accountId: "github:991", email: "ava@exla.ai", name: null, role: "member", joinedAt: iso(now() - 3 * 24 * 60 * MIN) },
  ],
  invites: [{ id: "inv_1", email: "jo@exla.ai", role: "member", createdAt: iso(now() - 2 * 60 * MIN), expiresAt: iso(now() + 6 * 24 * 60 * MIN) }],
}]]);
const inviteTokens = new Map([["tok_demo", { orgId: "org_exla", role: "member", email: null, used: false }]]);
let integMode = "on"; // on: configured | off: configured, nothing connected | unset: configured:false
const integStore = new Map();
function integ(scope) {
  if (!integStore.has(scope)) integStore.set(scope, scope === "org_exla"
    ? { slack: { connected: true, team: { id: "T1", name: "Exla" }, channel: { id: "C2", name: "agents" } } }
    : { slack: { connected: false, team: null, channel: null } });
  return integStore.get(scope);
}
const mine = (o) => o.members.find((m) => m.accountId === ME);
const orgList = () => [...orgs.values()].filter(mine).map((o) => ({ id: o.id, name: o.name, role: mine(o).role, memberCount: o.members.length, createdAt: o.createdAt }));

/** Returns true when it answered the request, undefined otherwise. */
function orgRoutes(req, res, p, url, body) {
  const m = req.method;
  const send = (status, obj) => { if (status === 204) { res.writeHead(204); res.end(); } else json(res, status, obj); return true; };
  if (p === "/api/app/event" && m === "POST") return send(204);
  if (p === "/api/app/orgs") {
    if (m === "POST") {
      const name = String(body.name || "").trim();
      if (!name) return send(400, { error: "Give the organization a name." });
      const id = `org_${Math.random().toString(36).slice(2, 8)}`;
      orgs.set(id, { id, name, createdAt: iso(now()), members: [{ accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: "admin", joinedAt: iso(now()) }], invites: [] });
      return send(200, { org: { id, name, role: "admin", memberCount: 1, createdAt: iso(now()) } });
    }
    return send(200, { orgs: orgList() });
  }
  let r;
  if ((r = p.match(/^\/api\/app\/orgs\/([^/]+)(?:\/(invites|members)(?:\/([^/]+))?)?$/))) {
    const org = orgs.get(decodeURIComponent(r[1]));
    if (!org || !mine(org)) return send(404, { error: "Organization not found" });
    const admin = mine(org).role === "admin";
    const admins = () => org.members.filter((x) => x.role === "admin").length;
    const [, , sub, rawId] = r;
    const id = rawId && decodeURIComponent(rawId);
    if (!sub) {
      if (m === "GET") return send(200, { org: { id: org.id, name: org.name, createdAt: org.createdAt }, role: mine(org).role, members: org.members.map((x) => ({ ...x, you: x.accountId === ME })), invites: admin ? org.invites : [] });
      if (!admin) return send(403, { error: "Only admins can do that." });
      if (m === "PATCH") { org.name = String(body.name || org.name).trim(); return send(200, { org: { id: org.id, name: org.name } }); }
      if (m === "DELETE") { orgs.delete(org.id); return send(200, { ok: true }); }
    }
    if (sub === "invites") {
      if (!admin) return send(403, { error: "Only admins can do that." });
      if (m === "POST") {
        const token = `tok_${Math.random().toString(36).slice(2, 12)}`;
        const inv = { id: `inv_${Math.random().toString(36).slice(2, 8)}`, email: body.email || null, role: body.role === "admin" ? "admin" : "member", createdAt: iso(now()), expiresAt: iso(now() + 7 * 24 * 60 * MIN) };
        org.invites.push(inv);
        inviteTokens.set(token, { orgId: org.id, role: inv.role, email: inv.email, used: false, inviteId: inv.id });
        return send(200, { invite: { ...inv, url: `http://${req.headers.host}/app/join/${token}` } });
      }
      if (m === "DELETE" && id) { org.invites = org.invites.filter((i) => i.id !== id); return send(200, { ok: true }); }
    }
    if (sub === "members" && id) {
      const target = org.members.find((x) => x.accountId === id);
      if (!target) return send(404, { error: "Not a member" });
      if (m === "POST") {
        if (!admin) return send(403, { error: "Only admins can do that." });
        if (target.role === "admin" && body.role !== "admin" && admins() === 1) return send(409, { error: "an org needs at least one admin" });
        target.role = body.role === "admin" ? "admin" : "member";
        return send(200, { ok: true });
      }
      if (m === "DELETE") {
        if (!admin && id !== ME) return send(403, { error: "Only admins can do that." });
        if (target.role === "admin" && admins() === 1) return send(409, { error: "an org needs at least one admin" });
        org.members = org.members.filter((x) => x !== target);
        return send(200, { ok: true });
      }
    }
    return send(405, { error: "Method not allowed" });
  }
  if ((r = p.match(/^\/api\/app\/invites\/([^/]+)(\/accept)?$/))) {
    const inv = inviteTokens.get(decodeURIComponent(r[1]));
    if (!inv) return send(404, { error: "This invite link is not valid." });
    if (inv.used) return send(410, { error: "This invite was already used or has expired." });
    const org = orgs.get(inv.orgId);
    if (!org) return send(410, { error: "This invite was already used or has expired." });
    const alreadyMember = !!mine(org);
    if (!r[2]) return send(200, { org: { id: org.id, name: org.name }, role: inv.role, email: inv.email, expiresAt: iso(now() + 6 * 24 * 60 * MIN), alreadyMember });
    if (m !== "POST") return send(405, { error: "Method not allowed" });
    if (inv.email && inv.email !== "viraat@example.com") return send(403, { error: `This invite is for ${inv.email}.` });
    if (!alreadyMember) org.members.push({ accountId: ME, email: "viraat@example.com", name: "Viraat Das", role: inv.role, joinedAt: iso(now()) });
    inv.used = true;
    if (inv.inviteId) org.invites = org.invites.filter((i) => i.id !== inv.inviteId);
    return send(200, { org: { id: org.id, name: org.name, role: mine(org).role } });
  }
  const scope = url.searchParams.get("org") || body.org || "personal";
  const orgOk = scope === "personal" || (orgs.get(scope) && mine(orgs.get(scope)));
  const canManage = scope === "personal" || mine(orgs.get(scope) || { members: [] })?.role === "admin";
  if (p.startsWith("/api/app/integrations")) {
    if (!orgOk) return send(404, { error: "Organization not found" });
  }
  const configured = integMode !== "unset";
  if (p === "/api/app/integrations" && m === "GET") {
    const it = integ(scope);
    const on = integMode === "on";
    return send(200, {
      slack: { configured, connected: configured && on && it.slack.connected, team: on ? it.slack.team : null, channel: on ? it.slack.channel : null },
      canManage,
    });
  }
  if ((r = p.match(/^\/api\/app\/integrations\/(slack)\/start$/)) && m === "POST") {
    if (!configured) return send(503, { error: "Slack is not set up on this server yet." });
    if (!canManage) return send(403, { error: "Only admins can do that." });
    integMode = "on";
    return send(200, { url: `/app/__mock/oauth/${r[1]}${scope !== "personal" ? `?org=${encodeURIComponent(scope)}` : ""}` });
  }
  if (p === "/api/app/integrations/slack" && m === "DELETE") {
    if (!canManage) return send(403, { error: "Only admins can do that." });
    integ(scope).slack = { connected: false, team: null, channel: null };
    return send(200, { ok: true });
  }
  const CHANNELS = [{ id: "C1", name: "general" }, { id: "C2", name: "agents" }, { id: "C3", name: "eng-alerts" }];
  if (p === "/api/app/integrations/slack/channels" && m === "GET") return send(200, { channels: CHANNELS });
  if (p === "/api/app/integrations/slack/channel" && m === "POST") {
    const ch = CHANNELS.find((c) => c.id === body.channel);
    if (!ch) return send(400, { error: "Pick a channel." });
    integ(scope).slack.channel = ch;
    return send(200, { channel: ch });
  }
  if ((r = p.match(/^\/api\/app\/link\/slack\/([^/]+)$/)) && m === "POST") {
    if (r[1] === "expired") return send(410, { error: "This link has expired. Run /rudder link in Slack again." });
    return send(200, { ok: true, slack: { team: "Exla", user: "viraat" } });
  }
  return undefined;
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- Project files (files-panel.js) ----------
// Two workspaces of wsp_rudder: the main checkout and run_a1's own fork.
const PNG_DOT = "iVBORw0KGgoAAAANSUhEUgAAAPAAAABaCAIAAAAJsExNAAAB3klEQVR4nO3dsW0DQRRDwevI3bgIZUpcq7twYihW7PgILE0M8ApYfE6+1+v3R5rpOv4CKRjQmgpoTQW0pgJaUwGtqYDWVEBrKqA1FdCaCmhNBbSmAlpTAa2pgNZUQGsqoDUV0JoKaE0FtKYCWlMBramA1lRAayqgNdVd0B+f30sd38MKN1cAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgA6dkqgG1YAOnZKoBtWADp2SqAbVgD6T1/Px3/s+N2yAR3rOE2ggU52nCbQQLec8mDH79azAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bAC0LFTAt2wAtCxUwLdsALQsVMC3bCCfwo1FdCaCmhNBbSmAlpTAa2pgNZUQGsqoDUV0JoKaE0FtKYCWlMBramA1lRAayqgNRXQmgpoTQW0pgJaUwGtqYDWVEBrKqA11RsfFgRxnxmPjgAAAABJRU5ErkJggg==";
const readmeText = (extra = "") => `# rudder\n\nRudder runs Claude Code and Codex agents on your own machines. ${extra}\n\n![Architecture](docs/diagram.png)\n\n## Layout\n\n| Path | What it holds |\n|---|---|\n| \`src/\` | the CLI |\n| \`native/\` | the Rust dashboard |\n| \`cloud/\` | control plane and workers |\n\n## Try it\n\n\`\`\`sh\nnpm install -g @exla/rudder\nrudder "fix the flaky test"\n\`\`\`\n\nSee the [guide](docs/guide.md) for more.\n`;
const mainTs = `import { runManager } from "./run-manager.js";\n\n// Start one agent and print where it runs.\nexport async function main(argv: string[]): Promise<number> {\n  const task = argv.join(" ").trim();\n  if (!task) {\n    console.error("Usage: rudder \\"task\\"");\n    return 2;\n  }\n  const run = await runManager.start({ task, backend: "claude" });\n  console.log(\`Started \${run.id} in \${run.workspace.path}\`);\n  return 0;\n}\n`;
const fileTrees = {
  checkout: {
    "README.md": readmeText(),
    "AGENTS.md": "# Rudder engineering rules\n\nRudder runs Claude Code and Codex agents locally and on Fly.io Linux.\n\n## Verification\n\n- Run one targeted check for what you changed.\n- No full suites per change.\n",
    "docs/guide.md": "# Guide\n\nStart with the [README](../README.md).\n\n- Agents work in their own workspace.\n- Finished work merges into main.\n",
    "docs/diagram.png": Buffer.from(PNG_DOT, "base64"),
    "src/main.ts": mainTs,
    "src/run-manager.ts": "export const runManager = {\n  async start(options: { task: string; backend: string }) {\n    return { id: \"run-1\", workspace: { path: \"/tmp\" }, ...options };\n  },\n};\n",
    "Cargo.toml": "[workspace]\nmembers = [\"native\", \"engine\"]\nresolver = \"2\"\n",
    "package.json": "{\n  \"name\": \"@exla/rudder\",\n  \"version\": \"4.4.2\",\n  \"type\": \"module\"\n}\n",
  },
};
fileTrees.run_a1 = { ...fileTrees.checkout, "README.md": readmeText("This line is the agent's own edit."), "src/files-panel.ts": "// Added by the agent in its own workspace.\nexport const panel = true;\n" };
const fileScopes = (ws) => ws !== "wsp_rudder" ? [{ projectId: "prj_mock", workspaceId: "checkout", attemptId: "checkout", ownerEpoch: 3 }]
  : ["checkout", "run_a1"].map((id) => ({ projectId: "prj_rudder", workspaceId: id, attemptId: id === "checkout" ? id : `${id}-attempt-1`, ownerEpoch: 3 }));
const revisionOf = (value) => createHash("sha256").update(value).digest("hex");
function fileDoc(rel, value, known) {
  const ext = path.extname(rel).toLowerCase();
  const revision = revisionOf(value);
  const base = { path: rel, revision };
  if (ext === ".png") return { ...base, kind: "image", mime: "image/png", ...(known === revision ? { unchanged: true } : { base64: value.toString("base64") }) };
  const kind = ext === ".md" ? "markdown" : ext === ".html" ? "html" : "text";
  const mime = kind === "markdown" ? "text/markdown" : kind === "html" ? "text/html" : ext === ".css" ? "text/css" : "text/plain";
  return { ...base, kind, mime, ...(known === revision ? { unchanged: true } : { text: value }) };
}
function projectFiles(req, res, url, body) {
  const q = body ?? Object.fromEntries(url.searchParams);
  const scope = body ? body.scope : { workspaceId: q.workspaceId };
  const tree = fileTrees[scope?.workspaceId] ?? (scope?.workspaceId === "checkout" ? fileTrees.checkout : null);
  if (!tree) return json(res, 403, { error: "Refresh this project's file workspaces" });
  return treeFiles(res, tree, q, body);
}
function treeFiles(res, tree, q, body) {
  const rel = q.path ?? "";
  if (body) {
    const current = tree[rel];
    if (typeof current !== "string") return json(res, 404, { error: "The project file no longer exists" });
    if (revisionOf(current) !== body.baseRevision) return json(res, 200, { path: rel, outcome: "conflict" });
    tree[rel] = body.text;
    return json(res, 200, { path: rel, outcome: "saved", revision: revisionOf(body.text) });
  }
  if (q.operation === "list") {
    const prefix = rel ? `${rel}/` : "";
    const seen = new Map();
    for (const key of Object.keys(tree)) {
      if (!key.startsWith(prefix)) continue;
      const [name, ...rest] = key.slice(prefix.length).split("/");
      seen.set(name, { name, path: prefix + name, kind: rest.length ? "folder" : "file" });
    }
    const entries = [...seen.values()].sort((a, b) => a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "folder" ? -1 : 1);
    return json(res, 200, { path: rel, entries, truncated: false });
  }
  const value = tree[rel];
  if (value === undefined) return json(res, 404, { error: "The project file no longer exists" });
  return json(res, 200, fileDoc(rel, value, q.knownRevision));
}

// ---------- Agent setup (files-hub.js) ----------
// Separate instruction files at first, so the page offers "Use one file for both".
const skillText = (name, description, body) => `---\nname: ${name}\ndescription: ${description}\n---\n\n# ${name}\n\n${body}\n`;
const freshSetup = () => ({
  linked: false,
  roots: {
    "instructions:claude": { "CLAUDE.md": "# Rules for every agent\n\n- No fallbacks unless asked.\n- Links to things running here go over Tailscale.\n- Run one targeted check per change.\n" },
    "instructions:codex": { "AGENTS.md": "# Codex notes\n\n- Prefer small commits on main.\n" },
    "skill:claude:rudder-burst": { "SKILL.md": skillText("rudder-burst", "Run heavy builds on the burst build pool instead of the shared home", "Use `rudder-burst <command>`.") },
    "skill:codex:rudder-burst": { "SKILL.md": skillText("rudder-burst", "Run heavy builds on the burst build pool instead of the shared home", "Use `rudder-burst <command>` (older copy).") },
    "skill:claude:yc": { "SKILL.md": skillText("yc", "Search Bookface and YC knowledge from the terminal", "Run `yc search`."), "reference/commands.md": "# Commands\n" },
    "skill:codex:libra-investigate": { "SKILL.md": skillText("libra-investigate", "Investigate a production alert with Libra's telemetry", "Start from the alert.") },
    "folder:claude-agents": { "reviewer.md": "---\nname: reviewer\ndescription: Reviews a diff for bugs\n---\nReview the diff.\n" },
  },
  shared: [],   // names of skills in ~/.agents/skills
  sharedWith: {}, // name -> true when linked into the other agent
});
let setup = freshSetup();
const setupTree = (id) => (id === "instructions:codex" && setup.linked ? setup.roots["instructions:claude"] : setup.roots[id]);
function setupOverview() {
  const claudeFile = setup.roots["instructions:claude"]["CLAUDE.md"] !== undefined;
  const skills = [];
  const row = (name, agents, root, location, note = "") => {
    const meta = /description: (.*)/.exec(setupTree(root)?.["SKILL.md"] ?? "");
    skills.push({ name, description: meta?.[1] ?? "", agents, root, location, note });
  };
  row("rudder-burst", ["claude"], "skill:claude:rudder-burst", "~/.claude/skills/rudder-burst", "Each agent has its own copy of rudder-burst. Edits to one do not reach the other.");
  row("rudder-burst", ["codex"], "skill:codex:rudder-burst", "~/.codex/skills/rudder-burst", "Each agent has its own copy of rudder-burst. Edits to one do not reach the other.");
  row("yc", setup.sharedWith.yc ? ["claude", "codex"] : ["claude"], "skill:claude:yc", "~/.claude/skills/yc");
  row("libra-investigate", ["codex"], "skill:codex:libra-investigate", "~/workspace/code/libra/.agents/skills/libra-investigate");
  for (const name of setup.shared) row(name, ["claude", "codex"], `skill:shared:${name}`, `~/.agents/skills/${name}`);
  skills.sort((a, b) => a.name.localeCompare(b.name));
  return {
    agents: { claude: { dir: "~/.claude", present: true }, codex: { dir: "~/.codex", present: true } },
    instructions: {
      claude: { agent: "claude", path: "~/.claude/CLAUDE.md", state: claudeFile ? "file" : "missing", linksTo: null, root: "instructions:claude" },
      codex: { agent: "codex", path: "~/.codex/AGENTS.md", state: setup.linked ? "link" : "file", linksTo: setup.linked ? "~/.claude/CLAUDE.md" : null, root: "instructions:codex" },
      shared: setup.linked,
    },
    skills,
    folders: [{ id: "folder:claude-agents", label: "Claude subagents", root: "folder:claude-agents" }],
  };
}
function setupAction(res, body) {
  if (body.operation === "link-instructions") {
    const replaced = setup.roots["instructions:codex"]["AGENTS.md"];
    if (!setup.linked && body.keep === "claude" && revisionOf(replaced) !== body.replacedRevision) return json(res, 409, { error: "~/.codex/AGENTS.md changed since you looked at it. Nothing was linked." });
    if (body.keep === "codex") setup.roots["instructions:claude"] = { "CLAUDE.md": replaced };
    setup.linked = true;
  } else if (body.operation === "share-skill") {
    setup.sharedWith[body.name] = true;
  } else if (body.operation === "new-skill") {
    setup.shared.push(body.name);
    setup.roots[`skill:shared:${body.name}`] = { "SKILL.md": skillText(body.name, body.description, "What the agent should do, step by step.") };
  }
  return json(res, 200, setupOverview());
}
const projectGuides = {
  wsp_rudder: { files: [
    { path: "AGENTS.md", kind: "file", linksTo: null, imports: [], readBy: ["claude", "codex"] },
    { path: "CLAUDE.md", kind: "link", linksTo: "AGENTS.md", imports: [], readBy: ["claude", "codex"] },
    { path: "README.md", kind: "file", linksTo: null, imports: [], readBy: [] },
  ], shared: true },
};

// ---------- Agent notes (notes.js) ----------
// notes.md in each agent's run folder, answered as the home's notes service
// does: no file until the first save. /app/__mock/notes/other-device edits
// every agent's notes as a save from another device would.
const agentNotes = new Map(); // `${ws}|${run}` -> { text, revision }
// A file a chat message names (file-link.js; the home side is
// src/project-files/file-refs.ts): the agent's workspace, then the checkout
// (both at /workspace/code/rudder, files under cloud/ from this repository),
// then a few files in the home user's folders.
const MOCK_CHECKOUT = "/workspace/code/rudder";
const hostFiles = {
  "/home/rudder/.claude/plans/dreamy-launching-firefly.md": "# Offstage snapshot plan\n\nSnapshot the agent's Mac session before each run, so a failed run restores in seconds. Background is in [the research notes](dreamy-launching-firefly-notes.md).\n\n## Steps\n\n- [x] Take an APFS snapshot when the run starts\n- [x] Keep the last three per agent\n- [ ] Restore on failure, then retry once\n- [ ] Show the restore in the agent's chat\n\n## Full Opus/Fable design\n\n| Piece | Owner | Notes |\n|---|---|---|\n| Snapshot daemon | `offstage/snapd.ts` | runs as root |\n| Restore | `offstage/restore.ts` | needs a reboot on Intel |\n| Retention | `offstage/prune.ts` | three per agent |\n\n### Snapshot\n\n```ts\nexport async function snapshot(run: string): Promise<string> {\n  const name = `rudder-${run}`;\n  await apfs.create(name);\n  return name;\n}\n```\n\n> Restores never touch `/Users/*/Library/Keychains`.\n\n## Open questions\n\n1. Does Fable need its own volume?\n2. How long does a restore take on an M1 Air?\n",
  "/home/rudder/.claude/plans/dreamy-launching-firefly-notes.md": "# Research notes\n\nAPFS snapshots are copy on write, so taking one costs about 40 ms.\n\nBack to [the plan](dreamy-launching-firefly.md).\n",
};
function fileRefMock(res, url) {
  if (homeMode !== "running") return json(res, 503, { error: "the cloud machine is not connected yet; it may still be starting" });
  if (fileRefMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
  const asked = url.searchParams.get("path") || "";
  const run = url.searchParams.get("runId") || "";
  const known = url.searchParams.get("knownRevision") || undefined;
  const absolute = asked.startsWith("~/") ? `/home/rudder/${asked.slice(2)}` : asked.startsWith("/") ? path.posix.normalize(asked) : null;
  const candidates = absolute ? [absolute] : [path.posix.join(MOCK_CHECKOUT, asked)];
  for (const file of candidates) {
    if (/\/\.(ssh|aws|config)\/|\/\.env/.test(file)) return json(res, 400, { error: "This file can hold keys or tokens, so the viewer does not open it" });
    let value = hostFiles[file];
    const rel = file.startsWith(`${MOCK_CHECKOUT}/`) ? file.slice(MOCK_CHECKOUT.length + 1) : null;
    if (value === undefined && rel !== null) {
      value = (fileTrees[run] ?? fileTrees.checkout)[rel] ?? fileTrees.checkout[rel];
      if (value === undefined && rel.startsWith("cloud/") && !rel.includes("..")) {
        try { value = fs.readFileSync(path.join(DIR, "..", "..", rel), "utf8"); } catch { value = undefined; }
      }
    }
    if (value === undefined || Buffer.isBuffer(value)) continue;
    const doc = fileDoc(file, value, known);
    return json(res, 200, { ...doc, kind: doc.kind === "markdown" ? "markdown" : "text" });
  }
  return json(res, 400, { error: `File not found: ${asked}` });
}

function agentNotesMock(res, url, body) {
  if (homeMode !== "running") return json(res, 503, { error: "the cloud machine is not connected yet; it may still be starting" });
  if (notesMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
  const ws = body ? body.workspaceId : url.searchParams.get("workspaceId");
  const run = body ? body.runId : url.searchParams.get("runId");
  const project = projects.find((p) => p.workspaceId === ws);
  if (!project?.agents?.some((a) => a.id === run)) return json(res, 400, { error: "That agent is not on your cloud machine" });
  const current = agentNotes.get(`${ws}|${run}`) ?? { text: "", revision: null };
  if (!body) return json(res, 200, current);
  if (body.baseRevision !== current.revision) return json(res, 200, { outcome: "conflict", ...current });
  const next = { text: body.text, revision: revisionOf(body.text) };
  agentNotes.set(`${ws}|${run}`, next);
  return json(res, 200, { outcome: "saved", revision: next.revision });
}

// ---------- Vibeprint canvas (canvas-panel.js) ----------
// A real `vibeprint serve` stands in for the home's: MOCK_VIBEPRINT_URL is its
// page (http://127.0.0.1:PORT/) and MOCK_VIBEPRINT_TOKEN its access token. The
// eye's check sends it a made-up diff for the agent, as the home sends the real one.
const vibeprintOn = new Map(); // ws -> start time
const vibeprintTracked = new Map(); // `${ws}/${run}` -> { lastAt }
async function vibeprintMock(req, res, url) {
  const body = req.method === "POST" ? await readBody(req) : null;
  const ws = body ? body.workspace : url.searchParams.get("workspace");
  const run = body ? body.run : url.searchParams.get("run");
  const base = process.env.MOCK_VIBEPRINT_URL, token = process.env.MOCK_VIBEPRINT_TOKEN;
  const tracking = () => {
    const t = vibeprintTracked.get(`${ws}/${run}`);
    return t ? { on: true, ...(t.lastAt ? { lastAt: new Date(t.lastAt).toISOString() } : {}) } : { on: false };
  };
  const view = () => {
    const since = vibeprintOn.get(ws);
    const t = run ? { tracking: tracking() } : {};
    if (since === undefined) return { state: "off", ...t };
    if (!base || !token) return { state: "failed", error: "Set MOCK_VIBEPRINT_URL and MOCK_VIBEPRINT_TOKEN to a running vibeprint serve.", ...t };
    return now() - since < 1500 ? { state: "starting", ...t } : { state: "running", url: base, token, ...t };
  };
  const vp = (route, payload) => fetch(`${base}${route}`, { method: payload ? "POST" : "GET", headers: { "x-vibeprint-token": token, "content-type": "application/json" }, ...(payload ? { body: JSON.stringify(payload) } : {}) });
  if (!body) return json(res, 200, view());
  if (body.op === "start") { if (!vibeprintOn.has(ws)) vibeprintOn.set(ws, now()); return json(res, 200, view()); }
  if (body.op === "stop") { vibeprintOn.delete(ws); return json(res, 200, { ...view(), state: "off" }); }
  if (body.op === "tracking") {
    if (!body.on) { vibeprintTracked.delete(`${ws}/${run}`); return json(res, 200, view()); }
    const entry = vibeprintTracked.get(`${ws}/${run}`) ?? {};
    vibeprintTracked.set(`${ws}/${run}`, entry);
    if (body.check) {
      const agent = projects.find((p) => p.workspaceId === ws)?.agents.find((a) => a.id === run);
      const r = await vp("api/track", { author: agent?.title ?? run, task: agent?.task ?? "", diff: "diff --git a/api/items.js b/api/items.js\n+export const items = (db) => db.query('select * from items')\n" });
      if (r.ok) entry.lastAt = now();
    }
    return json(res, 200, view());
  }
  if (body.op === "brief") {
    if (view().state !== "running") return json(res, 409, { error: "The canvas is not running" });
    const r = await vp(`api/brief${body.since ? `?since=${encodeURIComponent(body.since)}` : ""}`);
    return json(res, r.status, await r.json());
  }
  return json(res, 400, { error: "Unknown canvas request" });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const p = url.pathname;

  if (p === "/") { res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/login")) {
    signedOut = false;
    const next = url.searchParams.get("next") || "";
    res.writeHead(302, { Location: /^\/app\/[\w\-/%.]*$/.test(next) ? next : "/app" });
    return res.end();
  }
  if (p === "/app/__mock/signout") { signedOut = true; res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/home/")) { homeMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/usage/")) {
    usageMode = p.split("/").pop();
    if (usageMode === "old") homeVersion = "4.4.2";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/subagents/")) {
    trayMode = p.split("/").pop();
    if (trayMode === "old") homeVersion = "5.0.9";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/compact/")) {
    compactMode = p.split("/").pop();
    const t = transcriptFor("wsp_rudder", "run_a1");
    const msgs = t.msgs.filter((m) => m.id !== "compact-live");
    if (compactMode === "live") compactSince = now() - 41_000;
    if (compactMode === "done") msgs.push(compactMarker("compact-live", now(), { pre: 367_412, post: 26_140, ms: 58_200 }));
    transcripts.set("run_a1", msgs);
    res.writeHead(302, { Location: "/app/p/wsp_rudder/a/run_a1" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/update/")) { updateMode = p.split("/").pop(); res.writeHead(302, { Location: "/app/all/new" }); return res.end(); }
  if (p.startsWith("/app/__mock/send/")) { sendMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p.startsWith("/app/__mock/close/")) { closeMode = p.split("/").pop(); res.writeHead(302, { Location: "/app" }); return res.end(); }
  if (p === "/app/__mock/file-ref/ok" || p === "/app/__mock/file-ref/old") {
    fileRefMode = p.split("/").pop();
    if (fileRefMode === "old") homeVersion = "5.0.9";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/search/")) {
    searchMode = p.split("/").pop();
    if (searchMode === "old") homeVersion = "5.0.9";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p === "/app/__mock/notes/ok" || p === "/app/__mock/notes/old") {
    notesMode = p.split("/").pop();
    if (notesMode === "old") homeVersion = "5.0.9";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/harness/")) {
    harnessMode = p.split("/").pop();
    if (harnessMode === "old") homeVersion = "5.0.9";
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/all/")) {
    allMode = p.split("/").pop();
    if (allMode === "old") homeVersion = "5.0.5";
    if (coordinator) Object.assign(coordinator, coordinatorHealth());
    res.writeHead(302, { Location: "/app/all/new" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/creds/")) {
    creds = (CREDS[p.split("/").pop()] || CREDS.out)();
    for (const key of Object.keys(logins)) delete logins[key];
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/tailscale/")) {
    tailscaleMode = p.split("/").pop();
    tailscaleLogin = tailscaleMode === "login" ? tailscaleSession("waiting")
      : tailscaleMode === "expired" ? tailscaleSession("failed", "The sign-in link was not used within 10 minutes, so it expired. Connect again for a new one.") : null;
    res.writeHead(302, { Location: "/app/settings" });
    return res.end();
  }
  if (p.startsWith("/app/__mock/tailscale-flow/")) {
    tailscaleFlow = p.split("/").pop();
    tailscaleMode = "off";
    tailscaleLogin = null;
    res.writeHead(302, { Location: "/app/settings" });
    return res.end();
  }
  if (p === "/app/__mock/fresh") {
    creds = CREDS.out();
    githubConnected = false;
    freshProjects = [];
    res.writeHead(302, { Location: "/app" });
    return res.end();
  }
  if (p === "/app/__mock/computer") {
    const code = `K${Math.floor(Math.random() * 8) + 2}P-${Math.random().toString(36).slice(2, 5).toUpperCase().replace(/[01OIL]/g, "X")}`;
    computers.set(code, { deviceName: "Viraats-MacBook-Pro.local", expiresAt: now() + 5 * MIN });
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end(`code: ${code}\napprove: /app/approve?code=${code}\n`);
  }
  if (p === "/app/__mock/files/reset-setup") {
    setup = freshSetup();
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end("agent setup reset\n");
  }
  if (p === "/app/__mock/files/agent-edit") {
    fileTrees.run_a1["README.md"] = readmeText(`The agent rewrote this at ${new Date().toLocaleTimeString()}.`);
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end("run_a1 changed README.md\n");
  }
  if (p === "/app/__mock/notes/other-device") {
    for (const [key, n] of agentNotes) {
      const text = `${n.text}\nAdded on another device at ${new Date().toLocaleTimeString()}.\n`;
      if (n.revision) agentNotes.set(key, { text, revision: revisionOf(text) });
    }
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end("notes changed on another device\n");
  }
  if (p.startsWith("/app/__mock/integrations/")) { integMode = p.split("/").pop(); res.writeHead(302, { Location: "/app/settings" }); return res.end(); }
  if (p === "/app/__mock/invite") {
    const token = `tok_${Math.random().toString(36).slice(2, 10)}`;
    inviteTokens.set(token, { orgId: "org_exla", role: "member", email: null, used: false });
    res.writeHead(200, { "Content-Type": "text/plain" });
    return res.end(`join: /app/join/${token}\n`);
  }
  if (p.startsWith("/app/__mock/oauth/")) {
    const provider = p.split("/").pop();
    const scope = url.searchParams.get("org") || "personal";
    const it = integ(scope);
    if (provider === "slack") it.slack = { ...it.slack, connected: true, team: { id: "T1", name: "Exla" }, channel: null };
    res.writeHead(302, { Location: `/app/settings?connected=${provider}` });
    return res.end();
  }
  if (p === "/api/auth/sign-out" && req.method === "POST") { signedOut = true; return json(res, 200, { success: true }); }

  if (p.startsWith("/api/app/")) {
    await delay(120 + Math.random() * 180);
    if (signedOut) return json(res, 401, { error: "Not signed in" });

    if (p === "/api/app/state" && req.method === "GET") {
      // Living data: the working agent's message ticks forward.
      const a1 = projects[0].agents[0];
      a1.updatedAt = now() - 20_000;
      return json(res, 200, {
        email: "viraat@example.com", home: home(),
        projects: homeMode === "none" ? [] : freshProjects ?? projects,
        coordinator: allMode === "lost" ? null : coordinator,
        github: { connected: githubConnected, source: githubConnected ? "sync" : null, updatedAt: null },
        digest: digestPreviewOf(digestStore[0]) ?? null,
        orgs: orgList(),
      });
    }
    if (p === "/api/app/project-file-scopes" && req.method === "GET") {
      return json(res, 200, { scopes: fileScopes(url.searchParams.get("workspaceId")) });
    }
    if (p === "/api/app/project-files") {
      return projectFiles(req, res, url, req.method === "POST" ? await readBody(req) : null);
    }
    if (p === "/api/app/vibeprint") return vibeprintMock(req, res, url);
    if (p === "/api/app/agent-setup" && req.method === "GET") return json(res, 200, setupOverview());
    if (p === "/api/app/agent-setup/actions" && req.method === "POST") return setupAction(res, await readBody(req));
    if (p === "/api/app/agent-setup/files") {
      const body = req.method === "POST" ? await readBody(req) : null;
      const tree = setupTree(body ? body.scope?.workspaceId : url.searchParams.get("workspaceId"));
      if (!tree) return json(res, 403, { error: "That agent setup file is not available" });
      return treeFiles(res, tree, body ?? Object.fromEntries(url.searchParams), body);
    }
    if (p === "/api/app/project-guide" && req.method === "GET") {
      return json(res, 200, projectGuides[url.searchParams.get("workspaceId")] ?? { files: [{ path: "README.md", kind: "file", linksTo: null, imports: [], readBy: [] }], shared: false });
    }
    if (p === "/api/app/file-ref" && req.method === "GET") return fileRefMock(res, url);
    if (p === "/api/app/agent-notes") return agentNotesMock(res, url, req.method === "POST" ? await readBody(req) : null);
    if (p === "/api/app/digests" && req.method === "GET") {
      return json(res, 200, { digests: digestStore.map(digestPreviewOf), timeZone: "America/Los_Angeles" });
    }
    if (p === "/api/app/digest" && req.method === "GET") {
      if (url.searchParams.get("live")) {
        const live = mockDigest(digestDay(now()), now(), true);
        return live ? json(res, 200, { id: null, live: true, ...live, since: morning }) : json(res, 404, { error: "build cloud first" });
      }
      const d = digestStore.find((x) => x.id === url.searchParams.get("id")) ?? (url.searchParams.get("id") ? null : digestStore[0]);
      return d ? json(res, 200, { id: d.id, readAt: d.readAt, createdAt: d.createdAt, ...d.body }) : json(res, 404, { error: "no digest yet" });
    }
    if (p === "/api/app/digest/read" && req.method === "POST") {
      const body = await readBody(req);
      const d = digestStore.find((x) => x.id === body.id);
      if (d && !d.readAt) d.readAt = iso(now());
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/slash-commands" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      const list = slashLists[t.a.backend];
      if (!list) return json(res, 500, { error: `no "/" list for ${t.a.backend} agents in the mock (MOCK_SLASH_${t.a.backend.toUpperCase()})` });
      if (homeMode !== "running") return json(res, 503, { error: "the cloud machine is not connected yet; it may still be starting" });
      if (slashDelayMs) await new Promise((resolve) => setTimeout(resolve, slashDelayMs));
      return json(res, 200, list);
    }
    if (p === "/api/app/subagents" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      // server.ts homeNeedsUpdate: a home without the "subagents" method.
      if (trayMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
      const at = (s) => (s.status === "running" ? s.startedAt : s.endedAt ?? s.startedAt) ?? 0;
      const all = (SUBAGENT_DEMO.has(t.a.id) ? mockTray(t.a, t.msgs) : [])
        .sort((x, y) => (y.status === "running") - (x.status === "running") || at(y) - at(x));
      const open = all.find((s) => s.id === url.searchParams.get("open")) ?? null;
      return json(res, 200, {
        runId: t.a.id,
        subagents: all.map(trayLine),
        open: open && { ...trayLine(open), prompt: open.prompt, result: open.result, messages: open.messages, earlier: open.earlier },
      });
    }
    if (p === "/api/app/transcript" && req.method === "GET") {
      const t = transcriptFor(url.searchParams.get("workspace"), url.searchParams.get("run"));
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (mockSession && t.a === projects[0].agents[0]) {
        try {
          return json(res, 200, sessionTranscript(t.a, url));
        } catch (error) {
          return json(res, 500, { error: error.message });
        }
      }
      // Page cursors as the worker's (transcripts.mjs readTranscript), by message index here.
      const limit = Number(url.searchParams.get("limit") || 300);
      const at = (name) => url.searchParams.has(name) ? Number(url.searchParams.get(name).split(":")[1]) : null;
      const end = at("before") ?? t.msgs.length;
      const start = at("after") ?? Math.max(0, end - limit);
      const page = { runId: t.a.id, conversation: t.a.id, earlier: start > 0 ? `${t.a.id}:${start}:` : null, messages: t.msgs.slice(start, end) };
      if (url.searchParams.has("before")) return json(res, 200, page);
      const compacting = t.a.id === "run_a1" && compactMode === "live" ? { since: compactSince } : null;
      // Receipts of the requests the app asks after (home.mjs transcript), once there is one.
      const receipts = url.searchParams.getAll("pending").filter((id) => steerReceipts.has(id)).map((id) => ({ requestId: id, ...steerReceipts.get(id) }));
      return json(res, 200, { ...page, title: t.a.title, backend: t.a.backend, status: t.a.status, edits: mockEdits(t.msgs), stats: mockStats(t.a), context: mockContext(t.a), compacting, suggestion: suggestions.get(t.a.id) ?? null, goal: goals.get(t.a.id) ?? null, receipts });
    }
    if (p === "/api/app/search" && req.method === "GET") {
      if (homeMode !== "running") return json(res, 503, { error: "Waking the cloud machine…", waking: true });
      if (searchMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
      const q = (url.searchParams.get("q") || "").replace(/\s+/g, " ").trim();
      if (q.length < 2) return json(res, 400, { error: "q needs at least 2 characters" });
      if (searchMode === "slow") await new Promise((r) => setTimeout(r, 1500));
      return json(res, 200, mockSearch(q));
    }
    // Machine, warm snapshots, overlay forks, sccache and remote cache (machine-panel.js).
    if (p === "/api/app/machine" && req.method === "GET") {
      const running = homeMode === "running";
      mockMachineTick();
      return json(res, 200, mockMachinePayload(running));
    }
    if (p === "/api/app/machine/prune" && req.method === "POST") {
      if (homeMode !== "running") return json(res, 503, { error: "The cloud machine is asleep", asleep: true });
      const freed = mockMachinePrune();
      return json(res, 200, freed);
    }
    // Plan usage for the sidebar meter (src/usage.ts QuotaReport, from the home).
    if (p === "/api/app/usage" && req.method === "GET") {
      if (homeMode !== "running") return json(res, 503, { error: "The cloud machine is asleep", asleep: true });
      if (usageMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
      const fetchedAt = iso(now() - 2 * MIN);
      const win = (label, percent, inMin) => ({ label, percent, resetsAt: iso(now() + inMin * MIN) });
      const extra = (creds.claude ?? []).filter((a) => a.id !== "main" && a.ok);
      return json(res, 200, { fetchedAt, ttlSeconds: 300, accounts: [
        { provider: "claude", account: "main", default: accountRow("claude", "main")?.default ?? true, email: "viraat@example.com", plan: "Max 20x", blocked: null, error: null, fetchedAt,
          windows: [win("Session (5h)", 42, 134), win("Weekly", 61, 3 * 24 * 60 + 200), win("Weekly · Opus", 74, 3 * 24 * 60 + 200)] },
        ...extra.map((a) => ({ provider: "claude", account: a.id, default: a.default, email: a.account, plan: "Pro", blocked: null, error: null, fetchedAt,
          windows: [win("Session (5h)", 8, 212), win("Weekly", 12, 4 * 24 * 60)] })),
        { provider: "codex", account: "main", default: true, email: "viraat@example.com", plan: "Pro", blocked: null, error: null, fetchedAt,
          windows: [win("Session (5h)", 18, 251), win("Weekly", 33, 5 * 24 * 60 + 30)] },
        // agy's quota summary: one pool per model group; a full bucket has no reset.
        // Matches real Google AI Pro where Gemini 4 Argon has 0 remaining (100% used)
        // while the primary Gemini pool (Neon/Hydrogen/Flash) still has quota.
        { provider: "antigravity", account: "main", default: true, email: "viraat.personal@example.com", plan: "Google AI Pro", blocked: null, error: null, fetchedAt,
          windows: [win("Gemini · Weekly", 15, 6 * 24 * 60 + 800), win("Gemini · Session (5h)", 21, 263),
            { label: "Claude and GPT · Weekly", percent: 0, resetsAt: null }, { label: "Claude and GPT · Session (5h)", percent: 0, resetsAt: null },
            win("Gemini 4 Argon · Weekly", 100, 6 * 24 * 60 + 800)] },
      ] });
    }
    if (p === "/api/app/upload" && req.method === "POST") {
      let size = 0;
      for await (const chunk of req) size += chunk.length;
      const name = String(url.searchParams.get("name") || "file").replace(/[^A-Za-z0-9._-]+/g, "-");
      const proj = projects.find((x) => x.workspaceId === url.searchParams.get("workspace"));
      if (!proj) return json(res, 404, { error: "Project not found" });
      if (/fail/i.test(name)) return json(res, 502, { error: "could not fetch the file (HTTP 500)" });
      await delay(700);
      return json(res, 200, { path: `/workspace/code/${proj.name}/.rudder/uploads/20261017-013000-${name}`, name, size });
    }
    const body = req.method === "POST" || req.method === "PATCH" ? await readBody(req) : {};
    const handled = orgRoutes(req, res, p, url, body) ?? teamRoutes(req, res, p, url, body);
    if (handled !== undefined) return;
    if (p === "/api/app/home/update" && req.method === "POST") {
      if (updating) return json(res, 409, { error: "an update is already on its way" });
      if (homeVersion === CLOUD_VERSION) return json(res, 409, { error: "this machine already runs the newest Rudder" });
      // server.ts homeBusyAgents: mid-turn, or holding a prompt.
      const busy = [...projects, ...(coordinator ? [coordinator] : [])]
        .reduce((n, x) => n + x.agents.filter((a) => a.status === "working" || a.status === "starting" || a.asks?.length).length, 0);
      if (busy && body.interrupt !== true) {
        return json(res, 409, { error: `${busy === 1 ? "1 agent is" : `${busy} agents are`} working or waiting on you, and updating restarts ${busy === 1 ? "it" : "them"}` });
      }
      updating = true;
      updateError = null;
      if (updateMode === "fail") {
        setTimeout(() => { updating = false; updateError = "docker pull was refused by registry.fly.io; log this host in again (sudo docker login registry.fly.io)"; }, 5000);
      } else {
        setTimeout(() => { restarting = true; }, 3000);
        setTimeout(() => {
          restarting = false;
          updating = false;
          homeVersion = CLOUD_VERSION;
          if (allMode === "old") allMode = "ok";
          if (coordinator) Object.assign(coordinator, coordinatorHealth());
        }, 9000);
      }
      return json(res, 200, { home: home() });
    }
    if (p === "/api/app/send") {
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (/fail/i.test(body.text || "")) return json(res, 502, { error: "The agent's terminal is not accepting input right now." });
      // The dashboard's refusals (native terminal_composer_blocked), as worker/home.mjs answers them.
      if (sendMode === "menu") return json(res, 200, { ok: false, status: "refused", error: "not sent: a menu is open in its pane (Esc closes it)", code: "menu_open" });
      if (sendMode === "prompt") return json(res, 200, { ok: false, status: "refused", error: "not sent: the agent is waiting for approval. Open its Terminal to review the prompt, then retry", code: "permission_prompt" });
      if (sendMode === "queued") return json(res, 200, { ok: true, status: "queued", requestId: randomUUID() });
      if (sendMode === "lost") return json(res, 200, { ok: true, status: "delivered" });
      setTimeout(() => {
        // Files come back as `attachments`, as the worker reads them.
        t.msgs.push(...showAttachments([{ id: `u${now()}`, role: "user", ts: now(), text: body.text }]));
        t.a.status = "working"; t.a.updatedAt = now();
        suggestions.delete(t.a.id);
      }, 2500);
      setTimeout(() => {
        t.msgs.push({ id: `tl${now()}`, role: "tool", ts: now(), text: "", tool: { name: "Bash", input: "git status --short", output: " M cloud/app/app.js\n M cloud/app/styles.css", isError: false } });
      }, 5000);
      setTimeout(() => {
        t.msgs.push({ id: `a${now()}`, role: "assistant", ts: now(), text: `On it. I read your note:\n\n> ${String(body.text).slice(0, 200)}\n\nI'll report back when the change is in.` });
        t.a.status = "review"; t.a.updatedAt = now(); t.a.lastMessage = "On it. I'll report back when the change is in.";
      }, 8000);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    // Edit a sent prompt (chat-edit.js; native chat_edit.rs on a real home):
    // the chat goes back to just before it and the new text runs instead.
    // "fail" in the text answers with an error. A working agent's edit is
    // answered "queued", as a home answers when stopping the turn takes
    // longer than it waits; its receipt comes with the transcript 4 seconds
    // later, failed when the text says "stuck".
    if (p === "/api/app/agent-edit") {
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      const edits = mockEdits(t.msgs);
      if (edits[edits.length - 1]?.entry !== body.latest) return json(res, 502, { error: "This chat has new messages since it was shown. Reload the chat and try again" });
      const at = t.msgs.findIndex((m) => m.id === body.entry);
      if (at < 0) return json(res, 502, { error: "That message is no longer in this chat. Reload the chat and try again" });
      if (/fail/i.test(body.text || "")) return json(res, 502, { error: "could not relaunch run_a1: the agent's terminal is gone" });
      // The message and the turns after it leave at once; the agent records
      // the new one when it takes it, about 2 seconds later on a real home.
      const rewind = () => {
        const from = t.msgs.findIndex((m) => m.id === body.entry);
        t.msgs.splice(from, t.msgs.length - from);
        setTimeout(() => t.msgs.push(...showAttachments([{ id: `u${now()}`, role: "user", ts: now(), text: body.text }])), 2000);
        t.a.status = "working"; t.a.updatedAt = now();
        setTimeout(() => {
          t.msgs.push({ id: `a${now()}`, role: "assistant", ts: now(), text: "Starting from your edited message." });
          t.a.status = "review"; t.a.updatedAt = now(); t.a.lastMessage = "Starting from your edited message.";
        }, 5000);
      };
      if (t.a.status === "working") {
        const requestId = randomUUID();
        setTimeout(() => {
          if (/stuck/i.test(body.text || "")) {
            steerReceipts.set(requestId, { status: "failed", error: "Its turn did not stop within 20 seconds; nothing changed" });
            return;
          }
          rewind();
          steerReceipts.set(requestId, { status: "delivered" });
        }, 4000);
        return json(res, 200, { ok: true, status: "queued", requestId });
      }
      await delay(400);
      rewind();
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/coordinator") {
      coordinator ??= { workspaceId: "wsp_all_agents", name: "rudder-all-agents", repo: "", status: "running", agents: [], lastActivityAt: iso(now()), ...coordinatorHealth() };
      return json(res, 200, { workspace: coordinator.workspaceId });
    }
    if (p === "/api/app/task" && body.workspace === coordinator?.workspaceId) {
      const main = /^\/main\s+([\s\S]+)/.exec(String(body.text || ""));
      if (!main) return json(res, 200, { ok: true, status: "delivered" }); // /model
      const id = `run_all_${Math.random().toString(36).slice(2, 6)}`;
      const codex = allMode === "codex";
      // Codex's transcript names a scripted call by the tool it ran (worker/transcripts.mjs).
      const call = (tool, input) => (codex ? { name: "exec", input: `mcp__all_agents__${tool}` } : { name: `mcp__all_agents__${tool}`, input });
      const a = agent({ id, title: main[1].slice(0, 60), task: main[1], status: "working", backend: codex ? "codex" : "claude", model: codex ? "gpt-5.5" : "claude-sonnet-5-5", mode: "main", createdAt: now(), updatedAt: now(), fresh: true });
      setTimeout(() => coordinator.agents.unshift(a), 900);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `t${now()}`, role: "tool", ts: now(), text: "", tool: { ...call("list_agents", "{}"), output: "rudder [wsp_rudder]\n- run_a1 · working · claude opus · 1m ago · Phone app for agent control\n- run_a2 · waiting · codex gpt-6.1-sol · 4m ago · Fix flaky Ctrl+W chord test\n  needs you: permission for Bash: cargo test --release (prompt ask_1)", isError: false } });
      }, 2500);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `u${now()}`, role: "tool", ts: now(), text: "", tool: { ...call("read_agent", "{\"agent\":\"run_a2\",\"project\":\"rudder\"}"), output: "rudder / Fix flaky Ctrl+W chord test (run_a2)\nstatus: waiting · codex gpt-6.1-sol · updated 4m ago\nneeds you: permission for Bash: cargo test --release (prompt ask_1)\n\n[agent] The chord test passes alone and fails in the full run. Running the release suite to confirm.", isError: false } });
      }, 3500);
      setTimeout(() => {
        transcriptFor(coordinator.workspaceId, id)?.msgs.push({ id: `r${now()}`, role: "assistant", ts: now(), text: "Here is where things stand:\n\n**rudder**\n- *Phone app for agent control* is working: wiring the transcript poll.\n- *Fix flaky Ctrl+W chord test* needs you: it wants to run `cargo test --release` with network access.\n- *Tailnet HTTPS for previews* is done and ready to merge.\n\nWant me to approve the test run, or keep an eye on the working agents and tell you when they finish?" });
        a.status = "done"; a.updatedAt = now(); a.lastMessage = "Here is where things stand.";
      }, 6000);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/task") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "Project not found" });
      const id = `run_${Math.random().toString(36).slice(2, 8)}`;
      const title = String(body.text || "New task").split(/[.\n]/)[0].slice(0, 60);
      proj.agents.unshift(agent({ id, title, task: body.text, status: "working", backend: "claude", model: "opus", mode: "main", createdAt: now(), updatedAt: now(), lastMessage: "Starting up.", fresh: true }));
      proj.lastActivityAt = iso(now());
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/action" && body.action === "approve") {
      const proj = (freshProjects ?? projects).find((x) => x.workspaceId === body.workspace);
      const plan = proj?.plan;
      const waiting = (plan?.plans || []).filter((x) => x.awaitingApproval);
      const target = body.plan ? (plan?.plans || []).find((x) => x.id === body.plan) : waiting.length === 1 ? waiting[0] : null;
      if (!target) return json(res, 404, { error: waiting.length > 1 ? "several plans are waiting: name one" : "plan not found" });
      if (!target.awaitingApproval) return json(res, 409, { error: "plan is not waiting for approval" });
      target.awaitingApproval = false;
      plan.awaitingApproval = plan.plans.some((x) => x.awaitingApproval);
      for (const n of plan.nodes) if (n.status === "ready") n.status = "running";
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/action" && body.action === "reopen") {
      // Like a home: the dashboard moves the record back and resumes it.
      const proj = (freshProjects ?? projects).find((x) => x.workspaceId === body.workspace);
      const at = (proj?.closed || []).findIndex((c) => c.id === body.run);
      if (at < 0) return json(res, 400, { error: `could not reopen it: no closed chat ${body.run}` });
      await delay(600);
      const [c] = proj.closed.splice(at, 1);
      proj.agents.unshift(agent({ id: c.id, title: c.title, task: c.title, status: "done", backend: c.backend, model: c.model, updatedAt: now(), lastMessage: "Reopened from Show closed." }));
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/action") {
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (body.action === "delete") {
        // Like a home: the dashboard reads its steer inbox about once a second,
        // and the state stops listing the agent on the home's next publish,
        // after the answer. /app/__mock/close/fail: the dashboard refuses.
        await delay(900);
        if (closeMode === "fail") return json(res, 400, { error: "could not stop it, so it was not deleted" });
        setTimeout(() => {
          const proj = projects.find((x) => x.agents.includes(t.a));
          if (!proj) return;
          proj.agents = proj.agents.filter((x) => x !== t.a);
          // Kept a day under Show closed.
          proj.closed = [named({ id: t.a.id, title: t.a.title, backend: t.a.backend, model: t.a.model, effort: null, closedAt: now() }), ...(proj.closed || [])];
        }, 1000);
        return json(res, 200, { ok: true, status: "delivered" });
      }
      if (body.action === "pin" || body.action === "unpin") {
        if (body.action === "pin") t.a.pinned = true; else delete t.a.pinned;
        return json(res, 200, { ok: true, status: "delivered" });
      }
      if (body.action === "stop") { t.a.status = "stopped"; t.a.lastMessage = "Stopped by you."; }
      else if (body.action === "merge") {
        if (!["done", "review"].includes(t.a.status)) return json(res, 409, { error: "Only finished agents can be merged." });
        t.a.status = "merged"; t.a.lastMessage = "Merged into main.";
      } else return json(res, 400, { error: "Unknown action" });
      t.a.updatedAt = now();
      t.msgs.push({ id: `s${now()}`, role: "system", ts: now(), text: body.action === "stop" ? "Agent stopped from the app." : "Merged into main." });
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/agent-model") {
      // Another model or effort of the same harness: the chat carries on.
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      const refusal = mockRefusal(t.a.backend, body.account, t.a.backend === "codex" ? "Codex" : "Claude Code");
      if (refusal) return json(res, 400, { error: refusal });
      await delay(250);
      Object.assign(t.a, { model: body.model, modelName: MODEL_NAMES[body.model] ?? body.model, effort: body.effort === "auto" ? null : body.effort, account: body.account, updatedAt: now() });
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/agent-harness") {
      // Like a home: the old harness stops and the new one starts on a
      // handoff (cloud/worker/handoff.mjs), which the chat shows as one
      // marker, then its first reply.
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      if (harnessMode === "old" && homeVersion !== CLOUD_VERSION) return json(res, 400, { error: `Your cloud machine runs Rudder ${homeVersion}, which does not have this yet.`, needsUpdate: true });
      const label = (backend) => (backend === "codex" ? "Codex" : "Claude");
      // An account the machine is not signed in to is refused (handoff.mjs handoffRefusal).
      const refusal = mockRefusal(body.backend, body.account, label(body.backend));
      if (refusal) return json(res, 400, { error: refusal });
      await delay(250);
      const from = [label(t.a.backend), t.a.modelName].filter(Boolean).join(" · ");
      Object.assign(t.a, { backend: body.backend, model: body.model, modelName: MODEL_NAMES[body.model] ?? body.model, effort: body.effort === "auto" ? null : body.effort, account: body.account, status: "working", updatedAt: now() });
      const asks = body.instructions ? `## The user's instructions for this handoff\n${body.instructions}\n\n` : "";
      const summary = `## Goal\n${t.a.task}\n\n${asks}## Where it stopped\nIt had finished its last turn and was waiting for the user.`;
      t.msgs.splice(0, t.msgs.length, { id: `h${now()}`, role: "system", kind: "handoff", ts: now(), text: `Moved from ${from}`, handoff: { from, summary } });
      setTimeout(() => {
        t.msgs.push({ id: `r${now()}`, role: "assistant", ts: now(), text: `Picked up from ${from}. ${t.a.task}: done and waiting for review. Tell me what to look at next.` });
        t.a.status = "review";
      }, 1800);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/agent-rename") {
      // Like the dashboard: a name with "fail" in it is refused, and the
      // state shows a new name a moment after the rename is delivered.
      const t = transcriptFor(body.workspace, body.run);
      if (!t) return json(res, 404, { error: "Agent not found" });
      const title = String(body.title || "").replace(/\s+/g, " ").trim();
      if (!title) return json(res, 400, { error: "a name can't be empty" });
      await delay(300);
      if (/fail/i.test(title)) return json(res, 502, { error: "the dashboard could not deliver it" });
      setTimeout(() => { t.a.title = title.slice(0, 200); }, 1500);
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/pin") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "project not found" });
      proj.pinnedAt = body.pinned === true ? proj.pinnedAt || iso(now()) : null;
      return json(res, 200, { workspaceId: proj.workspaceId, pinnedAt: proj.pinnedAt });
    }
    if (p === "/api/app/project-order") {
      const ids = body.workspaces;
      if (!Array.isArray(ids) || new Set(ids).size !== ids.length) return json(res, 400, { error: "workspaces must list project ids, each once" });
      if (ids.some((id) => !projects.some((x) => x.workspaceId === id))) return json(res, 404, { error: "project not found" });
      for (const x of projects) x.sortOrder = ids.includes(x.workspaceId) ? ids.indexOf(x.workspaceId) : null;
      return json(res, 200, { workspaces: ids });
    }
    // One project's agents in the sidebar; projects never moved have no agentOrder.
    if (p === "/api/app/agent-order") {
      const proj = projects.find((x) => x.workspaceId === body.workspace);
      if (!proj) return json(res, 404, { error: "project not found" });
      if (!Array.isArray(body.runs) || new Set(body.runs).size !== body.runs.length) return json(res, 400, { error: "runs must list agent ids, each once" });
      proj.agentOrder = body.runs;
      return json(res, 200, { workspaceId: proj.workspaceId, runs: body.runs });
    }
    // Sign-ins: what logins.mjs on the machine answers. A machine that does
    // not exist yet is created first (503 until it is up, like the server).
    if (p === "/api/app/login/start") {
      if (homeMode !== "running") {
        if (homeMode !== "queued") { homeMode = "queued"; setTimeout(() => { homeMode = "running"; }, 5000); }
        return json(res, 503, { error: "the cloud machine is still starting; try again in a moment" });
      }
      const provider = body.provider;
      const id = `login-${now().toString(36)}`;
      const device = provider === "codex" && body.device === true;
      // "new" adds an account (home.mjs createAccount); its id is a base-36 time.
      const account = body.account === "new" ? now().toString(36) : body.account;
      logins[provider] = { id, provider, account, mode: device ? "device" : "browser", status: "waiting", error: null, startedAt: iso(now()), endedAt: null };
      if (device) {
        setTimeout(() => {
          if (logins.codex?.id !== id || logins.codex.status !== "waiting") return;
          Object.assign(logins.codex, { status: "done", endedAt: iso(now()) });
          putAccount("codex", mockSignedIn("codex", account));
        }, 7000);
      }
      return json(res, 200, {
        ...logins[provider],
        url: device ? "https://auth.openai.com/codex/device" : provider === "claude"
          ? "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=http%3A%2F%2Flocalhost%3A41643%2Fcallback&state=mock"
          : "https://auth.openai.com/oauth/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&state=mock",
        callback: device ? null : { port: provider === "claude" ? 41643 : 1455, path: provider === "claude" ? "/callback" : "/auth/callback" },
        codeUrl: provider === "claude" ? "https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=mock" : null,
        userCode: device ? "1QX5-T3MAL" : null,
      });
    }
    // Tailscale: the machine's `tailscale up` hands back a sign-in link, then
    // finishes on its own (tailscale-login.mjs); tailscale-flow/ picks how.
    if (p === "/api/app/tailscale/connect") {
      if (homeMode !== "running") {
        if (homeMode !== "queued") { homeMode = "queued"; setTimeout(() => { homeMode = "running"; }, 5000); }
        return json(res, 503, { error: "the cloud machine is still starting; try again in a moment" });
      }
      const url = "https://login.tailscale.com/a/1f2e3d4c5b6a";
      if (tailscaleMode === "on") return json(res, 200, { ...tailscaleSession("done"), url: null });
      if (tailscaleLogin && (tailscaleLogin.status === "waiting" || tailscaleLogin.status === "approval")) return json(res, 200, { ...tailscaleLogin, url });
      await delay(900);
      const login = tailscaleSession("waiting");
      tailscaleLogin = login;
      tailscaleMode = "login";
      const finish = (status, error = null) => {
        if (tailscaleLogin !== login) return;
        Object.assign(login, { status, error, endedAt: iso(now()) });
        tailscaleMode = status === "done" ? "on" : "off";
      };
      setTimeout(() => {
        if (tailscaleLogin !== login || login.status !== "waiting") return;
        if (tailscaleFlow === "refused") return finish("failed", "backend error: this tailnet does not allow new devices right now");
        if (tailscaleFlow !== "approval") return finish("done");
        login.status = "approval";
        tailscaleMode = "approval";
        setTimeout(() => finish("done"), 8000);
      }, 7000);
      return json(res, 200, { ...login, url });
    }
    if (p === "/api/app/tailscale/disconnect") {
      await delay(600);
      tailscaleMode = "off";
      tailscaleLogin = null;
      return json(res, 200, tailnet());
    }
    if (p === "/api/app/login/finish") {
      const login = logins[body.provider];
      if (!login || login.id !== body.id) return json(res, 400, { error: "That sign-in is no longer running. Start it again." });
      await delay(900);
      if (body.code === "bad") Object.assign(login, { status: "failed", error: "OAuth error: Invalid code", endedAt: iso(now()) });
      else {
        Object.assign(login, { status: "done", endedAt: iso(now()) });
        putAccount(body.provider, mockSignedIn(body.provider, login.account));
      }
      return json(res, 200, login);
    }
    // An account's menu in Settings (home.mjs changeAccount).
    if (p === "/api/app/agent-account") {
      const { provider, account, action } = body;
      if (!accountRow(provider, account)) return json(res, 400, { error: "That account is no longer on your cloud machine." });
      await delay(400);
      if (action === "default") creds = { ...creds, [provider]: creds[provider].map((a) => ({ ...a, default: a.id === account })) };
      else if (action === "signOut") putAccount(provider, { ...accountRow(provider, account), ok: false, signedIn: false, usageLimit: null, detail: "not signed in" });
      else if (action === "remove") {
        const left = creds[provider].filter((a) => a.id !== account);
        creds = { ...creds, [provider]: left.some((a) => a.default) ? left : left.map((a) => ({ ...a, default: a.id === "main" })) };
      } else return json(res, 400, { error: "Unknown action" });
      return json(res, 200, { ok: true, credentials: creds });
    }
    if (p === "/api/app/login/cancel") {
      const login = logins[body.provider];
      if (login && login.status === "waiting") Object.assign(login, { status: "cancelled", endedAt: iso(now()) });
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/computer/lookup" || p === "/api/app/computer/approve") {
      const code = String(body.code || "").toUpperCase().replace(/[^A-Z0-9]/g, "").replace(/^(.{3})(.{3})$/, "$1-$2");
      const computer = computers.get(code);
      if (!computer || computer.expiresAt < now()) return json(res, 400, { error: "that code is wrong or expired; run `rudder login` again for a new one" });
      if (p.endsWith("/lookup")) return json(res, 200, { code, deviceName: computer.deviceName, expiresAt: iso(computer.expiresAt) });
      computers.delete(code);
      return json(res, 200, { ok: true, approved: body.approve === true, deviceName: computer.deviceName });
    }
    if (p === "/api/app/github/start") {
      githubFlow = githubFlow || { userCode: "WDJB-MJHT", polls: 0 };
      return json(res, 200, { status: "waiting", userCode: githubFlow.userCode, verificationUri: "https://github.com/login/device", expiresAt: iso(now() + 15 * MIN), interval: 5 });
    }
    if (p === "/api/app/github/poll") {
      if (!githubFlow) return json(res, 200, { status: "failed", error: "The code expired. Start again." });
      githubFlow.polls += 1;
      if (githubFlow.polls < 3) return json(res, 200, { status: "waiting", userCode: githubFlow.userCode, verificationUri: "https://github.com/login/device", interval: 5 });
      githubFlow = null;
      githubConnected = true;
      return json(res, 200, { status: "done", account: "viraatdas" });
    }
    if (p === "/api/app/github/cancel") {
      githubFlow = null;
      return json(res, 200, { ok: true });
    }
    if (p === "/api/app/wake") {
      homeMode = "queued";
      setTimeout(() => { homeMode = "running"; }, 8000);
      return json(res, 200, { home: home() });
    }
    if (p === "/api/app/screen") {
      return json(res, 200, { updatedAt: Date.now(), text: [
        " Select model",
        " Switch between Claude models. Applies to this session.",
        "",
        "   1. Default (recommended)  Opus 5.5 for complex work",
        " ❯ 2. Sonnet                  Sonnet 5.5 for daily coding",
        "   3. Haiku                   Haiku 5.5 for quick answers",
        "",
        " Enter to confirm · Esc to exit",
      ].join("\n") });
    }
    if (p === "/api/app/withdraw") {
      return json(res, 200, steerReceipts.has(body.requestId) ? steerReceipts.get(body.requestId) : { status: "withdrawn" });
    }
    if (p === "/api/app/keys") {
      // Esc closes the menu or declines the prompt a message was refused for.
      if (body.keys === "esc" && (sendMode === "menu" || sendMode === "prompt")) sendMode = "ok";
      return json(res, 200, { ok: true, status: "delivered" });
    }
    if (p === "/api/app/models") {
      // The shape `rudder __models` returns (src/models.ts discoverHarnesses), cut from a real home.
      const all = ["low", "medium", "high", "xhigh", "max"];
      const gpt = ["low", "medium", "high", "xhigh"];
      const c = (label, value, detail, efforts = all) => ({ label, value, detail, efforts });
      const alias = (...args) => ({ ...c(...args), alias: true });
      return json(res, 200, {
        current: { backend: "claude", model: "claude-opus-5-5", name: "Opus 5.5", effort: "high" },
        harnesses: [
          { id: "claude", label: "Claude", account: "Claude Code", models: [
            c("Opus 5.5", "claude-opus-5-5", "For complex work and everyday tasks"),
            c("Sonnet 5.5", "claude-sonnet-5-5", "Most efficient for simpler tasks"),
            c("Haiku 5.5", "claude-haiku-5-5", "Fastest for quick answers"),
            c("Fable 5.2", "claude-fable-5-2", "For your toughest challenges"),
            c("Fable 5.1", "claude-fable-5-1", "Most capable for your hardest and longest-running tasks"),
            c("Opus 5", "claude-opus-5", "Best for everyday, complex tasks"),
            c("Fable 5", "claude-fable-5", "Most capable for your hardest and longest-running tasks"),
            c("Sonnet 5", "claude-sonnet-5", "Efficient for routine tasks"),
            c("Opus 4.8", "claude-opus-4-8", "Best for everyday, complex tasks"),
            c("Opus 4.7", "claude-opus-4-7", "Best for everyday, complex tasks"),
            c("Opus 4.6", "claude-opus-4-6", "Best for everyday, complex tasks", ["low", "medium", "high", "max"]),
            c("Sonnet 4.6", "claude-sonnet-4-6", "Efficient for routine tasks", ["low", "medium", "high", "max"]),
            c("Haiku 4.5", "claude-haiku-4-5-20251001", "Fastest for quick answers", []),
            alias("Opus 5.5", "opus", "For complex work and everyday tasks"),
            alias("Sonnet 5.5", "sonnet", "Most efficient for simpler tasks"),
            alias("Haiku 5.5", "haiku", "Fastest for quick answers"),
            alias("Fable 5.2", "fable", "For your toughest challenges"),
          ] },
          { id: "codex", label: "Codex", account: "ChatGPT", models: [
            c("Astra 6.2", "gpt-6.2-astra", "Our most capable model for complex coding and professional work.", gpt),
            c("Sol 6.2", "gpt-6.2-sol", "Fast, go-to model for coding and everyday work.", gpt),
            c("Sol 6.1", "gpt-6.1-sol", "Previous generation model for coding and everyday work.", gpt),
            c("Terra 6.1", "gpt-6.1-terra", "Efficient model that excels at visual understanding and computer use.", gpt),
            c("Astra 6", "gpt-6-astra", "Previous generation model for complex coding and professional work.", gpt),
            c("Sol 6", "gpt-6-sol", "Legacy model for coding and everyday work.", gpt),
            c("Luna 6", "gpt-6-luna", "Fast and affordable model for easier tasks.", gpt),
            c("Sol 5.6", "gpt-5.6-sol", "Legacy model for coding and everyday work.", gpt),
            c("Terra 5.6", "gpt-5.6-terra", "Legacy balanced model for agentic coding.", gpt),
            c("Luna 5.6", "gpt-5.6-luna", "Legacy fast model for simpler tasks.", gpt),
          ] },
          // Real ids from a home's `opencode models` (411 there); OpenCode's
          // names, and the context and release date as the detail.
          { id: "opencode", label: "OpenCode", account: "OpenCode", models: [
            c("Beam Free", "opencode/beam-free", "200K context · 2026-10-02", []),
            c("Nemotron 3.5 Super VL Free", "opencode/nemotron-3.5-super-vl-free", "256K context · 2026-09-18", []),
            c("Kimi K3.1 Pro", "baseten/moonshotai/Kimi-K3.1-Pro", "262K context · 2026-08-21", ["high"]),
            c("Morph DSv4.1 Flash", "morph/morph-dsv41flash", "128K context · 2026-07-30", []),
            c("Morph v3 Fast", "morph/morph-v3-fast", "32K context · 2025-08-15", []),
            c("Gemini Flash Latest", "openrouter/~google/gemini-flash-latest", "1M context · 2026-10-14", ["low", "high"]),
            c("Grok Latest", "openrouter/~x-ai/grok-latest", "2M context · 2026-09-30", []),
            c("Nova 2 Lite", "openrouter/amazon/nova-2-lite-v1", "1M context · 2025-12-02", []),
            c("Claude Haiku 5.5", "openrouter/anthropic/claude-haiku-5.5", "200K context · 2026-10-20", ["high", "max"]),
            c("Claude Sonnet 4.6", "openrouter/anthropic/claude-sonnet-4.6", "1M context · 2026-02-17", ["high", "max"]),
            c("Dolphin Mistral 24B Venice Edition", "openrouter/cognitivecomputations/dolphin-mistral-24b-venice-edition", "33K context · 2025-07-09", []),
            c("DeepSeek V3.1", "openrouter/deepseek/deepseek-chat-v3.1", "164K context · 2025-08-21", []),
            c("DeepSeek V4 Pro", "openrouter/deepseek/deepseek-v4-pro-0813", "1M context · 2026-08-13", ["high"]),
            c("Gemini 3 Pro Image Preview", "openrouter/google/gemini-3-pro-image-preview", "66K context · 2025-11-20", []),
            c("Gemini 3.5 Flash Lite", "openrouter/google/gemini-3.5-flash-lite", "1M context · 2026-07-22", ["low", "high"]),
            c("Gemini Omni 1.1 Flash", "openrouter/google/gemini-omni-1.1-flash", "1M context · 2026-10-08", ["low", "high"]),
            c("Gemma 4 31B (free)", "openrouter/google/gemma-4-31b-it:free", "128K context · 2026-04-02", []),
            c("Ling 3.2 Flash (free)", "openrouter/inclusionai/ling-3.2-flash:free", "128K context · 2026-09-04", []),
            c("Llama 3.1 70B Instruct", "openrouter/meta-llama/llama-3.1-70b-instruct", "131K context · 2024-07-23", []),
            c("Muse Glimmer 30B", "openrouter/meta/muse-glimmer-30b", "256K context · 2026-10-01", []),
            c("MiniMax M2.5", "openrouter/minimax/minimax-m2.5", "197K context · 2026-02-12", ["high"]),
            c("Mistral Large", "openrouter/mistralai/mistral-large", "128K context · 2024-02-26", []),
            c("Mistral Small 3", "openrouter/mistralai/mistral-small-24b-instruct-2501", "33K context · 2025-01-30", []),
            c("Kimi K2 0905", "openrouter/moonshotai/kimi-k2-0905", "262K context · 2025-09-05", []),
            c("Motif 3", "openrouter/motif-technologies/motif-3", "128K context · 2026-09-12", []),
            c("Nemotron 3 Ultra 550B A55B (free)", "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free", "256K context · 2026-06-10", ["high"]),
            c("GPT-4.1", "openrouter/openai/gpt-4.1", "1M context · 2025-04-14", []),
            c("GPT-5", "openrouter/openai/gpt-5", "400K context · 2025-08-07", ["low", "medium", "high"]),
            c("GPT-5.1-Codex-Mini", "openrouter/openai/gpt-5.1-codex-mini", "400K context · 2025-11-13", ["low", "medium", "high"]),
            c("GPT-5.4 Pro", "openrouter/openai/gpt-5.4-pro", "1.1M context · 2026-03-05", ["medium", "high", "xhigh"]),
            c("GPT-6 Astra", "openrouter/openai/gpt-6-astra", "1M context · 2026-08-04", ["low", "medium", "high", "xhigh"]),
            c("GPT-6.1 Terra Pro", "openrouter/openai/gpt-6.1-terra-pro", "1M context · 2026-10-09", ["low", "medium", "high", "xhigh"]),
            c("gpt-oss-20b", "openrouter/openai/gpt-oss-20b", "131K context · 2025-08-05", ["low", "medium", "high"]),
            c("Particle 1", "openrouter/particle-ai/particle-1", "128K context · 2026-09-25", []),
            c("Ternary Bonsai 2 27B", "openrouter/prism-ml/ternary-bonsai-2-27b", "64K context · 2026-08-28", []),
            c("Qwen3 30B A3B", "openrouter/qwen/qwen3-30b-a3b", "41K context · 2025-04-28", []),
            c("Qwen3 VL 30B A3B Instruct", "openrouter/qwen/qwen3-vl-30b-a3b-instruct", "262K context · 2025-10-03", []),
            c("Qwen3.6 27B", "openrouter/qwen/qwen3.6-27b", "262K context · 2026-05-14", ["high"]),
            c("Qwen3.8 Flash", "openrouter/qwen/qwen3.8-flash", "1M context · 2026-10-16", []),
            c("Relace Apply 3", "openrouter/relace/relace-apply-3", "256K context · 2025-09-26", []),
            c("ScaleDown Extract", "openrouter/scaledown/extract", "128K context · 2026-10-06", []),
            c("Hunyuan 4", "openrouter/tencent/hy4", "256K context · 2026-09-09", []),
            c("Pareto 26.10", "openrouter/unbiased/pareto-26.10", "200K context · 2026-10-22", []),
            c("Grok 4.3", "openrouter/x-ai/grok-4.3", "2M context · 2026-05-01", ["low", "high"]),
            c("GLM 4.5V", "openrouter/z-ai/glm-4.5v", "66K context · 2025-08-11", []),
            c("GLM-5.3", "openrouter/z-ai/glm-5.3", "205K context · 2026-09-15", ["high"]),
          ] },
        ],
      });
    }
    if (p === "/api/app/repos") {
      return json(res, 200, {
        owners: ["viraatdas", "exla-ai"],
        repos: [
          { repo: "exla-ai/smol", description: "Smol models", private: true, updatedAt: new Date(Date.now() - 3600e3).toISOString(), open: true },
          { repo: "exla-ai/vizzer", description: null, private: true, updatedAt: new Date(Date.now() - 86400e3).toISOString(), open: false },
          { repo: "exla-ai/rudder", description: "Run coding agents in parallel", private: false, updatedAt: new Date().toISOString(), open: true },
        ],
      });
    }
    if (p === "/api/app/create" && body.local) {
      const name = String(body.name || "");
      const ws = `wsl_${name.replace(/[^\w]/g, "_")}`;
      const list = freshProjects ?? projects;
      if (list.some((x) => x.name === name)) return json(res, 409, { error: `a project called ${name} is already on your cloud machine` });
      list.push({ workspaceId: ws, name, repo: "", ref: null, status: "queued", failureReason: null, dashboard: "cloning", clients: 0, lastActivityAt: iso(now()), agents: [], ports: [] });
      setTimeout(() => { const x = list.find((q) => q.workspaceId === ws); if (x) { x.dashboard = "running"; x.status = "running"; } }, 4000);
      if (homeMode === "none" || homeMode === "stopped") homeMode = "queued", setTimeout(() => { homeMode = "running"; }, 6000);
      return json(res, 200, { home: home(), workspace: { workspaceId: ws } });
    }
    if (p === "/api/app/create") {
      return json(res, 200, { workspace: { id: "ws-new", workspaceId: "ws-new", repo: `${body.owner}/${body.name}` } });
    }
    if (p === "/api/app/open") {
      const repo = String(body.repo || "");
      if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) return json(res, 400, { error: "Repository must look like owner/name." });
      const ws = `wsp_${repo.replace(/[^\w]/g, "_")}`;
      const list = freshProjects ?? projects;
      if (!list.find((x) => x.workspaceId === ws)) {
        list.push({ workspaceId: ws, name: repo.split("/")[1], repo, ref: null, status: "queued", failureReason: null, dashboard: "cloning", clients: 0, lastActivityAt: iso(now()), agents: [], ports: [] });
        setTimeout(() => { const x = list.find((q) => q.workspaceId === ws); if (x) { x.dashboard = "running"; x.status = "running"; } }, 10000);
      }
      if (homeMode === "none" || homeMode === "stopped") homeMode = "queued", setTimeout(() => { homeMode = "running"; }, 6000);
      return json(res, 200, { home: home(), workspace: { workspaceId: ws } });
    }
    return json(res, 404, { error: "Not found" });
  }

  if (p === "/app" || p.startsWith("/app/")) return serveStatic(req, res, url);
  json(res, 404, { error: "Not found" });
});

// The agent terminal's canonical screen socket (cloud/src/private-access/
// terminal-relay.ts): one read-only screen, a shell session with colored
// `ls`, `git diff` and the 16 ANSI colors, for the terminal panel.
const { WebSocketServer } = await import("ws");
const termSockets = new WebSocketServer({ noServer: true });
function mockTerminalScreen() {
  const columns = 100, rows = 30;
  const E = "\x1b[", sgr = (c, t) => `${E}${c}m${t}${E}0m`;
  const prompt = (cmd) => `${sgr("36", "~/code/rudder")} ${sgr("35", "main")} ${sgr("32", "$")} ${cmd}`;
  const lines = [
    prompt("ls --color"),
    `${sgr("1;34", "cloud")}  ${sgr("1;34", "docs")}  ${sgr("1;34", "engine")}  ${sgr("1;34", "mac")}  ${sgr("1;34", "native")}  ${sgr("1;34", "node_modules")}  package.json  README.md  ${sgr("1;32", "rudder.sh")}  ${sgr("1;34", "src")}  ${sgr("1;34", "tests")}`,
    prompt("git diff --stat"),
    ` cloud/app/terminal-panel.js | 312 ${sgr("32", "++++++++++++++++++++++++++")}`,
    ` cloud/app/app.js            |  18 ${sgr("32", "++")}${sgr("31", "----")}`,
    ` 2 files changed, 324 insertions(+), 6 deletions(-)`,
    prompt("git diff cloud/app/app.js"),
    sgr("1", "diff --git a/cloud/app/app.js b/cloud/app/app.js"),
    sgr("1", "index 3f1c2a4..8b9e0d1 100644"),
    sgr("1", "--- a/cloud/app/app.js"),
    sgr("1", "+++ b/cloud/app/app.js"),
    `${sgr("36", "@@ -14,7 +14,7 @@")} import { canvasButton, followCanvas } from "/app/canvas-panel.js";`,
    ` import { FilesHubScreen } from "/app/files-hub.js";`,
    sgr("31", `-import { CanonicalTerminalView } from "/app/terminal-client.js";`),
    sgr("32", `+import { followTerminal, setTerminal, terminalOpen } from "/app/terminal-panel.js";`),
    ` import { mountCommandPalette } from "/app/command-palette.js";`,
    prompt("npm run check"),
    sgr("2", "> rudder@5.0.11 check"),
    sgr("2", "> tsc -p tsconfig.json --noEmit"),
    `${sgr("33", "warn")} 1 file uses a deprecated API: ${sgr("4", "src/cloud.ts")}`,
    `${sgr("1;32", "done")} No type errors ${sgr("90", "(2.4s)")}`,
    prompt("colors"),
    [0, 1, 2, 3, 4, 5, 6, 7].map((n) => `${E}3${n}m${String(n).padEnd(2)}${E}0m ${E}4${n}m    ${E}0m`).join(" "),
    [0, 1, 2, 3, 4, 5, 6, 7].map((n) => `${E}9${n}m${String(n + 8).padEnd(2)}${E}0m ${E}10${n}m    ${E}0m`).join(" "),
    prompt(""),
  ];
  const plain = (t) => t.replace(/\x1b\[[0-9;]*m/g, "");
  const screen = `${E}?25h${E}m${E}H${E}J${lines.map((l, i) => `${E}${i + 1};1H${l}`).join("")}${E}${lines.length};${plain(lines.at(-1)).length + 1}H`;
  const scrollback = ["Last login: Wed Nov 19 08:02:11 on ttys004", plain(prompt("cd ~/code/rudder")), plain(prompt("git status")), "On branch main", "nothing to commit, working tree clean"];
  return { format: "vt100-ansi-v1", columns, rows, screen, scrollback, text: lines.map(plain).join("\n") };
}
server.on("upgrade", (req, socket, head) => {
  const url = new URL(req.url, "http://mock");
  if (url.pathname !== "/api/app/terminal-screen") { socket.destroy(); return; }
  termSockets.handleUpgrade(req, socket, head, (ws) => {
    const generation = `mock-${Date.now()}`;
    let sequence = 0;
    const send = () => ws.send(JSON.stringify({ version: 1, type: "terminal-screen", generation, sequence: ++sequence, reset: true, gap: false, screen: mockTerminalScreen() }));
    ws.send(JSON.stringify({ version: 1, type: "terminal-capability", capability: "canonical-screen-text-v1", access: "read-only", generation }));
    send();
    ws.on("message", (data) => { try { if (JSON.parse(String(data)).type === "terminal-snapshot-request") send(); } catch { /* not JSON */ } });
  });
});

server.listen(PORT, () => console.log(`Rudder app mock on http://localhost:${PORT}/app`));
