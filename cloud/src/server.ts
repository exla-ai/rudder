import { createHash, createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import fs from "node:fs/promises";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { Readable } from "node:stream";
import { betterAuth } from "better-auth";
import Database from "better-sqlite3";
import { toNodeHandler } from "better-auth/node";
import { DeleteObjectCommand, GetObjectCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import type { Duplex } from "node:stream";
import { WebSocket, WebSocketServer } from "ws";
import { createSecretsVault, SecretsVaultError, type SecretItemInput, type SecretsVault } from "./secrets.js";
import {
  formatOutputForSlack,
  parseSlackCommand,
  postSlackMessage,
  slackConfigFromEnv,
  SLACK_HELP_TEXT,
  verifySlackSignature,
} from "./slack.js";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

type JsonRecord = Record<string, Json>;

type DeviceLogin = {
  deviceCode: string;
  token?: string;
  accountId?: string;
  email?: string;
  expiresAt: number;
};

type GithubBrowserLogin = {
  githubDeviceCode: string;
  userCode: string;
  verificationUri: string;
  verificationUriComplete?: string;
  expiresAt: number;
  intervalMs: number;
  nextPollAt: number;
};

type SailStatus = "queued" | "running" | "paused" | "completed" | "failed" | "deleting";
type SailRuntime = "fly" | "byo-vm";

type Sail = {
  id: string;
  status: SailStatus;
  runtime: SailRuntime;
  repoName?: string;
  task?: string;
  branch?: string;
  machineId?: string;
  machineState?: string;
  snapshotKey?: string;
  lastHeartbeatAt?: string;
  slackThreadTs?: string;
  createdAt: string;
  updatedAt: string;
  bootstrapCommand?: string;
  failureReason?: string;
};

type WorkspaceStatus = "queued" | "running" | "paused" | "stopped" | "failed" | "deleting";
// snapshot / git-clone: one Fly machine per repo (the original shape).
// home: the account's single long-lived machine that holds every repo.
// home-repo: one repo on the home machine; owns no machine or volume itself.
type WorkspaceSourceKind = "snapshot" | "git-clone" | "home" | "home-repo";

type Workspace = {
  id: string;
  accountId: string;
  workspaceKey: string;
  repoName?: string;
  status: WorkspaceStatus;
  machineId?: string;
  machineState?: string;
  snapshotKey?: string;
  snapshotFingerprint?: string;
  region?: string;
  volumeId?: string;
  sourceKind?: WorkspaceSourceKind;
  repoUrl?: string;
  gitRef?: string;
  // Set on "home-repo" rows: the account's home machine row this repo lives on.
  homeId?: string;
  // Why the row last failed (machine start, clone, worker exit). Cleared on a
  // successful start so a stale reason never outlives the failure.
  failureReason?: string;
  // Set on "home-repo" rows the user pinned to the top of the phone app.
  pinnedAt?: string;
  lastActivityAt?: string;
  lastHeartbeatAt?: string;
  activeAgents?: number;
  createdAt: string;
  updatedAt: string;
};

type FlyMachine = {
  id?: string;
  name?: string;
  state?: string;
  instance_id?: string;
  config?: JsonRecord;
};

const port = Number(process.env.PORT || 3000);
const baseURL = requiredEnv("BETTER_AUTH_URL", `http://localhost:${port}`);
const authBaseURL = `${baseURL.replace(/\/$/, "")}/api/auth`;
const dataDir = process.env.RUDDER_CLOUD_DATA_DIR || path.join(os.homedir(), ".rudder-cloud");
const dbPath = process.env.RUDDER_CLOUD_DB || path.join(dataDir, "rudder-cloud.sqlite");
const awsRegion = process.env.AWS_REGION || process.env.AWS_DEFAULT_REGION || "us-east-1";
const snapshotBucket = process.env.RUDDER_S3_BUCKET || "";
const flyApiToken = process.env.FLY_API_TOKEN || "";
const flyApiBase = (process.env.FLY_API_HOSTNAME || "https://api.machines.dev").replace(/\/$/, "");
const flyAppName = (process.env.RUDDER_FLY_APP_NAME || process.env.FLY_APP_NAME || "").trim();
const flyRegion = (process.env.RUDDER_FLY_REGION || process.env.FLY_REGION || "iad").trim();
const flyWorkerImage = process.env.RUDDER_WORKER_IMAGE || "ghcr.io/viraatdas/rudder-worker:latest";
const flyWorkerMemoryMb = Number(process.env.RUDDER_WORKER_MEMORY_MB || 1024);
const flyWorkerCpus = Number(process.env.RUDDER_WORKER_CPUS || 1);
const flyWorkerCpuKind = process.env.RUDDER_WORKER_CPU_KIND || "shared";
const flyWorkspaceVolumeGb = Number(process.env.RUDDER_WORKSPACE_VOLUME_GB || 3);
const idlePauseMs = Number(process.env.RUDDER_IDLE_PAUSE_MS || 120 * 60 * 1000);
const stateKey = process.env.RUDDER_CLOUD_STATE_KEY || "control-plane/rudder-cloud.sqlite";
const persistStateToS3 = process.env.RUDDER_CLOUD_PERSIST_STATE !== "0";
const githubDeviceClientId = process.env.RUDDER_GITHUB_DEVICE_CLIENT_ID || "178c6fc778ccc68e1d6a";
const publicLoginUrl = (process.env.RUDDER_PUBLIC_LOGIN_URL || "").trim();
const adminEmails = new Set((process.env.RUDDER_ADMIN_EMAILS || "viraat.laldas@gmail.com,viraat@exla.ai")
  .split(",")
  .map((email) => email.trim().toLowerCase())
  .filter(Boolean));
// Slack user IDs authorized to issue control commands (list/output/stop/talk) in the
// shared channel. The channel is multi-tenant, so without this any channel member
// could drive any account's cloud instances — fail closed: when unset, control
// commands are refused and only `help` works.
const slackAllowedUsers = new Set((process.env.RUDDER_SLACK_ALLOWED_USERS || "")
  .split(",")
  .map((id) => id.trim())
  .filter(Boolean));
const deviceLogins = new Map<string, DeviceLogin>();
const githubBrowserLogins = new Map<string, GithubBrowserLogin>();
const s3 = new S3Client({ region: awsRegion });
const slack = slackConfigFromEnv();
// Slack message events are delivered at-least-once; dedupe by event_id so a
// retry doesn't double-inject into an instance.
const seenSlackEvents = new Set<string>();

await fs.mkdir(dataDir, { recursive: true });
await restoreDatabaseFromS3();

const database = new Database(dbPath);
database.pragma("journal_mode = WAL");
database.exec(`
  create table if not exists user (
    id text primary key not null,
    name text not null,
    email text not null unique,
    emailVerified integer not null,
    image text,
    createdAt date not null,
    updatedAt date not null
  );
  create table if not exists session (
    id text primary key not null,
    expiresAt date not null,
    token text not null unique,
    createdAt date not null,
    updatedAt date not null,
    ipAddress text,
    userAgent text,
    userId text not null references user(id) on delete cascade
  );
  create index if not exists session_userId_idx on session(userId);
  create table if not exists account (
    id text primary key not null,
    accountId text not null,
    providerId text not null,
    userId text not null references user(id) on delete cascade,
    accessToken text,
    refreshToken text,
    idToken text,
    accessTokenExpiresAt date,
    refreshTokenExpiresAt date,
    scope text,
    password text,
    createdAt date not null,
    updatedAt date not null
  );
  create index if not exists account_userId_idx on account(userId);
  create table if not exists verification (
    id text primary key not null,
    identifier text not null,
    value text not null,
    expiresAt date not null,
    createdAt date not null,
    updatedAt date not null
  );
  create index if not exists verification_identifier_idx on verification(identifier);
  create table if not exists rudder_tokens (
    token_hash text primary key,
    account_id text not null,
    email text,
    created_at text not null,
    last_used_at text
  );
  create table if not exists rudder_sails (
    id text primary key,
    account_id text not null,
    status text not null,
    runtime text,
    repo_name text,
    task text,
    branch text,
    machine_id text,
    machine_state text,
    snapshot_key text,
    manifest_json text,
    worker_token_hash text,
    last_heartbeat_at text,
    created_at text not null,
    updated_at text not null
  );
  create table if not exists rudder_settings (
    key text primary key,
    value text not null,
    updated_at text not null
  );
  create table if not exists rudder_workspaces (
    id text primary key,
    account_id text not null,
    workspace_key text not null,
    repo_name text,
    status text not null,
    machine_id text,
    machine_state text,
    snapshot_key text,
    worker_token_hash text,
    last_activity_at text,
    last_heartbeat_at text,
    created_at text not null,
    updated_at text not null,
    volume_id text,
    unique(account_id, workspace_key)
  );
`);
ensureColumn("rudder_sails", "worker_token_hash", "text");
ensureColumn("rudder_sails", "last_heartbeat_at", "text");
ensureColumn("rudder_sails", "runtime", "text");
ensureColumn("rudder_sails", "slack_thread_ts", "text");
ensureColumn("rudder_workspaces", "region", "text");
ensureColumn("rudder_workspaces", "snapshot_fingerprint", "text");
ensureColumn("rudder_workspaces", "volume_id", "text");
ensureColumn("rudder_workspaces", "source_kind", "text");
ensureColumn("rudder_workspaces", "repo_url", "text");
ensureColumn("rudder_workspaces", "git_ref", "text");
// Busy signal reported by the worker supervisor in each heartbeat: how many
// rudder runs are actively working in that machine. The idle sweep must not
// stop a workspace whose agents are mid-task just because nobody is attached
// (the laptop disconnected) — that is the "sessions survive the commute" case.
ensureColumn("rudder_workspaces", "active_agents", "integer");
// Why the reaper could not finish deleting a row yet (see sweepDeletions).
ensureColumn("rudder_sails", "delete_error", "text");
ensureColumn("rudder_workspaces", "delete_error", "text");
ensureColumn("rudder_workspaces", "home_id", "text");
ensureColumn("rudder_workspaces", "failure_reason", "text");
ensureColumn("rudder_sails", "failure_reason", "text");
ensureColumn("rudder_workspaces", "pinned_at", "text");

// Secrets vault: initialized lazily so a missing RUDDER_SECRETS_KEY only
// breaks the vault endpoints, not the rest of the control plane.
const secretsKeyBase64 = (process.env.RUDDER_SECRETS_KEY || "").trim();
let secretsVaultInstance: SecretsVault | null = null;
function secretsVault(): SecretsVault {
  if (!secretsKeyBase64) {
    throw new SecretsVaultError(503, "secrets vault is not configured on this control plane (RUDDER_SECRETS_KEY missing)");
  }
  if (!secretsVaultInstance) {
    secretsVaultInstance = createSecretsVault(database, secretsKeyBase64);
  }
  return secretsVaultInstance;
}

const insertToken = database.prepare(`
  insert or replace into rudder_tokens (token_hash, account_id, email, created_at, last_used_at)
  values (@tokenHash, @accountId, @email, @createdAt, @lastUsedAt)
`);
const findToken = database.prepare("select * from rudder_tokens where token_hash = ?");
const touchToken = database.prepare("update rudder_tokens set last_used_at = ? where token_hash = ?");
const insertSail = database.prepare(`
  insert into rudder_sails (
    id, account_id, status, runtime, repo_name, task, branch, machine_id, machine_state,
    snapshot_key, manifest_json, worker_token_hash, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @status, @runtime, @repoName, @task, @branch, @machineId, @machineState,
    @snapshotKey, @manifestJson, @workerTokenHash, @lastHeartbeatAt, @createdAt, @updatedAt
  )
`);
// Only ever used to clear a dead sail so its preferred id can be relaunched
// (see createSail); account-scoped so one account cannot clear another's row.
const deleteSailRow = database.prepare(`
  delete from rudder_sails where id = @id and account_id = @accountId
`);

const updateSail = database.prepare(`
  update rudder_sails
  set status = @status,
      machine_id = @machineId,
      machine_state = @machineState,
      failure_reason = case when @status = 'failed' then coalesce(failure_reason, 'Fly machine failed') else null end,
      updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const findSail = database.prepare("select * from rudder_sails where id = ? and account_id = ?");
const findSailById = database.prepare("select * from rudder_sails where id = ?");
const listSailsForAccount = database.prepare(
  "select * from rudder_sails where account_id = ? order by updated_at desc limit 100",
);
const updateHeartbeat = database.prepare(`
  update rudder_sails
  set status = @status,
      machine_id = coalesce(@machineId, machine_id),
      machine_state = @machineState,
      failure_reason = case when @status = 'failed' then coalesce(@failureReason, failure_reason, 'worker reported failure') else null end,
      last_heartbeat_at = @lastHeartbeatAt,
      updated_at = @updatedAt
  where id = @id
    and status <> 'deleting'
`);
const updateWorkerToken = database.prepare(`
  update rudder_sails
  set worker_token_hash = @workerTokenHash,
      updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const updateSailSlackThread = database.prepare(
  "update rudder_sails set slack_thread_ts = @threadTs, updated_at = @updatedAt where id = @id",
);
const findSailBySlackThread = database.prepare("select * from rudder_sails where slack_thread_ts = ?");
const listRunningSails = database.prepare(
  "select * from rudder_sails where status in ('queued','running','paused') order by updated_at desc limit 100",
);
// Newest stored snapshot for a repo, across sails AND workspaces — this is what a
// Slack `launch <repo> <task>` boots from (there is no terminal to upload a fresh
// snapshot, so the last thing pushed up for that repo is the base).
const latestSailSnapshotForRepo = database.prepare(`
  select snapshot_key, account_id, updated_at from rudder_sails
  where repo_name = ? and snapshot_key is not null
  order by updated_at desc limit 1
`);
const latestWorkspaceSnapshotForRepo = database.prepare(`
  select snapshot_key, account_id, updated_at from rudder_workspaces
  where repo_name = ? and snapshot_key is not null
  order by updated_at desc limit 1
`);
const listSailSnapshotRepos = database.prepare(`
  select repo_name, max(updated_at) as updated_at from rudder_sails
  where repo_name is not null and snapshot_key is not null group by repo_name
`);
const listWorkspaceSnapshotRepos = database.prepare(`
  select repo_name, max(updated_at) as updated_at from rudder_workspaces
  where repo_name is not null and snapshot_key is not null group by repo_name
`);
const insertWorkspace = database.prepare(`
  insert into rudder_workspaces (
    id, account_id, workspace_key, repo_name, status, machine_id, machine_state,
    snapshot_key, snapshot_fingerprint, region, volume_id, worker_token_hash,
    source_kind, repo_url, git_ref,
    last_activity_at, last_heartbeat_at, created_at, updated_at
  ) values (
    @id, @accountId, @workspaceKey, @repoName, @status, @machineId, @machineState,
    @snapshotKey, @snapshotFingerprint, @region, @volumeId, @workerTokenHash,
    @sourceKind, @repoUrl, @gitRef,
    @lastActivityAt, @lastHeartbeatAt, @createdAt, @updatedAt
  )
`);
const updateWorkspaceVolume = database.prepare(`
  update rudder_workspaces
  set volume_id = @volumeId,
      region = @region,
      updated_at = @updatedAt
  where id = @id
`);
const findWorkspaceByKey = database.prepare(
  "select * from rudder_workspaces where account_id = ? and workspace_key = ?",
);
const findWorkspaceById = database.prepare("select * from rudder_workspaces where id = ?");
const findWorkspaceForAccount = database.prepare(
  "select * from rudder_workspaces where id = ? and account_id = ?",
);
const listWorkspacesForAccount = database.prepare(
  "select * from rudder_workspaces where account_id = ? order by updated_at desc limit 100",
);
const listAllRunningWorkspaces = database.prepare(
  "select * from rudder_workspaces where status in ('running','paused','queued')",
);
const updateWorkspaceMachine = database.prepare(`
  update rudder_workspaces
  set status = @status,
      machine_id = @machineId,
      machine_state = @machineState,
      failure_reason = case when @status = 'failed' then coalesce(failure_reason, 'Fly machine failed') else null end,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceSnapshot = database.prepare(`
  update rudder_workspaces
  set snapshot_key = @snapshotKey,
      snapshot_fingerprint = @snapshotFingerprint,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceWorkerToken = database.prepare(`
  update rudder_workspaces
  set worker_token_hash = @workerTokenHash,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceActivity = database.prepare(`
  update rudder_workspaces
  set last_activity_at = @lastActivityAt,
      updated_at = @updatedAt
  where id = @id
`);
const updateWorkspaceHeartbeat = database.prepare(`
  update rudder_workspaces
  set status = @status,
      machine_id = coalesce(@machineId, machine_id),
      machine_state = @machineState,
      failure_reason = case when @status = 'failed' then coalesce(@failureReason, failure_reason, 'worker reported failure') else null end,
      active_agents = @activeAgents,
      last_heartbeat_at = @lastHeartbeatAt,
      updated_at = @updatedAt
  where id = @id
    and status not in ('stopped', 'deleting')
    and (@machineId is null or machine_id is null or machine_id = @machineId)
`);
// A create/start that throws used to leave the row "queued" forever with the
// reason only in the HTTP response and the server log. These record it.
const markSailFailed = database.prepare(
  "update rudder_sails set status = 'failed', failure_reason = ?, updated_at = ? where id = ? and status <> 'deleting'",
);
const markWorkspaceFailed = database.prepare(
  "update rudder_workspaces set status = 'failed', failure_reason = ?, updated_at = ? where id = ? and status <> 'deleting'",
);
const getSetting = database.prepare("select value from rudder_settings where key = ?");
const upsertSetting = database.prepare(`
  insert into rudder_settings (key, value, updated_at)
  values (@key, @value, @updatedAt)
  on conflict(key) do update set value = excluded.value, updated_at = excluded.updated_at
`);

let authProviderFingerprint = providerFingerprint();
let auth: ReturnType<typeof createBetterAuth> = createBetterAuth();
let authHandler = toNodeHandler(auth.handler);

// noDelay: keystroke/echo frames relayed between attach clients and workers
// are a few bytes each; they must never sit in the kernel buffer waiting for
// piggyback ACKs. Node >=18 defaults accepted sockets to noDelay, but make it
// explicit so the relay's latency floor doesn't hinge on a runtime default.
const server = http.createServer({ noDelay: true }, async (req, res) => {
  res.once("finish", () => {
    schedulePersistDatabase();
  });
  try {
    const url = new URL(req.url || "/", baseURL);
    if (url.pathname.startsWith("/api/auth")) {
      refreshAuthHandler();
      authHandler(req, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/health") {
      sendJson(res, 200, {
        ok: true,
        s3: Boolean(snapshotBucket),
        fly: Boolean(flyApiToken && flyAppName && flyWorkerImage),
        byoVm: Boolean(snapshotBucket && flyWorkerImage),
        state: Boolean(snapshotBucket && persistStateToS3),
        secrets: Boolean(secretsKeyBase64),
        auth: configuredProviders(),
      });
      return;
    }
    if (req.method === "GET" && url.pathname === "/setup/github") {
      // Provider-credential setup is admin-only: an unauthenticated visitor must not
      // be able to install (or, via the callback, overwrite) the OAuth client creds.
      await requireAdminRequest(req);
      renderGithubAppSetup(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/setup/github/callback") {
      await requireAdminRequest(req);
      await handleGithubAppSetupCallback(url, res);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/rudder/setup/github") {
      await handleOAuthCredentialSetup(req, res, "github");
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/rudder/setup/google") {
      await handleOAuthCredentialSetup(req, res, "google");
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/cli/login") {
      await handleCliLoginStart(res);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/cli/login/github-token") {
      await handleCliGithubToken(req, res);
      return;
    }
    if (url.pathname === "/api/cli/login/poll") {
      await handleCliLoginPoll(req, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/login") {
      renderLoginPage(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/oauth/google/start") {
      await handleCliOAuthStart(url, res, "google");
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/oauth/github/start") {
      await handleCliOAuthStart(url, res, "github");
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/github/start") {
      await handleCliGithubStart(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/github/wait") {
      await handleCliGithubWait(url, res);
      return;
    }
    if (req.method === "GET" && url.pathname === "/cli/approve") {
      await handleCliApprove(req, res, url);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/admin/workspace/gc") {
      await handleAdminWorkspaceGc(req, res, url);
      return;
    }
    if (req.method === "POST" && url.pathname === "/api/slack/events") {
      await handleSlackEvents(req, res);
      return;
    }
    if (url.pathname.startsWith("/api/rudder/secrets")) {
      await handleSecretsApi(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/rudder/sail")) {
      await handleSailApi(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/rudder/workspace")) {
      await handleWorkspaceApi(req, res, url);
      return;
    }
    if (url.pathname === "/app/login") {
      await handleAppLogin(url, res);
      return;
    }
    if (url.pathname === "/app/native-done") {
      await handleAppNativeDone(req, res);
      return;
    }
    if (req.method === "GET" && (url.pathname === "/app" || url.pathname.startsWith("/app/"))) {
      await serveAppFile(res, url.pathname);
      return;
    }
    if (url.pathname === "/api/app/pair/start" || url.pathname === "/api/app/pair/finish") {
      await handleAppPairing(req, res, url);
      return;
    }
    if (url.pathname.startsWith("/api/app/")) {
      await handleAppApi(req, res, url);
      return;
    }
    if (url.pathname === "/api/rudder/home" || url.pathname.startsWith("/api/rudder/home/")) {
      await handleHomeApi(req, res, url);
      return;
    }
    renderHome(res);
  } catch (error) {
    const status = error && typeof error === "object" && "status" in error && typeof error.status === "number"
      ? error.status
      : 500;
    console.error(`request error ${req.method} ${req.url} -> ${status}`, error);
    sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
  }
});

const SAIL_ATTACH_PATH_RE = /^\/api\/rudder\/sail\/([^/]+)\/(worker|attach)$/;
const WORKSPACE_ATTACH_PATH_RE = /^\/api\/rudder\/workspace\/([^/]+)\/(worker|attach)$/;
const HOME_CONTROL_PATH_RE = /^\/api\/rudder\/home\/([^/]+)\/control$/;
const REPLAY_BUFFER_BYTES = 256 * 1024;
const PENDING_INPUT_BYTES = 64 * 1024;
const MAX_SOCKET_BUFFER_BYTES = 1024 * 1024;
// A dead client socket (a laptop that slept) is noticed within two of these.
const SOCKET_PING_MS = 15_000;

type Channel = {
  worker?: WebSocket;
  clients: Set<WebSocket>;
  controller?: WebSocket;
  buffer: Buffer[];
  bufferBytes: number;
  pendingInput: Buffer[];
  pendingInputBytes: number;
  latestResize?: string;
};

type ChannelKind = "sail" | "workspace";

const sailChannels = new Map<string, Channel>();
const workspaceChannels = new Map<string, Channel>();

const wss = new WebSocketServer({
  noServer: true,
  // Compress large terminal redraws (a single Claude frame can be many kB of
  // ANSI). Skip small frames so per-keystroke echoes don't pay the deflate
  // tax.
  perMessageDeflate: {
    threshold: 1024,
    zlibDeflateOptions: { level: 1 },
  },
});

server.on("upgrade", (req: IncomingMessage, socket: Duplex, head: Buffer) => {
  // Both the attach client and the worker dial into this relay, so both relay
  // hops are accepted sockets that pass through here. Disable Nagle on each so
  // single-keystroke frames forward immediately in both directions.
  if (socket instanceof net.Socket) {
    try { socket.setNoDelay(true); } catch { /* ignore */ }
  }
  try {
    const url = new URL(req.url || "/", baseURL);
    const sailMatch = SAIL_ATTACH_PATH_RE.exec(url.pathname);
    if (sailMatch) {
      handleSailUpgrade(req, socket, head, sailMatch[1], sailMatch[2] as "worker" | "attach");
      return;
    }
    const homeControlMatch = HOME_CONTROL_PATH_RE.exec(url.pathname);
    if (homeControlMatch) {
      handleHomeControlUpgrade(req, socket, head, homeControlMatch[1]);
      return;
    }
    const workspaceMatch = WORKSPACE_ATTACH_PATH_RE.exec(url.pathname);
    if (workspaceMatch) {
      handleWorkspaceUpgrade(req, socket, head, workspaceMatch[1], workspaceMatch[2] as "worker" | "attach");
      return;
    }
    destroyUpgrade(socket, 404);
  } catch {
    destroyUpgrade(socket, 500);
  }
});

function handleSailUpgrade(
  req: IncomingMessage,
  socket: Duplex,
  head: Buffer,
  sailId: string,
  role: "worker" | "attach",
): void {
  const sailRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!sailRow) {
    destroyUpgrade(socket, 404);
    return;
  }
  if (String(sailRow.status) === "deleting") {
    destroyUpgrade(socket, 410);
    return;
  }
  if (role === "worker") {
    try {
      requireWorkerBearer(req, sailRow);
    } catch {
      destroyUpgrade(socket, 401);
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => attachWorker("sail", sailId, ws));
    return;
  }
  let auth;
  try {
    auth = requireBearer(req);
  } catch {
    destroyUpgrade(socket, 401);
    return;
  }
  if (String(sailRow.account_id) !== auth.accountId) {
    destroyUpgrade(socket, 403);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachClient("sail", sailId, ws, attachClientId(req)));
}

function handleWorkspaceUpgrade(
  req: IncomingMessage,
  socket: Duplex,
  head: Buffer,
  workspaceId: string,
  role: "worker" | "attach",
): void {
  const workspaceRow = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!workspaceRow) {
    destroyUpgrade(socket, 404);
    return;
  }
  if (String(workspaceRow.status) === "deleting") {
    destroyUpgrade(socket, 410);
    return;
  }
  if (role === "worker") {
    try {
      requireWorkerBearer(req, workspaceRow);
    } catch {
      destroyUpgrade(socket, 401);
      return;
    }
    const machineId = typeof req.headers["x-rudder-machine-id"] === "string"
      ? req.headers["x-rudder-machine-id"]
      : undefined;
    const expectedMachineId = optionalString(workspaceRow.machine_id);
    if (machineId && expectedMachineId && machineId !== expectedMachineId) {
      destroyUpgrade(socket, 409);
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => attachWorker("workspace", workspaceId, ws));
    return;
  }
  let auth;
  try {
    auth = requireBearer(req);
  } catch {
    destroyUpgrade(socket, 401);
    return;
  }
  if (String(workspaceRow.account_id) !== auth.accountId) {
    destroyUpgrade(socket, 403);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachClient("workspace", workspaceId, ws, attachClientId(req)));
}

function destroyUpgrade(socket: Duplex, status: number): void {
  const reason = status === 401 ? "Unauthorized"
    : status === 403 ? "Forbidden"
    : status === 404 ? "Not Found"
    : status === 409 ? "Conflict"
    : "Internal Server Error";
  try {
    socket.write(`HTTP/1.1 ${status} ${reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
  } catch {
    // ignore
  }
  socket.destroy();
}

function channelMap(kind: ChannelKind): Map<string, Channel> {
  return kind === "workspace" ? workspaceChannels : sailChannels;
}

function getChannel(kind: ChannelKind, id: string): Channel {
  const map = channelMap(kind);
  let channel = map.get(id);
  if (!channel) {
    channel = {
      clients: new Set(),
      buffer: [],
      bufferBytes: 0,
      pendingInput: [],
      pendingInputBytes: 0,
    };
    map.set(id, channel);
  }
  return channel;
}

function disposeChannelIfEmpty(kind: ChannelKind, id: string): void {
  const map = channelMap(kind);
  const channel = map.get(id);
  if (!channel) {
    return;
  }
  if (!channel.worker && channel.clients.size === 0) {
    map.delete(id);
  }
}

function attachWorker(kind: ChannelKind, id: string, ws: WebSocket): void {
  const channel = getChannel(kind, id);
  if (channel.worker && channel.worker.readyState === WebSocket.OPEN) {
    try {
      channel.worker.close(4000, "replaced");
    } catch {
      // ignore
    }
  }
  channel.worker = ws;
  channel.buffer = [];
  channel.bufferBytes = 0;
  enableSocketLiveness(ws);
  flushPendingClientState(channel);
  broadcastStatus(channel, "worker-connected");
  // Deliver any Slack messages that were queued while this sail was asleep.
  if (kind === "sail") {
    flushPendingSlackInputs(id);
  }

  ws.on("message", (data, isBinary) => {
    if (channel.worker !== ws) {
      return;
    }
    if (isBinary && data instanceof Buffer) {
      pushBuffer(channel, data);
      for (const client of channel.clients) {
        sendSocket(client, data, true);
      }
      return;
    }
    if (data instanceof Buffer) {
      const text = data.toString("utf8");
      forwardTextToClients(channel, text);
    }
  });

  ws.on("close", () => {
    if (channel.worker !== ws) {
      return;
    }
    channel.worker = undefined;
    broadcastStatus(channel, "worker-disconnected");
    disposeChannelIfEmpty(kind, id);
  });
  ws.on("error", () => undefined);
}

// Which terminal a client socket is: `rudder cloud` sends one id for its whole
// run, reconnects included.
const clientIds = new WeakMap<WebSocket, string>();

function attachClientId(req: IncomingMessage): string | undefined {
  try {
    const value = new URL(req.url || "/", baseURL).searchParams.get("client") ?? "";
    return /^[A-Za-z0-9_-]{8,64}$/.test(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function attachClient(kind: ChannelKind, id: string, ws: WebSocket, clientId?: string): void {
  const channel = getChannel(kind, id);
  if (clientId) {
    // The same terminal coming back (its laptop slept, the Wi-Fi changed):
    // the socket it left behind is dead even if this relay has not noticed
    // yet. Take its place, control and its buffered input included, instead
    // of watching it read-only until a ping times out.
    for (const previous of channel.clients) {
      if (clientIds.get(previous) !== clientId) {
        continue;
      }
      channel.clients.delete(previous);
      if (channel.controller === previous) {
        channel.controller = undefined;
      }
      try { previous.terminate(); } catch { /* ignore */ }
    }
    clientIds.set(ws, clientId);
  }
  channel.clients.add(ws);
  // One terminal owns input + PTY geometry at a time. A second observer must
  // not steal control merely by connecting: doing so resized the shared PTY to
  // the observer's dimensions and made the original terminal appear frozen.
  // Control moves only when the current controller disconnects.
  const becomesController = !isOpen(channel.controller);
  if (becomesController) {
    channel.controller = ws;
  }
  enableSocketLiveness(ws);
  if (kind === "workspace") {
    touchWorkspaceActivity(id);
  }

  sendSocket(ws, JSON.stringify({
    type: "status",
    state: isOpen(channel.worker) ? "worker-connected" : "worker-waiting",
    control: becomesController ? "active" : "read-only",
  }));
  if (channel.bufferBytes > 0) {
    sendSocket(ws, Buffer.concat(channel.buffer, channel.bufferBytes), true);
  }

  ws.on("message", (data, isBinary) => {
    if (channel.controller !== ws) {
      sendSocket(ws, JSON.stringify({ type: "status", state: "read-only" }));
      return;
    }
    const worker = channel.worker;
    if (isBinary && data instanceof Buffer) {
      if (isOpen(worker)) {
        sendSocket(worker, data, true);
      } else if (containsNonReplayableInterrupt(data)) {
        // Ctrl+C is an instruction for the LOCAL attach to disconnect while
        // the worker is absent. Replaying it into the next worker boot kills
        // the fresh dashboard immediately. Drop the whole coalesced terminal
        // chunk: bytes typed alongside a disconnect request are not safe to
        // replay into a later process either.
        sendSocket(ws, JSON.stringify({ type: "status", state: "interrupt-dropped" }));
      } else {
        pushPendingInput(channel, data);
        sendSocket(ws, JSON.stringify({ type: "status", state: "input-buffered" }));
      }
      if (kind === "workspace") {
        touchWorkspaceActivity(id);
      }
      return;
    }
    if (data instanceof Buffer) {
      const text = data.toString("utf8");
      if (isResizeMessage(text)) {
        channel.latestResize = text;
      }
      if (isOpen(worker)) {
        sendSocket(worker, text);
      }
    }
  });

  ws.on("close", () => {
    channel.clients.delete(ws);
    if (channel.controller === ws) {
      // Buffered input and geometry belong to the controller that produced
      // them. Never hand those bytes to a different terminal after promotion.
      channel.pendingInput = [];
      channel.pendingInputBytes = 0;
      channel.latestResize = undefined;
      channel.controller = Array.from(channel.clients).reverse().find(isOpen);
      if (channel.controller) {
        sendSocket(channel.controller, JSON.stringify({
          type: "status",
          state: "controller-promoted",
          control: "active",
        }));
      }
    }
    disposeChannelIfEmpty(kind, id);
  });
  ws.on("error", () => undefined);
}

function containsNonReplayableInterrupt(data: Buffer): boolean {
  return data.includes(0x03) || containsKittyCtrlC(data);
}

function containsKittyCtrlC(data: Buffer): boolean {
  // With the disambiguate-escape-codes flag enabled, Kitty reports Ctrl+C as
  // CSI 99;5u. Accept the optional event-type suffix used when a terminal has
  // additional reporting flags enabled as well.
  return /\x1b\[99;5(?::[123])?u/.test(data.toString("latin1"));
}

function pushBuffer(channel: Channel, chunk: Buffer): void {
  if (chunk.length >= REPLAY_BUFFER_BYTES) {
    channel.buffer = [chunk.subarray(chunk.length - REPLAY_BUFFER_BYTES)];
    channel.bufferBytes = REPLAY_BUFFER_BYTES;
    return;
  }
  channel.buffer.push(chunk);
  channel.bufferBytes += chunk.length;
  while (channel.bufferBytes > REPLAY_BUFFER_BYTES && channel.buffer.length > 0) {
    const dropped = channel.buffer.shift();
    if (dropped) {
      channel.bufferBytes -= dropped.length;
    }
  }
}

function pushPendingInput(channel: Channel, chunk: Buffer): void {
  const remaining = PENDING_INPUT_BYTES - channel.pendingInputBytes;
  if (remaining <= 0) {
    return;
  }
  const accepted = chunk.length <= remaining ? chunk : chunk.subarray(0, remaining);
  channel.pendingInput.push(Buffer.from(accepted));
  channel.pendingInputBytes += accepted.length;
}

function flushPendingClientState(channel: Channel): void {
  const worker = channel.worker;
  if (!isOpen(worker)) {
    return;
  }
  if (channel.latestResize) {
    sendSocket(worker, channel.latestResize);
  }
  for (const chunk of channel.pendingInput) {
    sendSocket(worker, chunk, true);
  }
  channel.pendingInput = [];
  channel.pendingInputBytes = 0;
}

function isResizeMessage(text: string): boolean {
  try {
    const value = JSON.parse(text) as { type?: unknown };
    return value?.type === "resize";
  } catch {
    return false;
  }
}

function isOpen(ws: WebSocket | undefined): ws is WebSocket {
  return Boolean(ws && ws.readyState === WebSocket.OPEN);
}

function sendSocket(ws: WebSocket | undefined, data: string | Buffer, binary = false): boolean {
  if (!isOpen(ws)) {
    return false;
  }
  if (ws.bufferedAmount > MAX_SOCKET_BUFFER_BYTES) {
    ws.terminate();
    return false;
  }
  if (binary) {
    ws.send(data, { binary: true });
  } else {
    ws.send(data);
  }
  return true;
}

function enableSocketLiveness(ws: WebSocket): void {
  let alive = true;
  ws.on("pong", () => { alive = true; });
  const timer = setInterval(() => {
    if (!alive || ws.readyState !== WebSocket.OPEN) {
      clearInterval(timer);
      if (ws.readyState !== WebSocket.CLOSED) {
        ws.terminate();
      }
      return;
    }
    alive = false;
    try { ws.ping(); } catch { ws.terminate(); }
  }, SOCKET_PING_MS);
  timer.unref?.();
  ws.once("close", () => clearInterval(timer));
}

function forwardTextToClients(channel: Channel, text: string): void {
  for (const client of channel.clients) {
    sendSocket(client, text);
  }
}

function broadcastStatus(channel: Channel, state: string): void {
  const payload = JSON.stringify({ type: "status", state });
  for (const client of channel.clients) {
    sendSocket(client, payload);
  }
}

// Inject text into a live worker's PTY exactly as a `cloud attach` client would.
// The worker supervisor treats binary frames as raw keystrokes, so we send the
// UTF-8 bytes (plus a carriage return to submit) on the worker socket. Returns
// false when no worker is currently connected for that instance.
function sendInputToChannel(kind: ChannelKind, id: string, text: string, submit = true): boolean {
  const channel = channelMap(kind).get(id);
  const worker = channel?.worker;
  if (!channel || !worker || worker.readyState !== WebSocket.OPEN) {
    return false;
  }
  const payload = submit ? `${text}\r` : text;
  worker.send(Buffer.from(payload, "utf8"), { binary: true });
  if (kind === "workspace") {
    touchWorkspaceActivity(id);
  }
  return true;
}

// Read the most recent raw output (with ANSI) buffered for an instance. The
// replay buffer only exists while the worker is connected, so this returns ""
// for paused/dead instances.
function readChannelOutput(kind: ChannelKind, id: string): string {
  const channel = channelMap(kind).get(id);
  if (!channel || channel.buffer.length === 0) {
    return "";
  }
  return Buffer.concat(channel.buffer).toString("utf8");
}

function instanceHasLiveWorker(kind: ChannelKind, id: string): boolean {
  const worker = channelMap(kind).get(id)?.worker;
  return Boolean(worker && worker.readyState === WebSocket.OPEN);
}

function touchWorkspaceActivity(workspaceId: string): void {
  const previous = workspaceActivityWrites.get(workspaceId) ?? 0;
  const nowMs = Date.now();
  if (nowMs - previous < 1_000) {
    return;
  }
  workspaceActivityWrites.set(workspaceId, nowMs);
  try {
    const now = new Date().toISOString();
    updateWorkspaceActivity.run({ id: workspaceId, lastActivityAt: now, updatedAt: now });
    // Typing into a repo on the home machine is activity on the home machine:
    // that is what its idle sweep looks at.
    const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
    const homeId = row ? optionalString(row.home_id) : undefined;
    if (homeId && homeId !== workspaceId) {
      updateWorkspaceActivity.run({ id: homeId, lastActivityAt: now, updatedAt: now });
    }
  } catch {
    // ignore
  }
}

const workspaceActivityWrites = new Map<string, number>();

server.listen(port, () => {
  console.log(`rudder cloud listening on ${baseURL}`);
});

for (const signal of ["SIGINT", "SIGTERM"] as const) {
  process.once(signal, () => {
    void persistDatabaseToS3().finally(() => {
      process.exit(signal === "SIGINT" ? 130 : 143);
    });
  });
}

const workspaceIdleMs = Number(process.env.RUDDER_WORKSPACE_IDLE_MS || 30 * 60 * 1000);
const workspaceSweepIntervalMs = Number(process.env.RUDDER_WORKSPACE_SWEEP_MS || 60 * 1000);
if (workspaceIdleMs >= 60 * 1000 && workspaceSweepIntervalMs >= 10 * 1000) {
  const timer = setInterval(() => {
    void sweepIdleWorkspaces().catch(() => undefined);
  }, workspaceSweepIntervalMs);
  timer.unref?.();
}

// ---- Deletion ------------------------------------------------------------
//
// Deleting is a REQUEST that a reaper finishes. The row flips to `deleting`
// at once (so heartbeats, attaches and stop/pause can no longer touch it, and
// a workspace's key is freed for a fresh attach), then the teardown runs:
// machine, volume, snapshot object, row. Any step that fails (Fly busy, the
// volume still detaching, a network blip) leaves the row `deleting` with the
// reason in `delete_error`, and sweepDeletions retries it until it is gone,
// including across a control-plane restart.
const markSailDeleting = database.prepare(`
  update rudder_sails set status = 'deleting', delete_error = null, updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const markWorkspaceDeleting = database.prepare(`
  update rudder_workspaces
  set status = 'deleting', workspace_key = @tombstoneKey, delete_error = null, updated_at = @updatedAt
  where id = @id and account_id = @accountId
`);
const listDeletingSails = database.prepare("select * from rudder_sails where status = 'deleting'");
const listDeletingWorkspaces = database.prepare("select * from rudder_workspaces where status = 'deleting'");
const removeSailRow = database.prepare("delete from rudder_sails where id = ? and status = 'deleting'");
const removeWorkspaceRow = database.prepare("delete from rudder_workspaces where id = ? and status = 'deleting'");
const recordSailDeleteError = database.prepare("update rudder_sails set delete_error = ?, updated_at = ? where id = ?");
const recordWorkspaceDeleteError = database.prepare(
  "update rudder_workspaces set delete_error = ?, updated_at = ? where id = ?",
);
const countSnapshotUsers = database.prepare(`
  select (select count(*) from rudder_sails where snapshot_key = @key and id <> @id and status <> 'deleting')
       + (select count(*) from rudder_workspaces where snapshot_key = @key and id <> @id and status <> 'deleting')
    as users
`);
const deleteInFlight = new Set<string>();

type DeletionKind = "sail" | "workspace";

async function requestDeletion(
  kind: DeletionKind,
  id: string,
  accountId: string,
): Promise<{ id: string; kind: DeletionKind; status: "deleted" | "deleting"; error?: string }> {
  const now = new Date().toISOString();
  if (kind === "sail") {
    markSailDeleting.run({ id, accountId, updatedAt: now });
  } else {
    markWorkspaceDeleting.run({ id, accountId, tombstoneKey: `deleted:${id}`, updatedAt: now });
  }
  closeChannel(kind, id);
  if (kind === "workspace") {
    const row = findWorkspaceById.get(id) as Record<string, unknown> | undefined;
    const homeId = row ? optionalString(row.home_id) : undefined;
    if (row && optionalString(row.source_kind) === "home") {
      // Deleting the home machine deletes every repo on it.
      for (const repo of homeRepos(id)) {
        markWorkspaceDeleting.run({ id: repo.id, accountId, tombstoneKey: `deleted:${repo.id}`, updatedAt: now });
        closeChannel("workspace", repo.id);
      }
      homeStates.delete(id);
    } else if (homeId) {
      void homeRpc(homeId, "close", { workspaceId: id, removeClone: true }).catch(() => null);
      pushHomeRepos(homeId);
    }
  }
  schedulePersistDatabase();
  // Try to finish now so the caller can say "deleted"; whatever is left is the
  // reaper's. Bounded so a slow Fly never holds the request open.
  const outcome = await Promise.race([
    reapDeletion(kind, id).then((error) => error),
    sleep(20_000).then(() => "still tearing down; the control plane will finish it"),
  ]);
  return outcome
    ? { id, kind, status: "deleting", error: outcome }
    : { id, kind, status: "deleted" };
}

function closeChannel(kind: DeletionKind, id: string): void {
  const channels = kind === "sail" ? sailChannels : workspaceChannels;
  const channel = channels.get(id);
  if (!channel) {
    return;
  }
  for (const ws of [channel.worker, ...channel.clients]) {
    try {
      ws?.close(4410, `${kind} deleted`);
    } catch {
      // already closed
    }
  }
  channels.delete(id);
}

/** Tear one `deleting` row down. Returns the error that stopped it, if any. */
async function reapDeletion(kind: DeletionKind, id: string): Promise<string | undefined> {
  const key = `${kind}:${id}`;
  if (deleteInFlight.has(key)) {
    return "deletion already in progress";
  }
  deleteInFlight.add(key);
  try {
    const row = (kind === "sail" ? findSailById.get(id) : findWorkspaceById.get(id)) as
      | Record<string, unknown>
      | undefined;
    if (!row || String(row.status) !== "deleting") {
      return undefined;
    }
    const machineId = optionalString(row.machine_id);
    const runtime = optionalString(row.runtime) ?? "fly";
    // A BYO VM sail's machine is the user's own: there is nothing of ours to
    // destroy, and its worker is refused from here on (heartbeat 410).
    const ownsMachine = kind === "workspace" || runtime === "fly";
    if (ownsMachine && machineId) {
      if (!flyApiToken || !flyAppName) {
        throw new Error("Fly is not configured; cannot destroy the machine");
      }
      await destroyFlyMachine(machineId);
    }
    const volumeId = kind === "workspace" ? optionalString(row.volume_id) : undefined;
    if (volumeId) {
      await destroyWorkspaceVolume(volumeId);
    }
    // The snapshot carries the repo and the auth that was staged with it, so
    // it goes too -- unless another live row still boots from the same object.
    const snapshotKey = optionalString(row.snapshot_key);
    if (snapshotKey && snapshotBucket) {
      const users = (countSnapshotUsers.get({ key: snapshotKey, id }) as { users: number }).users;
      if (!users) {
        await s3.send(new DeleteObjectCommand({ Bucket: snapshotBucket, Key: snapshotKey }));
      }
    }
    if (kind === "sail") {
      removeSailRow.run(id);
    } else {
      removeWorkspaceRow.run(id);
    }
    schedulePersistDatabase();
    console.log(`deleted ${kind} ${id}`);
    return undefined;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const now = new Date().toISOString();
    if (kind === "sail") {
      recordSailDeleteError.run(message, now, id);
    } else {
      recordWorkspaceDeleteError.run(message, now, id);
    }
    schedulePersistDatabase();
    console.warn(`delete ${kind} ${id} not finished, will retry: ${message}`);
    return message;
  } finally {
    deleteInFlight.delete(key);
  }
}

async function sweepDeletions(): Promise<void> {
  for (const row of listDeletingSails.all() as Record<string, unknown>[]) {
    await reapDeletion("sail", String(row.id));
  }
  for (const row of listDeletingWorkspaces.all() as Record<string, unknown>[]) {
    await reapDeletion("workspace", String(row.id));
  }
}

const deleteSweepIntervalMs = Number(process.env.RUDDER_DELETE_SWEEP_MS || 30 * 1000);
if (deleteSweepIntervalMs >= 1000) {
  const timer = setInterval(() => {
    void sweepDeletions().catch(() => undefined);
  }, deleteSweepIntervalMs);
  timer.unref?.();
  // Rows left `deleting` by a previous process are finished on boot.
  setTimeout(() => void sweepDeletions().catch(() => undefined), 1000).unref?.();
}

// How stale the busy signal may be and still count: the supervisor heartbeats
// every 30s, so a machine that is alive and busy refreshes it constantly. A
// frozen active_agents > 0 with NO recent heartbeat means the supervisor died
// (or the machine is wedged) — that must NOT keep a dead machine un-stopped
// forever, so the busy exemption requires a fresh heartbeat.
const busyHeartbeatFreshMs = 5 * 60 * 1000;

async function sweepIdleWorkspaces(): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    return;
  }
  const rows = listAllRunningWorkspaces.all() as Record<string, unknown>[];
  const now = Date.now();
  for (const row of rows) {
    const workspace = rowToWorkspace(row);
    if (!workspace.machineId) {
      continue;
    }
    if (Array.from(workspaceChannels.get(workspace.id)?.clients ?? []).some(isOpen)) {
      continue;
    }
    const isHome = workspace.sourceKind === "home";
    // The home machine is idle only when nobody is attached to ANY of its repos.
    if (isHome && homeRepos(workspace.id).some((repo) => Array.from(workspaceChannels.get(repo.id)?.clients ?? []).some(isOpen))) {
      continue;
    }
    // BUSY EXEMPTION: agents are actively working in this machine (reported by
    // the worker, not guessed from user activity). Stopping it would kill live
    // cloud sessions the moment the user's laptop disconnected for longer than
    // the idle window — the exact opposite of what the workspace is for.
    if ((workspace.activeAgents ?? 0) > 0) {
      const heartbeatMs = Date.parse(workspace.lastHeartbeatAt ?? "");
      if (Number.isFinite(heartbeatMs) && now - heartbeatMs <= busyHeartbeatFreshMs) {
        continue;
      }
    }
    const last = workspace.lastActivityAt ?? workspace.lastHeartbeatAt ?? workspace.updatedAt;
    const lastMs = Date.parse(last);
    if (!Number.isFinite(lastMs) || now - lastMs < (isHome ? homeIdleMs : workspaceIdleMs)) {
      continue;
    }
    try {
      await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/stop`,
        { method: "POST", body: { signal: "SIGTERM", timeout: "10s" } },
      );
      updateWorkspaceMachine.run({
        id: workspace.id,
        status: "stopped",
        machineId: workspace.machineId,
        machineState: "stopping",
        updatedAt: new Date().toISOString(),
      });
      if (isHome) {
        setHomeRepoStatus.run({ homeId: workspace.id, status: "stopped", updatedAt: new Date().toISOString() });
      }
    } catch {
      // ignore; will retry next sweep
    }
  }
}

let persistTimer: NodeJS.Timeout | undefined;
let persistInFlight = false;
let persistAgain = false;

async function restoreDatabaseFromS3(): Promise<void> {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  try {
    const response = await s3.send(new GetObjectCommand({
      Bucket: snapshotBucket,
      Key: stateKey,
    }));
    if (!response.Body) {
      return;
    }
    const buffer = await streamToBuffer(response.Body);
    if (buffer.length === 0) {
      return;
    }
    await fs.writeFile(dbPath, buffer, { mode: 0o600 });
  } catch (error) {
    const name = error && typeof error === "object" && "name" in error ? String(error.name) : "";
    if (name !== "NoSuchKey" && name !== "NotFound") {
      console.warn(`rudder cloud state restore skipped: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
}

function schedulePersistDatabase(): void {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  if (persistTimer) {
    clearTimeout(persistTimer);
  }
  persistTimer = setTimeout(() => {
    persistTimer = undefined;
    void persistDatabaseToS3();
  }, 750);
  persistTimer.unref?.();
}

async function persistDatabaseToS3(): Promise<void> {
  if (!snapshotBucket || !persistStateToS3) {
    return;
  }
  if (persistInFlight) {
    persistAgain = true;
    return;
  }
  persistInFlight = true;
  try {
    database.pragma("wal_checkpoint(FULL)");
    const body = await fs.readFile(dbPath);
    await s3.send(new PutObjectCommand({
      Bucket: snapshotBucket,
      Key: stateKey,
      Body: body,
      ContentType: "application/vnd.sqlite3",
      ServerSideEncryption: "AES256",
    }));
  } catch (error) {
    console.warn(`rudder cloud state persist failed: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    persistInFlight = false;
    if (persistAgain) {
      persistAgain = false;
      schedulePersistDatabase();
    }
  }
}

async function streamToBuffer(body: unknown): Promise<Buffer> {
  if (Buffer.isBuffer(body)) {
    return body;
  }
  if (body instanceof Uint8Array) {
    return Buffer.from(body);
  }
  if (body instanceof Readable || (body && typeof body === "object" && Symbol.asyncIterator in body)) {
    const chunks: Buffer[] = [];
    for await (const chunk of body as AsyncIterable<Buffer | Uint8Array | string>) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    }
    return Buffer.concat(chunks);
  }
  if (body && typeof body === "object" && "transformToByteArray" in body) {
    const bytes = await (body as { transformToByteArray(): Promise<Uint8Array> }).transformToByteArray();
    return Buffer.from(bytes);
  }
  throw new Error("unsupported S3 body type");
}

async function handleCliLoginStart(res: ServerResponse): Promise<void> {
  const deviceCode = randomUUID();
  const expiresAt = Date.now() + 5 * 60 * 1000;
  deviceLogins.set(deviceCode, { deviceCode, expiresAt });
  const loginBase = publicLoginUrl || `${baseURL}/cli/login`;
  const separator = loginBase.includes("?") ? "&" : "?";
  sendJson(res, 200, {
    deviceCode,
    loginUrl: `${loginBase}${separator}device_code=${encodeURIComponent(deviceCode)}`,
    pollUrl: "/api/cli/login/poll",
    interval: 2,
    expiresIn: 300,
  });
}

async function handleCliLoginPoll(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const body = req.method === "POST" ? await readJsonBody(req) : {};
  const url = new URL(req.url || "/", baseURL);
  const deviceCode = stringField(body, "deviceCode") || url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    sendJson(res, 404, { error: "login expired" });
    return;
  }
  if (!login.token) {
    sendJson(res, 200, { pending: true });
    return;
  }
  deviceLogins.delete(deviceCode);
  const responseBody: JsonRecord = {
    pending: false,
    token: login.token,
  };
  if (login.accountId) {
    responseBody.accountId = login.accountId;
  }
  if (login.email) {
    responseBody.email = login.email;
  }
  sendJson(res, 200, responseBody);
}

async function handleCliGithubToken(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const body = await readJsonBody(req);
  const githubToken = stringField(body, "token");
  if (!githubToken) {
    throw badRequest("token is required");
  }
  const user = await githubUser(githubToken);
  const issued = issueRudderToken(`github:${user.id}`, user.email ?? `${user.login}@users.noreply.github.com`);
  const responseBody: JsonRecord = {
    token: issued.token,
    accountId: issued.accountId,
    provider: "github",
  };
  if (issued.email) {
    responseBody.email = issued.email;
  }
  sendJson(res, 200, responseBody);
}

function issueRudderToken(accountId: string, email?: string): { token: string; accountId: string; email?: string } {
  const rudderToken = `rdr_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  insertToken.run({
    tokenHash: tokenHash(rudderToken),
    accountId,
    email: email ?? null,
    createdAt: now,
    lastUsedAt: now,
  });
  return { token: rudderToken, accountId, email };
}

function renderLoginPage(url: URL, res: ServerResponse): void {
  const deviceCode = url.searchParams.get("device_code") || "";
  const callbackURL = `/cli/approve?device_code=${encodeURIComponent(deviceCode)}`;
  const providers = configuredProviders();
  const buttons: string[] = [];
  if (providers.google) {
    buttons.push(providerButton(
      `/cli/oauth/google/start?device_code=${encodeURIComponent(deviceCode)}`,
      "Continue with Google",
      GOOGLE_ICON_SVG,
    ));
  }
  if (providers.github) {
    buttons.push(providerButton(
      `/cli/oauth/github/start?device_code=${encodeURIComponent(deviceCode)}`,
      "Continue with GitHub",
      GITHUB_ICON_SVG,
    ));
  }
  const cliBlock = deviceCode
    ? `<div class="device">
        Don't want to use a provider above? You can also
        <a href="/cli/github/start?device_code=${escapeHtml(deviceCode)}">sign in with a GitHub device code</a>.
      </div>`
    : `<div class="device">
        Run <code>rudder login</code> from the CLI to start a login session.
      </div>`;
  const noProviders = buttons.length === 0
    ? `<p class="empty">No OAuth providers are configured yet.${deviceCode ? " Use the GitHub device code option below." : ""}</p>`
    : "";
  const body = `
    <section class="hero">
      <h1>Sign in.</h1>
      <p class="lede">Connect this browser to Rudder Cloud so the CLI can launch and watch over cloud workers.</p>
      <div class="card">
        ${noProviders}
        ${buttons.join("\n")}
        ${cliBlock}
      </div>
    </section>
  `;
  sendHtml(res, renderShell({ title: "Sign in · Rudder Cloud", body }));
}

function providerButton(href: string, label: string, icon: string): string {
  return `<a class="provider" href="${escapeHtml(href)}"><span class="icon" aria-hidden="true">${icon}</span><span>${escapeHtml(label)}</span></a>`;
}

async function handleCliOAuthStart(url: URL, res: ServerResponse, provider: "google" | "github"): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }

  const providers = configuredProviders();
  if (!providers[provider]) {
    sendHtml(res, renderShell({
      title: "Provider unavailable · Rudder Cloud",
      body: `<section class="hero"><h1>Provider unavailable.</h1><p class="lede">${escapeHtml(provider)} login is not configured. Use the GitHub device code option instead.</p></section>`,
    }), 404);
    return;
  }

  const callbackURL = `${baseURL}/cli/approve?device_code=${encodeURIComponent(deviceCode)}`;
  refreshAuthHandler();
  const response = await auth.handler(new Request(`${authBaseURL}/sign-in/social`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      origin: baseURL,
    },
    body: JSON.stringify({
      provider,
      callbackURL,
      disableRedirect: true,
    }),
  }));
  const text = await response.text();
  let parsed: Json = text ? parseJson(text) : null;
  if (!response.ok) {
    const message = responseErrorMessage(parsed) ?? (text || `${response.status} ${response.statusText}`);
    sendHtml(res, renderShell({
      title: "Login failed · Rudder Cloud",
      body: `<section class="hero"><h1>Login failed.</h1><p class="lede">${escapeHtml(message)}</p></section>`,
    }), response.status);
    return;
  }
  const redirectURL = parsed && typeof parsed === "object" && !Array.isArray(parsed) && typeof parsed.url === "string"
    ? parsed.url
    : undefined;
  if (!redirectURL) {
    throw new Error("OAuth provider did not return an authorization URL");
  }
  const responseHeaders = response.headers as Headers & { getSetCookie?: () => string[] };
  for (const cookie of responseHeaders.getSetCookie?.() ?? splitSetCookieHeader(response.headers.get("set-cookie"))) {
    res.setHeader("Set-Cookie", appendHeader(res.getHeader("Set-Cookie"), cookie));
  }
  res.statusCode = 302;
  res.setHeader("Location", redirectURL);
  res.end();
}

const GOOGLE_ICON_SVG = `<svg viewBox="0 0 18 18" xmlns="http://www.w3.org/2000/svg"><path fill="#4285F4" d="M17.64 9.205c0-.639-.057-1.252-.164-1.841H9v3.481h4.844a4.14 4.14 0 0 1-1.796 2.716v2.258h2.908c1.702-1.567 2.684-3.875 2.684-6.614Z"/><path fill="#34A853" d="M9 18c2.43 0 4.467-.806 5.956-2.181l-2.908-2.258c-.806.54-1.836.86-3.048.86-2.344 0-4.328-1.584-5.036-3.711H.957v2.331A8.997 8.997 0 0 0 9 18Z"/><path fill="#FBBC05" d="M3.964 10.71A5.41 5.41 0 0 1 3.68 9c0-.593.102-1.17.284-1.71V4.959H.957A8.996 8.996 0 0 0 0 9c0 1.452.348 2.827.957 4.041l3.007-2.331Z"/><path fill="#EA4335" d="M9 3.58c1.321 0 2.508.454 3.44 1.345l2.582-2.581C13.463.891 11.426 0 9 0A8.997 8.997 0 0 0 .957 4.959L3.964 7.29C4.672 5.163 6.656 3.58 9 3.58Z"/></svg>`;
const GITHUB_ICON_SVG = `<svg viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor"><path fill-rule="evenodd" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>`;

const BRAND_SVG = `<svg viewBox="0 0 64 64" xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><rect x="6" y="6" width="52" height="52" rx="12" fill="#fff"/><rect x="6" y="6" width="52" height="52" rx="12" fill="none" stroke="#111" stroke-width="3"/><path d="M18 20h12c8 0 13 4 13 11s-5 11-13 11h-6v9h-6V20Zm6 6v10h6c4 0 7-2 7-5s-3-5-7-5h-6Z" fill="#111"/><path d="M39 42l8 9" stroke="#111" stroke-width="6" stroke-linecap="round"/></svg>`;

function renderShell(options: { title: string; body: string; footer?: string }): string {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="theme-color" content="#ffffff">
    <title>${escapeHtml(options.title)}</title>
    <link rel="icon" href="https://rudder.viraat.dev/favicon.svg" type="image/svg+xml">
    <style>
      :root { color:#111; background:#fff; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; font-weight:300; }
      * { box-sizing: border-box; }
      body { margin:0; background:#fff; color:#111; min-height:100vh; }
      a { color:inherit; text-decoration-thickness:1px; text-underline-offset:4px; }
      .page { max-width: 1100px; margin: 0 auto; padding: 32px 24px 72px; }
      header { display:flex; align-items:center; justify-content:space-between; gap:24px; padding-bottom: 64px; }
      .brand { display:inline-flex; align-items:center; gap:12px; font-size:15px; text-decoration:none; color:#111; }
      .brand svg { width:28px; height:28px; }
      nav { display:flex; gap:18px; color:#555; font-size:14px; }
      nav a { color:#555; }
      h1 { margin:0; font-size: clamp(44px, 7vw, 88px); line-height:0.94; font-weight:300; letter-spacing:0; }
      .lede { margin: 28px 0 0; max-width: 620px; font-size: clamp(17px, 1.6vw, 21px); line-height:1.45; color:#333; font-weight:300; }
      .card { margin-top: 38px; max-width: 460px; border:1px solid #111; background:#fff; padding: 22px; box-shadow: 12px 12px 0 #111; }
      .provider { display:flex; align-items:center; gap:14px; width:100%; border:1px solid #111; background:#fff; color:#111; padding: 12px 14px; font:inherit; font-size:15px; cursor:pointer; text-decoration:none; transition: background .12s ease, color .12s ease; }
      .provider + .provider { margin-top: 10px; }
      .provider:hover, .provider:focus-visible { background:#111; color:#fff; outline:none; }
      .provider .icon { width:20px; height:20px; flex-shrink:0; display:inline-flex; align-items:center; justify-content:center; }
      .provider .icon svg { width:100%; height:100%; }
      .device { margin-top: 22px; padding-top: 18px; border-top: 1px dashed #111; color:#555; font-size: 14px; line-height:1.55; }
      .device code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background:#f2f2f2; padding: 1px 6px; }
      .device a { color:#111; }
      .empty { color:#555; font-size:15px; line-height:1.5; margin: 0 0 14px; }
      .code-display { font: 600 32px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; letter-spacing: 6px; margin: 20px 0 6px; }
      .muted { color:#666; font-size:14px; line-height:1.55; }
      .pill { display:inline-block; padding: 6px 10px; border:1px solid #111; font:600 12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; text-transform:uppercase; letter-spacing:1px; }
      .btn-primary { display:inline-block; margin-top: 18px; border: 1px solid #111; background:#111; color:#fff; padding: 10px 14px; font:inherit; font-size:14px; text-decoration:none; }
      .btn-primary:hover { background:#fff; color:#111; }
      code.kbd { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background:#f2f2f2; padding: 2px 6px; }
      footer { margin-top: 56px; padding-top: 22px; border-top: 1px solid #ddd; color:#666; font-size: 13px; line-height:1.55; max-width: 620px; }
      @media (max-width: 640px) {
        header { padding-bottom: 36px; }
        .card { box-shadow: 8px 8px 0 #111; padding: 18px; }
        .code-display { font-size: 26px; letter-spacing: 4px; }
      }
    </style>
  </head>
  <body>
    <div class="page">
      <header>
        <a class="brand" href="https://rudder.viraat.dev">${BRAND_SVG}<span>Rudder Cloud</span></a>
        <nav>
          <a href="https://rudder.viraat.dev">rudder.viraat.dev</a>
          <a href="https://github.com/viraatdas/rudder">GitHub</a>
        </nav>
      </header>
      <main>${options.body}</main>
      <footer>${options.footer ?? "You can close this tab once the CLI says you're signed in. Rudder Cloud uses Better Auth for OAuth — your provider tokens stay on this server."}</footer>
    </div>
  </body>
</html>`;
}

function renderGithubAppSetup(url: URL, res: ServerResponse): void {
  const org = url.searchParams.get("org")?.trim();
  const state = createSetupState();
  const action = org
    ? `https://github.com/organizations/${encodeURIComponent(org)}/settings/apps/new?state=${encodeURIComponent(state)}`
    : `https://github.com/settings/apps/new?state=${encodeURIComponent(state)}`;
  const manifest = {
    name: org ? `Rudder Cloud (${org})` : "Rudder Cloud",
    url: "https://rudder.viraat.dev",
    hook_attributes: {
      url: `${baseURL}/api/github/events`,
      active: false,
    },
    redirect_url: `${baseURL}/setup/github/callback`,
    callback_urls: [
      `${authBaseURL}/callback/github`,
    ],
    setup_url: `${baseURL}/setup/github`,
    description: "Rudder Cloud login and coding-agent orchestration.",
    public: false,
    request_oauth_on_install: false,
    default_permissions: {},
    default_events: [],
  };
  sendHtml(res, renderShell({
    title: "Set up GitHub OAuth · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>GitHub OAuth.</h1>
        <p class="lede">This creates a GitHub App from a manifest and stores its OAuth client ID and secret in Rudder Cloud's persisted state.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Callback URL</p>
          <p><code class="kbd">${escapeHtml(`${authBaseURL}/callback/github`)}</code></p>
          <form action="${escapeHtml(action)}" method="post" style="margin-top:18px">
            <input type="hidden" name="manifest" value="${escapeHtml(JSON.stringify(manifest))}">
            <button class="btn-primary" type="submit">Create GitHub App</button>
          </form>
        </div>
      </section>
    `,
  }));
}

async function handleGithubAppSetupCallback(url: URL, res: ServerResponse): Promise<void> {
  const code = url.searchParams.get("code") || "";
  const state = url.searchParams.get("state") || "";
  if (!code || !verifySetupState(state)) {
    sendHtml(res, renderShell({
      title: "Setup expired · Rudder Cloud",
      body: `<section class="hero"><h1>Setup expired.</h1><p class="lede">Open <code class="kbd">/setup/github</code> and try again.</p></section>`,
    }), 400);
    return;
  }
  const app = await githubManifestConversion(code);
  const clientId = typeof app.client_id === "string" ? app.client_id : undefined;
  const clientSecret = typeof app.client_secret === "string" ? app.client_secret : undefined;
  if (!clientId || !clientSecret) {
    throw new Error("GitHub manifest conversion did not return OAuth client credentials");
  }
  setSetting("github_client_id", clientId);
  setSetting("github_client_secret", clientSecret);
  refreshAuthHandler(true);
  await persistDatabaseToS3();
  sendHtml(res, renderShell({
    title: "GitHub OAuth ready · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>All set.</h1>
        <p class="lede">Rudder Cloud saved the GitHub App OAuth credentials. The sign-in page can now show the GitHub button.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Next</p>
          <p><a class="btn-primary" href="/cli/login">Go to sign in</a></p>
          <div class="device"><a href="/health">Check health</a></div>
        </div>
      </section>
    `,
  }));
}

async function handleOAuthCredentialSetup(
  req: IncomingMessage,
  res: ServerResponse,
  provider: "github" | "google",
): Promise<void> {
  const authContext = requireBearer(req);
  requireAdmin(authContext);
  const body = await readJsonBody(req);
  const clientId = stringField(body, "clientId") || stringField(body, "client_id");
  const clientSecret = stringField(body, "clientSecret") || stringField(body, "client_secret");
  if (!clientId || !clientSecret) {
    throw badRequest("clientId and clientSecret are required");
  }
  setSetting(`${provider}_client_id`, clientId.trim());
  setSetting(`${provider}_client_secret`, clientSecret.trim());
  refreshAuthHandler(true);
  await persistDatabaseToS3();
  sendJson(res, 200, {
    ok: true,
    provider,
    auth: configuredProviders(),
  });
}

async function handleCliGithubStart(url: URL, res: ServerResponse): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }
  const existing = githubBrowserLogins.get(deviceCode);
  const githubLogin = existing && existing.expiresAt > Date.now()
    ? existing
    : await startGithubBrowserLogin(deviceCode);
  renderGithubDevicePage(res, deviceCode, githubLogin);
}

async function handleCliGithubWait(url: URL, res: ServerResponse): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  const githubLogin = githubBrowserLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now() || !githubLogin || githubLogin.expiresAt < Date.now()) {
    githubBrowserLogins.delete(deviceCode);
    renderExpiredPage(res);
    return;
  }
  if (Date.now() < githubLogin.nextPollAt) {
    renderGithubDevicePage(res, deviceCode, githubLogin);
    return;
  }
  githubLogin.nextPollAt = Date.now() + githubLogin.intervalMs;
  const poll = await githubOAuthRequest<{
    access_token?: string;
    error?: string;
    error_description?: string;
    interval?: number;
  }>("https://github.com/login/oauth/access_token", {
    client_id: githubDeviceClientId,
    device_code: githubLogin.githubDeviceCode,
    grant_type: "urn:ietf:params:oauth:grant-type:device_code",
  });
  if (poll.access_token) {
    const user = await githubUser(poll.access_token);
    const issued = issueRudderToken(`github:${user.id}`, user.email ?? `${user.login}@users.noreply.github.com`);
    login.token = issued.token;
    login.accountId = issued.accountId;
    login.email = issued.email;
    githubBrowserLogins.delete(deviceCode);
    renderSuccessPage(res, login.email);
    return;
  }
  if (poll.error === "slow_down") {
    githubLogin.intervalMs = Math.max(githubLogin.intervalMs + 5000, (poll.interval ?? 5) * 1000);
    githubLogin.nextPollAt = Date.now() + githubLogin.intervalMs;
  } else if (poll.error && poll.error !== "authorization_pending") {
    githubBrowserLogins.delete(deviceCode);
    sendHtml(res, renderShell({
      title: "GitHub login failed · Rudder Cloud",
      body: `
        <section class="hero">
          <h1>GitHub login failed.</h1>
          <p class="lede">${escapeHtml(poll.error_description || poll.error)}</p>
          <div class="card">
            <p class="muted" style="margin-top:0">Run the CLI again to try once more.</p>
            <p><code class="kbd">rudder login</code></p>
          </div>
        </section>
      `,
    }), 400);
    return;
  }
  renderGithubDevicePage(res, deviceCode, githubLogin);
}

async function startGithubBrowserLogin(deviceCode: string): Promise<GithubBrowserLogin> {
  const start = await githubOAuthRequest<{
    device_code?: string;
    user_code?: string;
    verification_uri?: string;
    verification_uri_complete?: string;
    expires_in?: number;
    interval?: number;
    error?: string;
    error_description?: string;
  }>("https://github.com/login/device/code", {
    client_id: githubDeviceClientId,
    scope: "read:user user:email",
  });
  if (!start.device_code || !start.user_code || !start.verification_uri) {
    throw new Error(start.error_description || start.error || "GitHub device login failed");
  }
  const githubLogin: GithubBrowserLogin = {
    githubDeviceCode: start.device_code,
    userCode: start.user_code,
    verificationUri: start.verification_uri,
    verificationUriComplete: start.verification_uri_complete,
    expiresAt: Date.now() + (start.expires_in ?? 900) * 1000,
    intervalMs: Math.max(1000, (start.interval ?? 5) * 1000),
    nextPollAt: Date.now() + Math.max(1000, (start.interval ?? 5) * 1000),
  };
  githubBrowserLogins.set(deviceCode, githubLogin);
  return githubLogin;
}

async function githubOAuthRequest<T>(url: string, body: Record<string, string>): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `${response.status} ${response.statusText}`);
  }
  return parsed as T;
}

async function githubManifestConversion(code: string): Promise<JsonRecord> {
  const response = await fetch(`https://api.github.com/app-manifests/${encodeURIComponent(code)}/conversions`, {
    method: "POST",
    headers: {
      Accept: "application/vnd.github+json",
      "User-Agent": "rudder-cloud",
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `GitHub manifest conversion failed: ${response.status}`);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("GitHub manifest conversion returned an unexpected response");
  }
  return parsed;
}

function createSetupState(): string {
  const payload = Buffer.from(JSON.stringify({
    exp: Date.now() + 10 * 60 * 1000,
    nonce: randomBytes(16).toString("base64url"),
  })).toString("base64url");
  return `${payload}.${setupStateSignature(payload)}`;
}

function verifySetupState(state: string): boolean {
  const [payload, signature] = state.split(".");
  if (!payload || !signature) {
    return false;
  }
  const expected = setupStateSignature(payload);
  const signatureBuffer = Buffer.from(signature);
  const expectedBuffer = Buffer.from(expected);
  if (signatureBuffer.length !== expectedBuffer.length || !timingSafeEqual(signatureBuffer, expectedBuffer)) {
    return false;
  }
  try {
    const parsed = JSON.parse(Buffer.from(payload, "base64url").toString("utf8")) as { exp?: unknown };
    return typeof parsed.exp === "number" && parsed.exp > Date.now();
  } catch {
    return false;
  }
}

function setupStateSignature(payload: string): string {
  return createHmac("sha256", requiredEnv("BETTER_AUTH_SECRET")).update(payload).digest("base64url");
}

function renderGithubDevicePage(res: ServerResponse, deviceCode: string, githubLogin: GithubBrowserLogin): void {
  const href = githubLogin.verificationUriComplete || githubLogin.verificationUri;
  const refreshSec = Math.ceil(githubLogin.intervalMs / 1000);
  const html = renderShell({
    title: "Authorize GitHub · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Authorize on GitHub.</h1>
        <p class="lede">Open GitHub, paste this code, then come back. This tab will finish on its own.</p>
        <div class="card">
          <span class="pill">Device code</span>
          <div class="code-display">${escapeHtml(githubLogin.userCode)}</div>
          <p class="muted">Waiting for GitHub approval&hellip;</p>
          <a class="btn-primary" href="${escapeHtml(href)}" target="_blank" rel="noreferrer">Open GitHub</a>
          <div class="device">Stuck? Run <code>rudder login</code> again from the CLI.</div>
        </div>
      </section>
    `,
  }).replace(
    "</head>",
    `<meta http-equiv="refresh" content="${refreshSec};url=/cli/github/wait?device_code=${encodeURIComponent(deviceCode)}"></head>`,
  );
  sendHtml(res, html);
}

async function handleCliApprove(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const deviceCode = url.searchParams.get("device_code") || "";
  const login = deviceLogins.get(deviceCode);
  if (!login || login.expiresAt < Date.now()) {
    renderExpiredPage(res);
    return;
  }
  const session = await getBetterAuthSession(req);
  if (!session?.user) {
    renderLoginPage(url, res);
    return;
  }
  // Only bake the email into the (admin-trusted) rdr_ token when it is VERIFIED, so
  // the token path shares the same invariant as requireAdminRequest's session path:
  // an unverified email can never become an admin token. (better-auth is social-only
  // today, but enforce it at the trust boundary rather than relying on that.)
  const verifiedEmail =
    typeof session.user.email === "string" &&
    (session.user as { emailVerified?: unknown }).emailVerified === true
      ? session.user.email
      : undefined;
  const issued = issueRudderToken(
    String(session.user.id || `better-auth:${randomUUID()}`),
    verifiedEmail,
  );
  login.token = issued.token;
  login.accountId = issued.accountId;
  login.email = verifiedEmail;
  renderSuccessPage(res, login.email);
}

async function handleAdminWorkspaceGc(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
): Promise<void> {
  await requireAdminRequest(req);
  ensureFlyConfigured();
  const dryRun = url.searchParams.get("dry") === "1" || url.searchParams.get("dryRun") === "1";
  const now = new Date().toISOString();
  const workspaceRows = database
    .prepare("select * from rudder_workspaces")
    .all() as Record<string, unknown>[];
  const sailRows = database
    .prepare("select * from rudder_sails")
    .all() as Record<string, unknown>[];

  const reclaimedWorkspaces: { id: string; machineId: string; previousStatus: string }[] = [];
  const reclaimedSails: { id: string; machineId: string; previousStatus: string }[] = [];
  const reclaimedVolumes: { id: string; volumeId: string }[] = [];
  const skipped: { kind: "workspace" | "sail"; id: string; reason: string }[] = [];
  const errors: { kind: "workspace" | "sail"; id: string; error: string }[] = [];

  const update = database.prepare(
    "update rudder_workspaces set status = ?, machine_id = NULL, machine_state = 'destroyed', updated_at = ? where id = ?",
  );
  const clearVolume = database.prepare(
    "update rudder_workspaces set volume_id = NULL, updated_at = ? where id = ?",
  );
  const updateSailRow = database.prepare(
    "update rudder_sails set status = ?, machine_id = NULL, machine_state = 'destroyed', updated_at = ? where id = ?",
  );

  for (const row of workspaceRows) {
    const id = String(row.id);
    const status = String(row.status);
    const machineId = optionalString(row.machine_id);
    if (status === "deleting") {
      skipped.push({ kind: "workspace", id, reason: "being deleted (the delete sweep owns it)" });
      continue;
    }
    if (!machineId) {
      skipped.push({ kind: "workspace", id, reason: "no machineId" });
      continue;
    }
    if (status === "stopped" && !machineId) {
      skipped.push({ kind: "workspace", id, reason: "already stopped" });
      continue;
    }
    try {
      const exists = await flyMachineExists(machineId);
      if (exists) {
        skipped.push({ kind: "workspace", id, reason: "machine still present on Fly" });
        continue;
      }
      reclaimedWorkspaces.push({ id, machineId, previousStatus: status });
      const volumeId = optionalString(row.volume_id);
      if (volumeId) {
        reclaimedVolumes.push({ id, volumeId });
      }
      if (!dryRun) {
        update.run("stopped", now, id);
        if (volumeId) {
          await destroyWorkspaceVolume(volumeId);
          clearVolume.run(now, id);
        }
      }
    } catch (error) {
      errors.push({
        kind: "workspace",
        id,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  for (const row of sailRows) {
    const id = String(row.id);
    const status = String(row.status);
    const machineId = optionalString(row.machine_id);
    if (status === "deleting") {
      skipped.push({ kind: "sail", id, reason: "being deleted (the delete sweep owns it)" });
      continue;
    }
    if (!machineId) {
      skipped.push({ kind: "sail", id, reason: "no machineId" });
      continue;
    }
    if (status === "completed" || status === "failed") {
      skipped.push({ kind: "sail", id, reason: `terminal status: ${status}` });
      continue;
    }
    try {
      const exists = await flyMachineExists(machineId);
      if (exists) {
        skipped.push({ kind: "sail", id, reason: "machine still present on Fly" });
        continue;
      }
      reclaimedSails.push({ id, machineId, previousStatus: status });
      if (!dryRun) {
        updateSailRow.run("completed", now, id);
      }
    } catch (error) {
      errors.push({
        kind: "sail",
        id,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }

  if (!dryRun && (reclaimedWorkspaces.length || reclaimedSails.length)) {
    schedulePersistDatabase();
  }

  sendJson(res, 200, {
    ok: true,
    dryRun,
    reclaimedWorkspaces,
    reclaimedSails,
    reclaimedVolumes,
    skipped,
    errors,
  });
}

async function flyMachineExists(machineId: string): Promise<boolean> {
  const response = await fetch(
    `${flyApiBase}/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    {
      method: "GET",
      headers: {
        Authorization: `Bearer ${flyApiToken}`,
        Accept: "application/json",
      },
    },
  );
  if (response.status === 404) {
    return false;
  }
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`Fly API ${response.status}: ${text.trim()}`);
  }
  // 200 means the machine exists, but Fly may still report state "destroyed"
  // for already-deleted machines. Treat destroyed/destroying as gone.
  try {
    const body = (await response.json()) as { state?: string } | null;
    const state = body?.state;
    if (state === "destroyed" || state === "destroying") {
      return false;
    }
  } catch {
    // fall through: assume exists
  }
  return true;
}

async function requireAdminRequest(req: IncomingMessage): Promise<void> {
  const adminToken = process.env.RUDDER_ADMIN_TOKEN || "";
  const authHeader = req.headers.authorization || "";
  const bearer = authHeader.startsWith("Bearer ")
    ? authHeader.slice("Bearer ".length).trim()
    : "";
  if (adminToken && bearer && timingSafeEqualString(bearer, adminToken)) {
    return;
  }
  // Fallback: admin-email rudder bearer token.
  if (bearer.startsWith("rdr_")) {
    const ctx = requireBearer(req);
    requireAdmin(ctx);
    return;
  }
  // Fallback: better-auth session cookie from an admin email. The email must be
  // verified — an unverified provider email must never satisfy the admin check.
  const session = await getBetterAuthSession(req);
  const email = typeof session?.user?.email === "string" ? session.user.email.toLowerCase() : undefined;
  const emailVerified = (session?.user as { emailVerified?: unknown } | undefined)?.emailVerified === true;
  if (email && emailVerified && adminEmails.has(email)) {
    return;
  }
  throw unauthorized();
}

function timingSafeEqualString(a: string, b: string): boolean {
  const ba = Buffer.from(a);
  const bb = Buffer.from(b);
  if (ba.length !== bb.length) {
    return false;
  }
  return timingSafeEqual(ba, bb);
}

async function handleSailApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const heartbeatMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/heartbeat$/);
  if (req.method === "POST" && heartbeatMatch) {
    await handleWorkerHeartbeat(req, res, heartbeatMatch[1]);
    return;
  }

  const workerSecretsMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/secrets$/);
  if (req.method === "GET" && workerSecretsMatch) {
    await handleWorkerSecrets(req, res, findSailById.get(workerSecretsMatch[1]) as Record<string, unknown> | undefined);
    return;
  }

  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/sail") {
    await refreshAccountSails(authContext.accountId);
    sendJson(res, 200, { sails: listAccountSails(authContext.accountId) });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/launch") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const sail = await createSail(authContext.accountId, body);
    sendJson(res, 200, sail);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/sail/onload") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const sail = await createSail(authContext.accountId, body, stringField(body, "runId"));
    sendJson(res, 200, sail);
    return;
  }
  const bootstrapMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/bootstrap$/);
  if (req.method === "POST" && bootstrapMatch) {
    const sail = getAccountSail(bootstrapMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    const next = await refreshByoVmBootstrap(sail, authContext.accountId);
    sendJson(res, 200, next);
    return;
  }
  const inputMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/input$/);
  if (req.method === "POST" && inputMatch) {
    const sail = getAccountSail(inputMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    const body = await readJsonBody(req);
    const text = stringField(body, "text");
    if (typeof text !== "string" || text.length === 0) {
      sendJson(res, 400, { error: "text is required" });
      return;
    }
    const submit = body && typeof body === "object" && !Array.isArray(body) && body.submit === false ? false : true;
    const delivered = sendInputToChannel("sail", sail.id, text, submit);
    sendJson(res, delivered ? 200 : 409, delivered
      ? { delivered: true }
      : { delivered: false, error: "instance is not connected" });
    return;
  }
  const outputMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/output$/);
  if (req.method === "GET" && outputMatch) {
    const sail = getAccountSail(outputMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    sendJson(res, 200, {
      id: sail.id,
      connected: instanceHasLiveWorker("sail", sail.id),
      output: readChannelOutput("sail", sail.id),
    });
    return;
  }
  const deleteMatch = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)(?:\/delete)?$/);
  if (deleteMatch && (req.method === "DELETE" || (req.method === "POST" && url.pathname.endsWith("/delete")))) {
    const sail = getAccountSail(deleteMatch[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    sendJson(res, 200, await requestDeletion("sail", sail.id, authContext.accountId));
    return;
  }
  const match = url.pathname.match(/^\/api\/rudder\/sail\/([^/]+)\/(pause|resume|onload|stop)$/);
  if (req.method === "POST" && match) {
    const sail = getAccountSail(match[1], authContext.accountId);
    if (!sail) {
      sendJson(res, 404, { error: "sail not found" });
      return;
    }
    if (sail.status === "deleting") {
      sendJson(res, 409, { error: "sail is being deleted" });
      return;
    }
    const next = await mutateSail(sail, authContext.accountId, match[2]);
    sendJson(res, 200, next);
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

function parseSecretItem(body: Json): SecretItemInput {
  const record = (body && typeof body === "object" && !Array.isArray(body) ? body : {}) as JsonRecord;
  const name = typeof record.name === "string" ? record.name.trim() : "";
  const kind = record.kind === "file" ? "file" : record.kind === "env" ? "env" : undefined;
  const valueBase64 = typeof record.valueBase64 === "string" ? record.valueBase64 : "";
  if (!name || !kind || !valueBase64) {
    throw new SecretsVaultError(400, "name, kind (env|file) and valueBase64 are required");
  }
  return {
    name,
    kind,
    filePath: typeof record.filePath === "string" ? record.filePath : undefined,
    value: Buffer.from(valueBase64, "base64"),
    source: typeof record.source === "string" ? record.source : undefined,
  };
}

const MAX_SECRETS_BULK_BODY_BYTES = 64 * 1024 * 1024;

async function handleSecretsApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/secrets") {
    const { secrets, version } = secretsVault().list(authContext.accountId);
    sendJson(res, 200, { secrets: secrets as unknown as Json, secretsVersion: version });
    return;
  }
  if (req.method === "PUT" && url.pathname === "/api/rudder/secrets/item") {
    const body = await readJsonBody(req, 4 * 1024 * 1024);
    const saved = secretsVault().put(authContext.accountId, parseSecretItem(body));
    // Persist immediately: a secret acknowledged to the client must survive a
    // control-plane crash, not wait out the 750ms debounce window.
    await persistDatabaseToS3();
    sendJson(res, 200, { ok: true, secret: saved as unknown as Json });
    return;
  }
  if (req.method === "DELETE" && url.pathname === "/api/rudder/secrets/item") {
    const name = (url.searchParams.get("name") || "").trim();
    if (!name) {
      sendJson(res, 400, { error: "name is required" });
      return;
    }
    const removed = secretsVault().remove(authContext.accountId, name);
    await persistDatabaseToS3();
    sendJson(res, removed ? 200 : 404, removed ? { ok: true } : { error: "secret not found" });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/secrets/bulk") {
    const body = await readJsonBody(req, MAX_SECRETS_BULK_BODY_BYTES);
    const items = body && typeof body === "object" && !Array.isArray(body) && Array.isArray((body as JsonRecord).items)
      ? ((body as JsonRecord).items as Json[])
      : [];
    const results: Json[] = [];
    for (const item of items) {
      try {
        const saved = secretsVault().put(authContext.accountId, parseSecretItem(item));
        results.push({ name: saved.name, ok: true });
      } catch (error) {
        const parsedName = item && typeof item === "object" && !Array.isArray(item) && typeof (item as JsonRecord).name === "string"
          ? String((item as JsonRecord).name)
          : "<invalid>";
        results.push({
          name: parsedName,
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    }
    await persistDatabaseToS3();
    sendJson(res, 200, { results });
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

// Worker-authenticated secrets fetch: the supervisor calls this at every boot
// (rootfs is ephemeral) with its rdrw_ bearer, before spawning the agent.
// Secret values are deliberately NOT placed in Fly machine env/config, which
// is readable via the Fly API.
async function handleWorkerSecrets(
  req: IncomingMessage,
  res: ServerResponse,
  row: Record<string, unknown> | undefined,
): Promise<void> {
  if (!row) {
    sendJson(res, 404, { error: "not found" });
    return;
  }
  requireWorkerBearer(req, row);
  if (!secretsKeyBase64) {
    // Vault not configured: boot proceeds on legacy snapshot credentials.
    sendJson(res, 200, { version: 0, env: {}, files: [] });
    return;
  }
  sendJson(res, 200, secretsVault().exportForWorker(String(row.account_id)) as unknown as Json);
}

async function handleWorkspaceApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const workerSecretsMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/secrets$/);
  if (req.method === "GET" && workerSecretsMatch) {
    await handleWorkerSecrets(req, res, findWorkspaceById.get(workerSecretsMatch[1]) as Record<string, unknown> | undefined);
    return;
  }

  const heartbeatMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/heartbeat$/);
  if (req.method === "POST" && heartbeatMatch) {
    await handleWorkspaceHeartbeat(req, res, heartbeatMatch[1]);
    return;
  }

  const snapshotUrlMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/snapshot-url$/);
  if (req.method === "GET" && snapshotUrlMatch) {
    await handleWorkspaceSnapshotUrl(req, res, snapshotUrlMatch[1]);
    return;
  }

  const authContext = requireBearer(req);
  if (req.method === "GET" && url.pathname === "/api/rudder/workspace") {
    sendJson(res, 200, {
      workspaces: listAccountWorkspaces(authContext.accountId).map(annotateWorkspaceClients),
    });
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/rudder/workspace/lookup") {
    const repo = url.searchParams.get("repo");
    const key = repo && REPO_SLUG_RE.test(repo.trim().replace(/\.git$/, ""))
      ? workspaceKeyForRepo(repo.trim().replace(/\.git$/, ""))
      : url.searchParams.get("key");
    if (!key) {
      sendJson(res, 400, { error: "key or repo is required" });
      return;
    }
    const row = findWorkspaceByKey.get(authContext.accountId, key) as
      | Record<string, unknown>
      | undefined;
    if (!row) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    sendJson(res, 200, annotateWorkspaceClients(rowToWorkspace(row)));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/workspace/attach") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const result = await ensureWorkspaceForAttach(authContext.accountId, body);
    sendJson(res, 200, result);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/workspace/create") {
    const body = await readJsonBody(req, 1024 * 1024);
    const result = await ensureCloneWorkspace(authContext.accountId, body);
    sendJson(res, 200, result);
    return;
  }
  const deleteMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)(?:\/delete)?$/);
  if (deleteMatch && (req.method === "DELETE" || (req.method === "POST" && url.pathname.endsWith("/delete")))) {
    const workspace = getAccountWorkspace(deleteMatch[1], authContext.accountId);
    if (!workspace) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    sendJson(res, 200, await requestDeletion("workspace", workspace.id, authContext.accountId));
    return;
  }
  const stopMatch = url.pathname.match(/^\/api\/rudder\/workspace\/([^/]+)\/(stop|pause|resume)$/);
  if (req.method === "POST" && stopMatch) {
    const workspace = getAccountWorkspace(stopMatch[1], authContext.accountId);
    if (!workspace) {
      sendJson(res, 404, { error: "workspace not found" });
      return;
    }
    if (workspace.status === "deleting") {
      sendJson(res, 409, { error: "workspace is being deleted" });
      return;
    }
    const next = await mutateWorkspace(workspace, stopMatch[2]);
    sendJson(res, 200, next);
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

async function createSail(
  accountId: string,
  body: Json,
  preferredId?: string,
  existingSnapshotKey?: string,
): Promise<Sail> {
  const runtime = sailRuntimeFromBody(body);
  ensureCloudRuntimeConfigured(runtime);
  const now = new Date().toISOString();
  // A preferred id (onload keys sails by the local run id) must be retryable:
  // a handoff whose first attempt died mid-flight leaves a sail row behind,
  // and the retry used to 500 on the id's UNIQUE constraint. If that sail's
  // worker is LIVE, the conversation is already in the cloud — return it
  // instead of cloning it. Otherwise clear the corpse (row + any machine)
  // and launch fresh.
  if (preferredId) {
    const existing = getAccountSail(preferredId, accountId);
    if (existing) {
      if (instanceHasLiveWorker("sail", preferredId)) {
        return existing;
      }
      if (existing.machineId) {
        await destroyFlyMachine(existing.machineId).catch(() => undefined);
      }
      deleteSailRow.run({ id: preferredId, accountId });
    }
  }
  // Slack-launched sails reuse an already-stored snapshot (there is no terminal
  // to upload a fresh one from); everything else uploads as before.
  const snapshot = existingSnapshotKey
    ? { key: existingSnapshotKey }
    : await storeSnapshot(accountId, body);
  const id = preferredId || uniqueSailId(stringField(body, "name"));
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const task = stringField(body, "task");
  const repoName = stringField(body, "repoName");
  const snapshotInput = objectField(body, "snapshot");
  const manifest = snapshotInput ? objectField(snapshotInput, "manifest") : undefined;
  const manifestRepo = manifest ? objectField(manifest, "repo") : undefined;
  const branch = manifestRepo ? stringField(manifestRepo, "branch") : undefined;
  insertSail.run({
    id,
    accountId,
    status: "queued",
    runtime,
    repoName: repoName ?? null,
    task: task ?? null,
    branch: branch ?? null,
    machineId: null,
    machineState: runtime === "byo-vm" ? "bootstrap-pending" : null,
    snapshotKey: snapshot.key,
    manifestJson: JSON.stringify(manifest ?? {}),
    workerTokenHash: tokenHash(workerToken),
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });

  // Announce the new instance into the shared Slack channel so it is talk-able
  // from there. Fire-and-forget: a Slack hiccup must never fail a launch.
  void announceSailToSlack(id, task, repoName, runtime);

  if (runtime === "byo-vm") {
    const sail = getAccountSail(id, accountId) ?? {
      id,
      status: "queued",
      runtime,
      repoName,
      task,
      branch,
      machineState: "bootstrap-pending",
      snapshotKey: snapshot.key,
      createdAt: now,
      updatedAt: now,
    };
    return {
      ...sail,
      bootstrapCommand: await byoVmBootstrapCommand({
        sailId: id,
        accountId,
        snapshotKey: snapshot.key,
        workerToken,
        task,
        repoName,
      }),
    };
  }

  const snapshotUrl = await signedSnapshotUrl(snapshot.key);
  const machine = await createFlyMachine({
    sailId: id,
    accountId,
    snapshotUrl,
    workerToken,
    task,
    repoName,
  }).catch((error) => {
    recordStartFailure("sail", id, error);
    throw error;
  });
  const status = flyStateToSailStatus(machine.state);
  updateSail.run({
    id,
    accountId,
    status,
    machineId: machine.id ?? null,
    machineState: machine.state ?? null,
    updatedAt: new Date().toISOString(),
  });
  return getAccountSail(id, accountId) ?? {
    id,
    status,
    runtime,
    repoName,
    task,
    branch,
    machineId: machine.id,
    machineState: machine.state,
    snapshotKey: snapshot.key,
    createdAt: now,
    updatedAt: now,
  };
}

function uniqueSailId(name?: string): string {
  const base = slugForSailId(name) || `${cloudWord()}-${cloudWord()}`;
  let id = base;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    if (!findSailById.get(id)) {
      return id;
    }
    id = `${base}-${randomBytes(2).toString("hex")}`;
  }
  return `cloud-${randomBytes(5).toString("hex")}`;
}

function slugForSailId(value?: string): string {
  const slug = (value || "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 42)
    .replace(/-+$/g, "");
  return slug ? `cloud-${slug}` : "";
}

function cloudWord(): string {
  const words = [
    "amber",
    "atlas",
    "bright",
    "harbor",
    "orbit",
    "rapid",
    "river",
    "signal",
    "silver",
    "summit",
    "swift",
    "wave",
  ];
  return words[randomBytes(1)[0] % words.length] || "cloud";
}

async function storeSnapshot(accountId: string, body: Json): Promise<{ key: string }> {
  ensureS3Configured();
  const snapshot = objectField(body, "snapshot");
  const base64 = snapshot ? stringField(snapshot, "base64") : undefined;
  const contentType = snapshot ? stringField(snapshot, "contentType") || "application/gzip" : "application/gzip";
  if (!base64) {
    throw badRequest("snapshot.base64 is required");
  }
  const buffer = Buffer.from(base64, "base64");
  const key = `snapshots/${accountId}/${new Date().toISOString().slice(0, 10)}/${randomUUID()}.tgz`;
  await s3.send(new PutObjectCommand({
    Bucket: snapshotBucket,
    Key: key,
    Body: buffer,
    ContentType: contentType,
    ServerSideEncryption: "AES256",
  }));
  return { key };
}

async function signedSnapshotUrl(key: string): Promise<string> {
  ensureS3Configured();
  return await getSignedUrl(
    s3,
    new GetObjectCommand({
      Bucket: snapshotBucket,
      Key: key,
    }),
    { expiresIn: 60 * 60 },
  );
}

async function createFlyMachine(params: {
  sailId: string;
  accountId: string;
  snapshotUrl: string;
  workerToken: string;
  task?: string;
  repoName?: string;
}): Promise<FlyMachine> {
  ensureFlyConfigured();
  const machine = await flyRequest<FlyMachine>(`/v1/apps/${encodeURIComponent(flyAppName)}/machines`, {
    method: "POST",
    body: {
      name: flyMachineName("rudder", params.sailId),
      region: flyRegion,
      config: {
        image: flyWorkerImage,
        env: {
          RUDDER_SAIL_ID: params.sailId,
          RUDDER_ACCOUNT_ID: params.accountId,
          RUDDER_CLOUD_URL: baseURL,
          RUDDER_WORKER_TOKEN: params.workerToken,
          RUDDER_SNAPSHOT_URL: params.snapshotUrl,
          RUDDER_TASK: params.task || "",
          RUDDER_REPO_NAME: params.repoName || "",
        },
        guest: {
          cpu_kind: flyWorkerCpuKind,
          cpus: flyWorkerCpus,
          memory_mb: flyWorkerMemoryMb,
        },
        restart: {
          policy: "no",
        },
        auto_destroy: false,
      },
    },
  });
  if (!machine.id) {
    return machine;
  }
  return await startFlyMachine(machine.id, `sail ${params.sailId}`);
}

async function mutateSail(sail: Sail, accountId: string, action: string): Promise<Sail> {
  if (sail.runtime === "fly") {
    return await mutateFlySail(sail, accountId, action);
  }
  if (action === "stop") {
    updateSail.run({
      id: sail.id,
      accountId,
      status: "stopped",
      machineId: sail.machineId ?? null,
      machineState: "stopped",
      updatedAt: new Date().toISOString(),
    });
    return getAccountSail(sail.id, accountId) ?? sail;
  }
  throw badRequest("BYO VM sails cannot be paused or resumed from Rudder Cloud. Stop the worker on your VM, or use stop to mark it stopped.");
}

async function mutateFlySail(sail: Sail, accountId: string, action: string): Promise<Sail> {
  if (!sail.machineId) {
    throw badRequest("sail does not have a Fly machine yet");
  }
  let machine: FlyMachine;
  if (action === "pause") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/suspend`,
      { method: "POST", body: {} },
    );
  } else if (action === "resume" || action === "onload") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/start`,
      { method: "POST", body: {} },
    );
  } else if (action === "stop") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGINT", timeout: "10s" } },
    );
  } else {
    throw badRequest(`unsupported sail action: ${action}`);
  }
  updateSail.run({
    id: sail.id,
    accountId,
    status: action === "pause" ? "paused" : flyStateToSailStatus(machine.state),
    machineId: sail.machineId,
    machineState: machine.state ?? null,
    updatedAt: new Date().toISOString(),
  });
  return getAccountSail(sail.id, accountId) ?? sail;
}

async function refreshByoVmBootstrap(sail: Sail, accountId: string): Promise<Sail> {
  if (sail.runtime !== "byo-vm") {
    throw badRequest("bootstrap is only available for BYO VM sails");
  }
  if (!sail.snapshotKey) {
    throw badRequest("sail does not have a snapshot");
  }
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  updateWorkerToken.run({
    id: sail.id,
    accountId,
    workerTokenHash: tokenHash(workerToken),
    updatedAt: now,
  });
  const next = getAccountSail(sail.id, accountId) ?? { ...sail, updatedAt: now };
  return {
    ...next,
    bootstrapCommand: await byoVmBootstrapCommand({
      sailId: sail.id,
      accountId,
      snapshotKey: sail.snapshotKey,
      workerToken,
      task: sail.task,
      repoName: sail.repoName,
    }),
  };
}

async function byoVmBootstrapCommand(params: {
  sailId: string;
  accountId: string;
  snapshotKey: string;
  workerToken: string;
  task?: string;
  repoName?: string;
}): Promise<string> {
  const snapshotUrl = await signedSnapshotUrl(params.snapshotKey);
  const env: Array<[string, string]> = [
    ["RUDDER_SAIL_ID", params.sailId],
    ["RUDDER_ACCOUNT_ID", params.accountId],
    ["RUDDER_CLOUD_URL", baseURL],
    ["RUDDER_WORKER_TOKEN", params.workerToken],
    ["RUDDER_SNAPSHOT_URL", snapshotUrl],
    ["RUDDER_TASK", params.task || ""],
    ["RUDDER_REPO_NAME", params.repoName || ""],
  ];
  const armWorkerImage = imageWithTag(flyWorkerImage, "arm64");
  const dockerLines = [
    "docker run --rm",
    ...env.map(([key, value]) => `  -e ${key}=${shellQuote(value)}`),
    "  \"$RUDDER_WORKER_IMAGE\"",
  ];
  const dockerCommand = dockerLines.map((line, index) => index < dockerLines.length - 1 ? `${line} \\` : line).join("\n");
  return [
    `RUDDER_WORKER_IMAGE=${shellQuote(flyWorkerImage)}`,
    "case \"$(uname -m)\" in",
    `  aarch64|arm64) RUDDER_WORKER_IMAGE=${shellQuote(armWorkerImage)} ;;`,
    "esac",
    dockerCommand,
  ].join("\n");
}

async function handleWorkerHeartbeat(req: IncomingMessage, res: ServerResponse, sailId: string): Promise<void> {
  const sailRow = findSailById.get(sailId) as Record<string, unknown> | undefined;
  if (!sailRow) {
    sendJson(res, 404, { error: "sail not found" });
    return;
  }
  requireWorkerBearer(req, sailRow);
  if (String(sailRow.status) === "deleting") {
    sendJson(res, 410, { error: "sail deleted", status: "deleting" });
    return;
  }
  const body = await readJsonBody(req);
  const state = stringField(body, "state");
  const status: SailStatus = state === "completed"
    ? "completed"
    : state === "failed"
      ? "failed"
      : "running";
  const now = new Date().toISOString();
  const previousStatus = String(sailRow.status);
  updateHeartbeat.run({
    id: sailId,
    status,
    machineId: stringField(body, "machineId") ?? null,
    machineState: state || status,
    failureReason: status === "failed" ? heartbeatFailureReason(body) : null,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
  // On the transition into a terminal state, post the final result + a tail of
  // output into the instance's Slack thread.
  if ((status === "completed" || status === "failed") && previousStatus !== status) {
    const threadTs = optionalString(sailRow.slack_thread_ts);
    const icon = status === "completed" ? "✅" : "❌";
    const tail = readChannelOutput("sail", sailId);
    void postToSlack(
      `${icon} *${sailId}* ${status}.${tail ? `\n${formatOutputForSlack(tail)}` : ""}`,
      threadTs,
    );
  }
  sendJson(res, 200, { ok: true, status });
}

// The worker says why it failed when it can ("git clone failed: ..."); an exit
// code alone still beats a bare "failed".
function heartbeatFailureReason(body: Json): string | null {
  const reason = stringField(body, "reason")?.trim();
  if (reason) {
    return reason.slice(0, 2000);
  }
  const exitCode = numberField(body, "exitCode");
  return exitCode === undefined ? null : `worker exited with code ${exitCode}`;
}

function recordStartFailure(kind: "sail" | "workspace", id: string, error: unknown): void {
  const message = (error instanceof Error ? error.message : String(error)).slice(0, 2000);
  const now = new Date().toISOString();
  try {
    if (kind === "sail") {
      markSailFailed.run(message, now, id);
    } else {
      markWorkspaceFailed.run(message, now, id);
    }
    schedulePersistDatabase();
  } catch {
    // the original error is what the caller reports
  }
}

async function refreshAccountSails(accountId: string): Promise<void> {
  const sails = listAccountSails(accountId);
  for (const sail of sails) {
    if (sail.runtime !== "fly" || !flyApiToken || !flyAppName) {
      continue;
    }
    if (sail.status === "completed" || sail.status === "failed") {
      continue;
    }
    if (!sail.machineId) {
      continue;
    }
    if (shouldPauseStaleSail(sail)) {
      await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}/suspend`,
        { method: "POST", body: {} },
      ).catch(() => null);
      const now = new Date().toISOString();
      updateSail.run({
        id: sail.id,
        accountId,
        status: "paused",
        machineId: sail.machineId,
        machineState: "suspended",
        updatedAt: now,
      });
      continue;
    }
    const machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(sail.machineId)}`,
      { method: "GET" },
    ).catch(() => null);
    if (!machine) {
      continue;
    }
    updateSail.run({
      id: sail.id,
      accountId,
      status: flyStateToSailStatus(machine.state),
      machineId: sail.machineId,
      machineState: machine.state ?? null,
      updatedAt: new Date().toISOString(),
    });
  }
}

function shouldPauseStaleSail(sail: Sail): boolean {
  if (sail.status !== "running" || !idlePauseMs || idlePauseMs < 1000) {
    return false;
  }
  const heartbeatOrCreated = sail.lastHeartbeatAt ?? sail.createdAt;
  const lastSeen = Date.parse(heartbeatOrCreated);
  return Number.isFinite(lastSeen) && Date.now() - lastSeen > idlePauseMs;
}

async function ensureWorkspaceForAttach(accountId: string, body: Json): Promise<JsonRecord> {
  ensureCloudRuntimeConfigured("fly");
  const workspaceKey = stringField(body, "workspaceKey");
  if (!workspaceKey || workspaceKey.length < 4 || workspaceKey.length > 128) {
    throw badRequest("workspaceKey is required (4-128 chars)");
  }
  const repoName = stringField(body, "repoName");
  const existingRow = findWorkspaceByKey.get(accountId, workspaceKey) as Record<string, unknown> | undefined;
  if (existingRow) {
    const existing = rowToWorkspace(existingRow);
    return await reuseOrRestartWorkspace(existing, body, repoName);
  }
  return await createWorkspace(accountId, workspaceKey, body, repoName);
}

async function createWorkspace(
  accountId: string,
  workspaceKey: string,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  const region = sanitizeRegion(stringField(body, "region"));
  const snapshot = await storeSnapshot(accountId, body);
  const snapshotFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId(repoName);
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  insertWorkspace.run({
    id,
    accountId,
    workspaceKey,
    repoName: repoName ?? null,
    status: "queued",
    machineId: null,
    machineState: null,
    snapshotKey: snapshot.key,
    snapshotFingerprint,
    region,
    volumeId: null,
    workerTokenHash: tokenHash(workerToken),
    sourceKind: "snapshot",
    repoUrl: null,
    gitRef: null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  const regionToUse = region ?? flyRegion;
  const volume = await createWorkspaceVolume(id, regionToUse);
  const volumeId = volume.id;
  updateWorkspaceVolume.run({
    id,
    volumeId: volumeId ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  const snapshotUrl = await signedSnapshotUrl(snapshot.key);
  const machine = await createFlyWorkspaceMachine({
    workspaceId: id,
    accountId,
    snapshotUrl,
    workerToken,
    repoName,
    region: regionToUse,
    volumeId,
  });
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({
    id,
    status: started.status,
    machineId: machine.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  const workspace = (findWorkspaceById.get(id) as Record<string, unknown> | undefined);
  const result = workspace ? rowToWorkspace(workspace) : {
    id,
    accountId,
    workspaceKey,
    repoName,
    status: started.status,
    machineId: machine.id,
    machineState: started.machineState,
    snapshotKey: snapshot.key,
    snapshotFingerprint: snapshotFingerprint ?? undefined,
    region: region ?? undefined,
    createdAt: now,
    updatedAt: now,
  } as Workspace;
  return { ...result, isNew: true } as unknown as JsonRecord;
}

const REPO_SLUG_RE = /^[\w.-]+\/[\w.-]+$/;

// Clone-based workspaces are keyed by repo identity instead of the local
// checkout path. The "gh:" prefix keeps the keyspace disjoint from the
// 32-hex path-hash keys snapshot workspaces use.
function workspaceKeyForRepo(slug: string): string {
  return `gh:${createHash("sha256").update(`github.com/${slug.toLowerCase()}`).digest("hex").slice(0, 29)}`;
}

async function ensureCloneWorkspace(accountId: string, body: Json): Promise<JsonRecord> {
  ensureCloudRuntimeConfigured("fly");
  const repo = (stringField(body, "repo") ?? "").trim().replace(/\.git$/, "");
  if (!REPO_SLUG_RE.test(repo)) {
    throw badRequest("repo must look like owner/name");
  }
  const branch = stringField(body, "branch")?.trim() || undefined;
  const workspaceKey = workspaceKeyForRepo(repo);
  const existingRow = findWorkspaceByKey.get(accountId, workspaceKey) as Record<string, unknown> | undefined;
  const repoName = repo.split("/")[1];
  if (existingRow) {
    return await reuseOrRestartWorkspace(rowToWorkspace(existingRow), body, repoName);
  }
  const repoUrl = `https://github.com/${repo}.git`;
  const region = sanitizeRegion(stringField(body, "region"));
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId(repoName);
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  insertWorkspace.run({
    id,
    accountId,
    workspaceKey,
    repoName,
    status: "queued",
    machineId: null,
    machineState: null,
    snapshotKey: null,
    snapshotFingerprint: null,
    region,
    volumeId: null,
    workerTokenHash: tokenHash(workerToken),
    sourceKind: "git-clone",
    repoUrl,
    gitRef: branch ?? null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  const regionToUse = region ?? flyRegion;
  const volume = await createWorkspaceVolume(id, regionToUse);
  updateWorkspaceVolume.run({
    id,
    volumeId: volume.id ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  const machine = await createFlyWorkspaceMachine({
    workspaceId: id,
    accountId,
    workerToken,
    repoName,
    region: regionToUse,
    volumeId: volume.id,
    gitRemote: repoUrl,
    gitRef: branch,
  });
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({
    id,
    status: started.status,
    machineId: machine.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  const next = findWorkspaceById.get(id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : { id, accountId, workspaceKey }), isNew: true } as unknown as JsonRecord;
}

async function reuseOrRestartWorkspace(
  workspace: Workspace,
  body: Json,
  repoName: string | undefined,
): Promise<JsonRecord> {
  ensureFlyConfigured();
  let machine: FlyMachine | null = null;
  if (workspace.machineId) {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}`,
      { method: "GET" },
    ).catch(() => null);
  }
  if (instanceHasLiveWorker("workspace", workspace.id)) {
    const now = new Date().toISOString();
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: "running",
      machineId: workspace.machineId ?? machine?.id ?? null,
      machineState: "running",
      updatedAt: now,
    });
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : { ...workspace, status: "running" }), isNew: false } as unknown as JsonRecord;
  }
  // Path 1: machine already running -> just attach.
  if (machine && (machine.state === "started" || machine.state === "starting")) {
    const now = new Date().toISOString();
    updateWorkspaceMachine.run({
      id: workspace.id,
      status: flyStateToWorkspaceStatus(machine.state),
      machineId: machine.id ?? workspace.machineId ?? null,
      machineState: machine.state ?? null,
      updatedAt: now,
    });
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
    const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
    return { ...(next ? rowToWorkspace(next) : workspace), isNew: false } as unknown as JsonRecord;
  }

  const isClone = workspace.sourceKind === "git-clone";
  const incomingFingerprint = stringField(body, "snapshotFingerprint") ?? null;
  const snapshotInput = objectField(body, "snapshot");
  // Clone workspaces have no snapshot to compare: the repo's source of truth
  // is origin, so a stopped machine is always warm-restartable.
  const fingerprintMatches = isClone || Boolean(
    incomingFingerprint
      && workspace.snapshotFingerprint
      && incomingFingerprint === workspace.snapshotFingerprint,
  );
  // A mismatch triggers the expensive destroy+recreate path, so name the
  // reason: which side is missing, or that both exist and differ. Prefixes
  // only — fingerprints hash credentials-adjacent inputs.
  if (!isClone && !fingerprintMatches) {
    console.info(
      `workspace ${workspace.id}: fingerprint mismatch (stored ${workspace.snapshotFingerprint?.slice(0, 8) ?? "none"}, incoming ${incomingFingerprint?.slice(0, 8) ?? "none"}) — recreate path`,
    );
  }
  // The recreate decision has several SILENT inputs beyond the fingerprint
  // (machine lookup returned null, machine in an unexpected state, workspace
  // marked failed, no snapshot key). When the expensive path is about to be
  // taken anyway, name the full condition set once — chasing a spurious
  // rebuild without this took hours of log archaeology.
  const explainRecreate = () =>
    `machine=${machine ? `${machine.id}:${machine.state}` : "null"} recordedMachineId=${workspace.machineId ?? "none"} fingerprintMatches=${fingerprintMatches} snapshotKey=${workspace.snapshotKey ? "yes" : "no"} status=${workspace.status}`;

  // Path 2: warm restart — machine exists but is stopped, and the user's
  // local state hasn't changed (fingerprint matches). With a persistent
  // Fly Volume mounted at /workspace, the supervisor's alreadyStaged()
  // marker survives stop+start so the supervisor skips re-staging entirely
  // (~1-2s total). Without a volume, the marker is on the ephemeral rootfs
  // and the supervisor re-downloads using a fresh URL via the snapshot-url
  // endpoint (~10-30s). Either way, no destroy+recreate is needed.
  // A machine caught mid-"stopping" (the supervisor just exited) is seconds
  // from being warm-restartable; without this wait it missed both reuse paths
  // and fell through to a full destroy+recreate. Poll briefly for it to
  // settle — if it doesn't, the recreate fallthrough still applies.
  if (machine?.id && machine.state === "stopping") {
    const machineId = machine.id;
    for (let attempt = 0; attempt < 15 && machine?.state === "stopping"; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
      machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
        { method: "GET" },
      ).catch(() => machine);
    }
  }

  if (
    machine
    && machine.id
    && (machine.state === "stopped" || machine.state === "suspended")
    && fingerprintMatches
    && (workspace.snapshotKey || isClone)
    && workspace.status !== "failed"
  ) {
    const restarted = await warmRestartWorkspaceMachine({
      machineId: machine.id,
      snapshotKey: workspace.snapshotKey ?? "",
    }).catch((error) => {
      console.warn(`warm restart failed for ${workspace.id}: ${error instanceof Error ? error.message : String(error)}`);
      return null;
    });
    if (restarted) {
      const now = new Date().toISOString();
      const started = workspaceStartResult(restarted.state);
      updateWorkspaceMachine.run({
        id: workspace.id,
        status: started.status,
        machineId: restarted.id ?? machine.id,
        machineState: started.machineState,
        updatedAt: now,
      });
      updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
      const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
      return { ...(next ? rowToWorkspace(next) : workspace), isNew: false } as unknown as JsonRecord;
    }
    // Warm restart failed (env update error, etc.) — fall through to
    // destroy+recreate so the user always gets a working machine.
  }

  // Path 3: must (re)create a machine. Either no machine, machine is gone,
  // or fingerprint mismatch (user's local state changed). Need a fresh
  // snapshot from the CLI. Also destroy + recreate the volume so the user
  // sees their new repo state instead of the cached one on the old disk.
  //
  // REFUSE BEFORE DESTROYING ANYTHING. Recreating a snapshot workspace needs
  // a snapshot in hand; the CLI's first attach is a cheap probe without one.
  // This check used to live after the machine and volume destroys, so the
  // probe demolished a healthy stopped machine, THEN 400'd "snapshot is
  // required" — turning every warm reuse into a 40s full rebuild (and, had
  // the CLI died between probe and re-upload, stranding the workspace with
  // no machine at all). Validate first; destroy only when rebuild can follow.
  if (!isClone && !snapshotInput) {
    console.info(`workspace ${workspace.id}: probe reached recreate path (${explainRecreate()})`);
    throw badRequest("snapshot is required to (re)create this workspace");
  }
  console.info(`workspace ${workspace.id}: recreating (${explainRecreate()})`);
  // `machine` is null both when the machine is genuinely gone AND when the GET
  // above simply failed, because that lookup swallows its error. Keying the
  // delete off it meant one flaky Fly read left the machine alive, still holding
  // the volume, and the recreate died on "volume is currently bound to machine".
  // A DELETE against a machine that no longer exists is a 404 this tolerates.
  if (workspace.machineId && machine?.state !== "destroyed" && machine?.state !== "destroying") {
    await destroyFlyMachine(workspace.machineId);
  }
  // Clone workspaces recreate by re-cloning from origin — no snapshot upload.
  // Keep a surviving volume: the staged clone on it short-circuits re-staging.
  if (isClone) {
    return await recreateCloneWorkspaceMachine(workspace, repoName);
  }
  // A Fly volume cannot be deleted while its machine is attached. Delete the
  // machine first, then wait for detachment; if cleanup still fails, keep the
  // old volume id in SQLite and abort instead of creating an untracked volume.
  if (workspace.volumeId) {
    await destroyWorkspaceVolume(workspace.volumeId);
  }
  if (!snapshotInput) {
    throw badRequest("snapshot is required to (re)create this workspace");
  }
  const snapshotKey = (await storeSnapshot(workspace.accountId, body)).key;
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const now = new Date().toISOString();
  const region = sanitizeRegion(stringField(body, "region")) ?? workspace.region ?? null;
  const regionToUse = region ?? flyRegion;
  let newVolumeId: string | undefined;
  try {
    const freshVolume = await createWorkspaceVolume(workspace.id, regionToUse);
    newVolumeId = freshVolume.id;
  } catch (error) {
    // Volume name may still be reserved if the prior delete is still
    // settling on Fly's side. Retry once with a timestamp suffix.
    const message = error instanceof Error ? error.message : String(error);
    console.warn(`recreate volume for ${workspace.id} failed (${message}); retrying with suffix`);
    const suffixed = await createWorkspaceVolumeNamed(
      workspaceVolumeName(workspace.id, String(Date.now()).slice(-6)),
      regionToUse,
    );
    newVolumeId = suffixed.id;
  }
  updateWorkspaceVolume.run({
    id: workspace.id,
    volumeId: newVolumeId ?? null,
    region: regionToUse,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceSnapshot.run({ id: workspace.id, snapshotKey, snapshotFingerprint: incomingFingerprint, updatedAt: now });
  updateWorkspaceWorkerToken.run({ id: workspace.id, workerTokenHash: tokenHash(workerToken), updatedAt: now });
  const snapshotUrl = await signedSnapshotUrl(snapshotKey);
  const fresh = await createFlyWorkspaceMachine({
    workspaceId: workspace.id,
    accountId: workspace.accountId,
    snapshotUrl,
    workerToken,
    repoName: repoName ?? workspace.repoName,
    region: regionToUse,
    volumeId: newVolumeId,
  });
  const started = workspaceStartResult(fresh.state);
  updateWorkspaceMachine.run({
    id: workspace.id,
    status: started.status,
    machineId: fresh.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : workspace), isNew: true } as unknown as JsonRecord;
}

async function recreateCloneWorkspaceMachine(
  workspace: Workspace,
  repoName: string | undefined,
): Promise<JsonRecord> {
  const now = new Date().toISOString();
  const regionToUse = workspace.region ?? flyRegion;
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  updateWorkspaceWorkerToken.run({ id: workspace.id, workerTokenHash: tokenHash(workerToken), updatedAt: now });
  const createMachine = (volumeId: string | undefined) => createFlyWorkspaceMachine({
    workspaceId: workspace.id,
    accountId: workspace.accountId,
    workerToken,
    repoName: repoName ?? workspace.repoName,
    region: regionToUse,
    volumeId,
    gitRemote: workspace.repoUrl,
    gitRef: workspace.gitRef,
  });
  let fresh: FlyMachine;
  try {
    fresh = await createMachine(workspace.volumeId);
  } catch (error) {
    // The recorded volume may itself be gone (destroyed out-of-band). Create a
    // replacement and retry once; the worker re-clones onto the fresh disk.
    const message = error instanceof Error ? error.message : String(error);
    console.warn(`clone machine create for ${workspace.id} failed (${message}); retrying with a fresh volume`);
    const volume = await createWorkspaceVolumeNamed(
      workspaceVolumeName(workspace.id, String(Date.now()).slice(-6)),
      regionToUse,
    );
    updateWorkspaceVolume.run({
      id: workspace.id,
      volumeId: volume.id ?? null,
      region: regionToUse,
      updatedAt: new Date().toISOString(),
    });
    fresh = await createMachine(volume.id);
  }
  const started = workspaceStartResult(fresh.state);
  updateWorkspaceMachine.run({
    id: workspace.id,
    status: started.status,
    machineId: fresh.id ?? null,
    machineState: started.machineState,
    updatedAt: new Date().toISOString(),
  });
  updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return { ...(next ? rowToWorkspace(next) : workspace), isNew: true } as unknown as JsonRecord;
}

const FLY_REGIONS = new Set([
  "ams","arn","atl","bog","bom","bos","cdg","den","dfw","ewr","eze","fra","gdl",
  "gig","gru","hkg","iad","jnb","lax","lhr","mad","maa","mia","nrt","ord","otp",
  "phx","qro","scl","sea","sin","sjc","syd","waw","yul","yyz",
]);

function sanitizeRegion(value: string | undefined): string | null {
  if (!value) return null;
  const lower = value.trim().toLowerCase();
  return FLY_REGIONS.has(lower) ? lower : null;
}

async function warmRestartWorkspaceMachine(params: {
  machineId: string;
  snapshotKey: string;
}): Promise<FlyMachine | null> {
  ensureFlyConfigured();
  // Refresh the image while preserving the complete existing machine config,
  // including env and mounts. Persistent workspaces otherwise stay pinned to
  // the worker image digest they were originally created with forever.
  void params.snapshotKey;
  try {
    await refreshWorkspaceMachineImage(params.machineId);
    return await startFlyMachine(params.machineId, `workspace ${params.machineId}`);
  } catch (error) {
    console.warn(`warm restart ${params.machineId}: start failed: ${error instanceof Error ? error.message : String(error)}`);
    throw error;
  }
}

async function refreshWorkspaceMachineImage(machineId: string): Promise<void> {
  const machine = await flyRequest<FlyMachine>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    { method: "GET" },
  );
  if (!machine.config) {
    return;
  }
  // Re-submit even when the configured tag text is unchanged. Fly resolves a
  // mutable tag such as `latest` when the machine config is updated, not when
  // an already-created machine is merely started.
  await flyRequest<FlyMachine>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    {
      method: "POST",
      body: { config: { ...machine.config, image: flyWorkerImage } },
    },
  );
}

async function startFlyMachine(machineId: string, label: string): Promise<FlyMachine> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= 12; attempt += 1) {
    try {
      const started = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}/start`,
        { method: "POST", body: {} },
      );
      // Fly's start endpoint responds with {previous_state, migrated, ...} —
      // NO id and NO state. Callers persist `.id` into the workspace row; when
      // this returned the raw start response, every create/recreate stored
      // machineId=NULL, so the next attach couldn't find the machine and did
      // a full ~40s destroy+rebuild instead of an instant reuse. The row only
      // healed when the supervisor's heartbeat reported the id minutes later.
      // Always return an object that carries the id and a usable state.
      return {
        ...started,
        id: started?.id ?? machineId,
        state: started?.state ?? "starting",
      };
    } catch (error) {
      lastError = error;
      const machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
        { method: "GET" },
      ).catch(() => null);
      if (machine?.state === "started" || machine?.state === "starting") {
        return machine;
      }
      if (attempt < 12) {
        await sleep(Math.min(5000, attempt * 2000));
      }
    }
  }
  const message = lastError instanceof Error ? lastError.message : String(lastError);
  throw new Error(`Fly start failed for ${label}: ${message}`);
}

function workspaceStartResult(state: string | undefined): { status: WorkspaceStatus; machineState: string } {
  if (state === "failed") {
    return { status: "failed", machineState: "failed" };
  }
  if (state === "started" || state === "starting" || state === "running") {
    return { status: "running", machineState: state };
  }
  // Fly's start endpoint can return the pre-start machine document (`created`)
  // even though the start request was accepted and the worker connects moments
  // later. Treat a successful start request as running/starting so a later
  // stale Fly response never overwrites a worker heartbeat back to queued.
  return { status: "running", machineState: "starting" };
}

async function createFlyWorkspaceMachine(params: CreateWorkspaceMachineParams): Promise<FlyMachine> {
  try {
    return await createFlyWorkspaceMachineInner(params);
  } catch (error) {
    recordStartFailure("workspace", params.workspaceId, error);
    throw error;
  }
}

type CreateWorkspaceMachineParams = {
  workspaceId: string;
  accountId: string;
  snapshotUrl?: string;
  workerToken: string;
  repoName?: string;
  region?: string;
  volumeId?: string;
  gitRemote?: string;
  gitRef?: string;
  // Home machines are sized and named differently and carry extra env.
  guest?: { cpu_kind: string; cpus: number; memory_mb: number };
  env?: Record<string, string>;
  namePrefix?: string;
};

async function createFlyWorkspaceMachineInner(params: CreateWorkspaceMachineParams): Promise<FlyMachine> {
  ensureFlyConfigured();
  const config: JsonRecord = {
    image: flyWorkerImage,
    env: {
      RUDDER_WORKSPACE_ID: params.workspaceId,
      RUDDER_ACCOUNT_ID: params.accountId,
      RUDDER_CLOUD_URL: baseURL,
      RUDDER_WORKER_TOKEN: params.workerToken,
      // Clone-mode machines get the (public) remote/ref here; the git token
      // itself arrives via the vault at boot, never via Fly machine config.
      ...(params.snapshotUrl ? { RUDDER_SNAPSHOT_URL: params.snapshotUrl } : {}),
      ...(params.gitRemote ? { RUDDER_GIT_REMOTE: params.gitRemote } : {}),
      ...(params.gitRef ? { RUDDER_GIT_REF: params.gitRef } : {}),
      RUDDER_REPO_NAME: params.repoName || "",
      ...(params.env ?? {}),
    },
    guest: params.guest ?? {
      cpu_kind: flyWorkerCpuKind,
      cpus: flyWorkerCpus,
      memory_mb: flyWorkerMemoryMb,
    },
    restart: { policy: "no" },
    auto_destroy: false,
  };
  if (params.volumeId) {
    config.mounts = [{ volume: params.volumeId, path: "/workspace" }];
  }
  const machine = await flyRequest<FlyMachine>(`/v1/apps/${encodeURIComponent(flyAppName)}/machines`, {
    method: "POST",
    body: {
      name: flyMachineName(params.namePrefix ?? "rudder-ws", params.workspaceId),
      region: params.region || flyRegion,
      config,
    },
  });
  if (!machine.id) {
    return machine;
  }
  return await startFlyMachine(machine.id, `workspace ${params.workspaceId}`);
}

type FlyVolume = {
  id?: string;
  name?: string;
  region?: string;
  state?: string;
};

function workspaceVolumeName(workspaceId: string, suffix?: string): string {
  // Fly volume names allow only alphanumeric + underscore, max 30 chars per
  // the API. Replace any other char (e.g. "-") with "_" and truncate.
  const base = `rdr_${workspaceId.replace(/[^a-zA-Z0-9]/g, "_")}`;
  if (!suffix) {
    return base.slice(0, 30);
  }
  const tail = `_${suffix}`;
  const maxBase = 30 - tail.length;
  return `${base.slice(0, maxBase)}${tail}`;
}

function flyMachineName(prefix: string, id: string): string {
  const suffix = randomBytes(3).toString("hex");
  const base = `${prefix}-${id}`
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .replace(/-+/g, "-");
  const maxBaseLength = 63 - suffix.length - 1;
  const safeBase = (base || prefix).slice(0, maxBaseLength).replace(/-+$/g, "");
  return `${safeBase || "rudder"}-${suffix}`;
}

async function createWorkspaceVolume(workspaceId: string, region: string): Promise<FlyVolume> {
  return await createWorkspaceVolumeNamed(workspaceVolumeName(workspaceId), region);
}

async function createWorkspaceVolumeNamed(name: string, region: string, sizeGb = flyWorkspaceVolumeGb): Promise<FlyVolume> {
  ensureFlyConfigured();
  return await flyRequest<FlyVolume>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/volumes`,
    {
      method: "POST",
      body: {
        name,
        region,
        size_gb: sizeGb,
      },
    },
  );
}

/** Force-destroy a Fly machine. A machine that is already gone counts as done. */
async function destroyFlyMachine(machineId: string): Promise<void> {
  try {
    await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}?force=true`,
      { method: "DELETE" },
    );
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!/not found|404/i.test(message)) {
      throw error;
    }
  }
}

/**
 * The machine Fly says is holding a volume, read out of its own refusal:
 * "failed_precondition: volume is currently bound to machine: 80e966df155e08".
 */
function machineHoldingVolume(message: string): string | null {
  return /bound to machine:?\s*([0-9a-z]+)/i.exec(message)?.[1] ?? null;
}

async function destroyWorkspaceVolume(volumeId: string): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    throw new Error("Fly is not configured; cannot destroy workspace volume");
  }
  let lastError: unknown;
  // Detaching after a machine destroy is not instant, so this retries rather
  // than failing on the first refusal.
  const freed = new Set<string>();
  for (let attempt = 1; attempt <= 12; attempt += 1) {
    try {
      await flyRequest<FlyVolume>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/volumes/${encodeURIComponent(volumeId)}`,
        { method: "DELETE" },
      );
      return;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (/not found|404/i.test(message)) {
        return;
      }
      lastError = error;
      // A volume still bound to a machine will refuse forever, so retrying the
      // same DELETE just burns the budget and reports a precondition the caller
      // cannot act on. Fly names the machine in the refusal: destroy THAT one.
      //
      // It is not always the machine this workspace has on record. The recreate
      // path only deletes `workspace.machineId`, and it skips even that when the
      // machine GET happened to fail, so an orphan from an earlier half-finished
      // recreate can sit on the volume with nothing pointing at it.
      const holder = machineHoldingVolume(message);
      if (holder && !freed.has(holder)) {
        freed.add(holder);
        console.warn(
          `volume ${volumeId} is held by machine ${holder}; destroying it to release the volume`,
        );
        try {
          await destroyFlyMachine(holder);
        } catch (destroyError) {
          const detail =
            destroyError instanceof Error ? destroyError.message : String(destroyError);
          throw new Error(
            `destroy volume ${volumeId} failed: it is bound to machine ${holder}, which could not be destroyed: ${detail}`,
          );
        }
      }
      if (attempt < 12) {
        await sleep(1_000);
      }
    }
  }
  throw new Error(`destroy volume ${volumeId} failed: ${lastError instanceof Error ? lastError.message : String(lastError)}`);
}

async function mutateWorkspace(workspace: Workspace, action: string): Promise<Workspace> {
  if (workspace.sourceKind === "home-repo") {
    // A repo on the home machine has no machine of its own. Resuming it starts
    // the home machine; stopping it closes just that repo's dashboard.
    if (action === "resume") {
      await ensureHomeRunning(workspace.accountId);
      pushHomeRepos(workspace.homeId ?? "");
    } else if (workspace.homeId) {
      await homeRpc(workspace.homeId, "close", { workspaceId: workspace.id }).catch(() => null);
    }
    return rowToWorkspace(findWorkspaceById.get(workspace.id));
  }
  if (!workspace.machineId) {
    throw badRequest("workspace does not have a Fly machine yet");
  }
  let machine: FlyMachine;
  if (action === "stop" || action === "pause") {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(workspace.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGTERM", timeout: "10s" } },
    );
  } else if (action === "resume") {
    await refreshWorkspaceMachineImage(workspace.machineId);
    machine = await startFlyMachine(workspace.machineId, `workspace ${workspace.id}`);
  } else {
    throw badRequest(`unsupported workspace action: ${action}`);
  }
  const started = workspaceStartResult(machine.state);
  const status = action === "stop" || action === "pause"
    ? "stopped" as WorkspaceStatus
    : started.status;
  const now = new Date().toISOString();
  updateWorkspaceMachine.run({
    id: workspace.id,
    status,
    machineId: workspace.machineId,
    machineState: action === "stop" || action === "pause" ? machine.state ?? null : started.machineState,
    updatedAt: now,
  });
  if (action === "resume") {
    updateWorkspaceActivity.run({ id: workspace.id, lastActivityAt: now, updatedAt: now });
  }
  const next = findWorkspaceById.get(workspace.id) as Record<string, unknown> | undefined;
  return next ? rowToWorkspace(next) : { ...workspace, status, machineState: machine.state };
}

async function handleWorkspaceHeartbeat(req: IncomingMessage, res: ServerResponse, workspaceId: string): Promise<void> {
  const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const body = await readJsonBody(req);
  const state = stringField(body, "state");
  const machineId = stringField(body, "machineId");
  const expectedMachineId = optionalString(row.machine_id);
  if (machineId && expectedMachineId && machineId !== expectedMachineId) {
    sendJson(res, 409, { error: "stale workspace machine" });
    return;
  }
  const currentStatus = String(row.status) as WorkspaceStatus;
  if (currentStatus === "deleting") {
    // Gone as far as anyone is concerned: tell the worker so it can exit,
    // and never let its heartbeat bring the row back.
    sendJson(res, 410, { error: "workspace deleted", status: currentStatus });
    return;
  }
  if (currentStatus === "stopped") {
    sendJson(res, 200, { ok: true, status: currentStatus, ignored: true });
    return;
  }
  const status: WorkspaceStatus = state === "failed" ? "failed" : "running";
  const activeAgents = numberField(body, "activeAgents");
  const now = new Date().toISOString();
  if (optionalString(row.source_kind) === "home" && status === "running") {
    setHomeRepoStatus.run({ homeId: workspaceId, status: "running", updatedAt: now });
  }
  updateWorkspaceHeartbeat.run({
    id: workspaceId,
    status,
    machineId: machineId ?? null,
    machineState: state || status,
    failureReason: status === "failed" ? heartbeatFailureReason(body) : null,
    activeAgents: activeAgents ?? 0,
    lastHeartbeatAt: now,
    updatedAt: now,
  });
  sendJson(res, 200, { ok: true, status });
}

async function handleWorkspaceSnapshotUrl(req: IncomingMessage, res: ServerResponse, workspaceId: string): Promise<void> {
  const row = findWorkspaceById.get(workspaceId) as Record<string, unknown> | undefined;
  if (!row) {
    sendJson(res, 404, { error: "workspace not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const snapshotKey = optionalString(row.snapshot_key);
  if (!snapshotKey) {
    sendJson(res, 400, { error: "workspace has no snapshot" });
    return;
  }
  const signed = await signedSnapshotUrl(snapshotKey);
  sendJson(res, 200, { url: signed, expiresInSeconds: 3600 });
}

function listAccountWorkspaces(accountId: string): Workspace[] {
  return (listWorkspacesForAccount.all(accountId) as unknown[]).map(rowToWorkspace);
}

function annotateWorkspaceClients(workspace: Workspace): Workspace & { clientCount: number } {
  const channel = workspaceChannels.get(workspace.id);
  const clientCount = channel
    ? Array.from(channel.clients).filter(isOpen).length
    : 0;
  return { ...workspace, clientCount };
}

function getAccountWorkspace(id: string, accountId: string): Workspace | null {
  const row = findWorkspaceForAccount.get(id, accountId);
  return row ? rowToWorkspace(row) : null;
}

function rowToWorkspace(row: unknown): Workspace {
  const value = row as Record<string, unknown>;
  return {
    id: String(value.id),
    accountId: String(value.account_id),
    workspaceKey: String(value.workspace_key),
    repoName: optionalString(value.repo_name),
    status: String(value.status) as WorkspaceStatus,
    machineId: optionalString(value.machine_id),
    machineState: optionalString(value.machine_state),
    snapshotKey: optionalString(value.snapshot_key),
    snapshotFingerprint: optionalString(value.snapshot_fingerprint),
    region: optionalString(value.region),
    volumeId: optionalString(value.volume_id),
    sourceKind: (optionalString(value.source_kind) as WorkspaceSourceKind | undefined) ?? "snapshot",
    repoUrl: optionalString(value.repo_url),
    gitRef: optionalString(value.git_ref),
    homeId: optionalString(value.home_id),
    failureReason: optionalString(value.failure_reason),
    pinnedAt: optionalString(value.pinned_at),
    lastActivityAt: optionalString(value.last_activity_at),
    lastHeartbeatAt: optionalString(value.last_heartbeat_at),
    activeAgents: Number.isFinite(Number(value.active_agents)) && value.active_agents !== null
      ? Math.max(0, Math.floor(Number(value.active_agents)))
      : 0,
    createdAt: String(value.created_at),
    updatedAt: String(value.updated_at),
  };
}

function flyStateToWorkspaceStatus(state: string | undefined): WorkspaceStatus {
  switch (state) {
    case "started":
    case "starting":
      return "running";
    case "stopped":
    case "stopping":
    case "suspended":
      return "stopped";
    case "destroyed":
      return "stopped";
    case "failed":
      return "failed";
    default:
      return "queued";
  }
}

function uniqueWorkspaceId(repoName: string | undefined): string {
  const base = slugForWorkspaceId(repoName) || `workspace`;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    const candidate = attempt === 0
      ? `${base}-${randomBytes(2).toString("hex")}`
      : `${base}-${randomBytes(3).toString("hex")}`;
    if (!findWorkspaceById.get(candidate)) {
      return candidate;
    }
  }
  return `workspace-${randomBytes(5).toString("hex")}`;
}

function slugForWorkspaceId(value: string | undefined): string {
  const slug = (value || "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 24)
    .replace(/-+$/g, "");
  return slug || "";
}

async function flyRequest<T>(pathname: string, init: { method: string; body?: JsonRecord }): Promise<T> {
  ensureFlyConfigured();
  const headers: Record<string, string> = {
    Authorization: `Bearer ${flyApiToken}`,
    Accept: "application/json",
  };
  let body: string | undefined;
  if (init.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(init.body);
  }
  const response = await fetch(`${flyApiBase}${pathname}`, {
    method: init.method,
    headers,
    body,
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw new Error(responseErrorMessage(parsed) ?? text.trim() ?? `Fly API ${response.status}`);
  }
  return parsed as T;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function githubUser(token: string): Promise<{ id: number | string; login: string; email?: string }> {
  const response = await fetch("https://api.github.com/user", {
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/vnd.github+json",
      "User-Agent": "rudder-cloud",
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
  const text = await response.text();
  const parsed = text ? parseJson(text) : null;
  if (!response.ok) {
    throw unauthorized();
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw unauthorized();
  }
  const id = typeof parsed.id === "number" || typeof parsed.id === "string" ? parsed.id : undefined;
  const login = typeof parsed.login === "string" ? parsed.login : undefined;
  if (!id || !login) {
    throw unauthorized();
  }
  // SECURITY: the public profile `email` is user-mutable and may be unverified, so it
  // must never drive authorization (it gates admin via adminEmails). Use the
  // GitHub-verified primary email instead; undefined when none is verified.
  const email = await githubVerifiedPrimaryEmail(token);
  return { id, login, email };
}

async function githubVerifiedPrimaryEmail(token: string): Promise<string | undefined> {
  try {
    const response = await fetch("https://api.github.com/user/emails", {
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        "User-Agent": "rudder-cloud",
        "X-GitHub-Api-Version": "2022-11-28",
      },
    });
    if (!response.ok) {
      return undefined;
    }
    const list = (await response.json()) as unknown;
    if (!Array.isArray(list)) {
      return undefined;
    }
    const primary = list.find(
      (entry): entry is { email: string } =>
        Boolean(entry) &&
        typeof entry === "object" &&
        (entry as { primary?: unknown }).primary === true &&
        (entry as { verified?: unknown }).verified === true &&
        typeof (entry as { email?: unknown }).email === "string",
    );
    return primary?.email.toLowerCase();
  } catch {
    return undefined;
  }
}

// --------------------------------------------------------------------------
// Slack: the shared channel is the "main panel" for every cloud instance.
// --------------------------------------------------------------------------

async function postToSlack(text: string, threadTs?: string): Promise<string | undefined> {
  if (!slack.enabled) {
    return undefined;
  }
  const result = await postSlackMessage({
    botToken: slack.botToken,
    channel: slack.channel,
    text,
    threadTs,
  });
  if (!result.ok) {
    console.warn(`slack post failed: ${result.error}`);
    return undefined;
  }
  return result.ts;
}

// Open a thread for a freshly launched instance and remember its root ts so
// later replies route back to this instance.
async function announceSailToSlack(
  id: string,
  task: string | undefined,
  repoName: string | undefined,
  runtime: SailRuntime,
): Promise<void> {
  if (!slack.enabled) {
    return;
  }
  const lines = [
    `🚀 *${id}* launched on ${runtime === "fly" ? "Fly" : "your VM"}.`,
    repoName ? `repo: \`${repoName}\`` : "",
    task ? `> ${task}` : "",
    "_Reply in this thread to talk to the agent._",
  ].filter(Boolean);
  const ts = await postToSlack(lines.join("\n"));
  if (ts) {
    updateSailSlackThread.run({ id, threadTs: ts, updatedAt: new Date().toISOString() });
  }
}

// After injecting input, the agent needs a moment to react. Read the output
// tail a few seconds later and post it back into the instance's thread.
function scheduleOutputEcho(sailId: string, threadTs: string | undefined, delayMs = 4500): void {
  setTimeout(() => {
    const tail = readChannelOutput("sail", sailId);
    if (tail.trim()) {
      void postToSlack(formatOutputForSlack(tail), threadTs);
    }
  }, delayMs).unref();
}

// Default body cap for unauthenticated/early-parsed routes. Snapshots (base64
// tarballs) are larger, so authenticated upload routes pass a bigger explicit cap.
const DEFAULT_MAX_BODY_BYTES = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BODY_BYTES = 256 * 1024 * 1024;

async function readRawBody(req: IncomingMessage, maxBytes = DEFAULT_MAX_BODY_BYTES): Promise<string> {
  return (await readBodyBytes(req, maxBytes)).toString("utf8");
}

async function readBodyBytes(req: IncomingMessage, maxBytes: number): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      req.destroy();
      throw payloadTooLarge();
    }
    chunks.push(buf);
  }
  return Buffer.concat(chunks);
}

async function handleSlackEvents(req: IncomingMessage, res: ServerResponse): Promise<void> {
  // Slack events are tiny; cap hard so an unauthenticated POST can't exhaust memory.
  const raw = await readRawBody(req, 256 * 1024);
  // Fail closed: never process an inbound Slack event without verifying its
  // signature. An unset signing secret means Slack is misconfigured, not open.
  if (!slack.signingSecret) {
    sendJson(res, 401, { error: "slack not configured" });
    return;
  }
  const ok = verifySlackSignature({
    signingSecret: slack.signingSecret,
    timestamp: req.headers["x-slack-request-timestamp"] as string | undefined,
    signature: req.headers["x-slack-signature"] as string | undefined,
    rawBody: raw,
  });
  if (!ok) {
    sendJson(res, 401, { error: "bad signature" });
    return;
  }
  let body: Record<string, unknown>;
  try {
    body = JSON.parse(raw) as Record<string, unknown>;
  } catch {
    sendJson(res, 400, { error: "invalid json" });
    return;
  }

  if (body.type === "url_verification") {
    sendJson(res, 200, { challenge: typeof body.challenge === "string" ? body.challenge : "" });
    return;
  }

  // Slack expects a 200 within 3s; do the real work afterwards.
  sendJson(res, 200, { ok: true });

  if (body.type !== "event_callback" || !slack.enabled) {
    return;
  }
  const eventId = typeof body.event_id === "string" ? body.event_id : "";
  if (eventId) {
    if (seenSlackEvents.has(eventId)) {
      return;
    }
    seenSlackEvents.add(eventId);
    if (seenSlackEvents.size > 2000) {
      seenSlackEvents.clear();
    }
  }
  const event = body.event as Record<string, unknown> | undefined;
  if (!event) {
    return;
  }
  processSlackEvent(event).catch((error) => {
    console.error("slack event error", error);
  });
}

// Newest snapshot stored for a repo (sails and workspaces both count) — the base a
// Slack `launch` boots from. Returns the owning account so the new sail lands in
// the same account that uploaded the snapshot.
function latestSnapshotForRepo(repoName: string): { key: string; accountId: string } | null {
  const rows = [
    latestSailSnapshotForRepo.get(repoName),
    latestWorkspaceSnapshotForRepo.get(repoName),
  ].filter((row): row is Record<string, unknown> => Boolean(row));
  rows.sort((a, b) => String(b.updated_at || "").localeCompare(String(a.updated_at || "")));
  const newest = rows[0];
  if (!newest) {
    return null;
  }
  return { key: String(newest.snapshot_key), accountId: String(newest.account_id) };
}

function listSnapshotRepoSummaries(): { repo: string; updatedAt: string }[] {
  const merged = new Map<string, string>();
  const rows = [
    ...(listSailSnapshotRepos.all() as Record<string, unknown>[]),
    ...(listWorkspaceSnapshotRepos.all() as Record<string, unknown>[]),
  ];
  for (const row of rows) {
    const repo = String(row.repo_name || "");
    const at = String(row.updated_at || "");
    if (!repo) {
      continue;
    }
    const prev = merged.get(repo);
    if (!prev || at > prev) {
      merged.set(repo, at);
    }
  }
  return [...merged.entries()]
    .map(([repo, updatedAt]) => ({ repo, updatedAt }))
    .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
}

// Messages sent from Slack to a sleeping instance: the machine is woken and the
// message is delivered once its worker WS reconnects. Bounded per sail so a sail
// that never comes back cannot hoard memory.
const pendingSlackInputs = new Map<string, { message: string; threadTs?: string }[]>();

function queuePendingSlackInput(sailId: string, message: string, threadTs?: string): void {
  const queue = pendingSlackInputs.get(sailId) ?? [];
  queue.push({ message, threadTs });
  while (queue.length > 20) {
    queue.shift();
  }
  pendingSlackInputs.set(sailId, queue);
}

function flushPendingSlackInputs(sailId: string): void {
  const queue = pendingSlackInputs.get(sailId);
  if (!queue || queue.length === 0) {
    return;
  }
  pendingSlackInputs.delete(sailId);
  // Give the resumed agent PTY a beat to be ready before typing into it.
  setTimeout(() => {
    for (const item of queue) {
      const delivered = sendInputToChannel("sail", sailId, item.message, true);
      if (delivered) {
        scheduleOutputEcho(sailId, item.threadTs);
      } else if (item.threadTs) {
        void postToSlack(
          `*${sailId}* reconnected but did not accept the queued message; send it again.`,
          item.threadTs,
        );
      }
    }
  }, 3000);
}

async function processSlackEvent(event: Record<string, unknown>): Promise<void> {
  const type = String(event.type || "");
  // Ignore anything the bot itself posted, and edits/joins/etc.
  if (event.bot_id || event.subtype) {
    return;
  }
  if (type !== "app_mention" && type !== "message") {
    return;
  }
  const text = typeof event.text === "string" ? event.text : "";
  const ts = typeof event.ts === "string" ? event.ts : undefined;
  const threadTs = typeof event.thread_ts === "string" ? event.thread_ts : undefined;
  const inThread = Boolean(threadTs);

  // Plain channel chatter is not for us — only act on @mentions and on replies
  // inside a thread we own.
  const threadSail = threadTs
    ? (findSailBySlackThread.get(threadTs) as Record<string, unknown> | undefined)
    : undefined;
  if (type === "message" && !threadSail) {
    return;
  }

  const command = parseSlackCommand(text, { inThread });
  const replyThread = threadTs ?? ts;

  // The channel is shared across accounts, so authorize the sender before any command
  // that reads output or controls an instance. Without an allowlist, fail closed.
  if (command.action !== "help") {
    const sender = typeof event.user === "string" ? event.user : "";
    if (!sender || !slackAllowedUsers.has(sender)) {
      await postToSlack(
        "You're not authorized to control cloud instances from Slack. Ask an admin to add your Slack user ID to RUDDER_SLACK_ALLOWED_USERS.",
        replyThread,
      );
      return;
    }
  }

  switch (command.action) {
    case "help":
      await postToSlack(SLACK_HELP_TEXT, replyThread);
      return;
    case "list": {
      const sails = listRunningSails.all().map(rowToSail);
      if (sails.length === 0) {
        await postToSlack("No running cloud instances. Start one with `rudder cloud \"<task>\"`.", replyThread);
        return;
      }
      const lines = sails.map((s) => {
        const live = instanceHasLiveWorker("sail", s.id) ? "🟢" : "⚪️";
        return `${live} *${s.id}* — ${s.status}${s.task ? ` · ${s.task}` : ""}`;
      });
      await postToSlack(`*Cloud instances*\n${lines.join("\n")}`, replyThread);
      return;
    }
    case "output": {
      const tail = readChannelOutput("sail", command.id);
      await postToSlack(
        instanceHasLiveWorker("sail", command.id)
          ? formatOutputForSlack(tail)
          : `*${command.id}* is not connected.`,
        replyThread,
      );
      return;
    }
    case "stop": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      if (!row) {
        await postToSlack(`No instance \`${command.id}\`.`, replyThread);
        return;
      }
      const accountId = String(row.account_id);
      const sail = getAccountSail(command.id, accountId);
      if (sail) {
        await mutateSail(sail, accountId, "stop").catch(() => undefined);
      }
      await postToSlack(`🛑 Stopping *${command.id}*.`, replyThread);
      return;
    }
    case "pause":
    case "resume": {
      const row = findSailById.get(command.id) as Record<string, unknown> | undefined;
      const accountId = row ? String(row.account_id) : "";
      const sail = accountId ? getAccountSail(command.id, accountId) : null;
      if (!sail) {
        await postToSlack(`No instance \`${command.id}\`.`, replyThread);
        return;
      }
      try {
        await mutateSail(sail, accountId, command.action);
        await postToSlack(
          command.action === "pause" ? `⏸️ Paused *${command.id}*.` : `▶️ Waking *${command.id}*.`,
          replyThread,
        );
      } catch (error) {
        await postToSlack(
          `${command.action} failed: ${error instanceof Error ? error.message : String(error)}`,
          replyThread,
        );
      }
      return;
    }
    case "repos": {
      const repos = listSnapshotRepoSummaries();
      if (repos.length === 0) {
        await postToSlack(
          "No repo snapshots yet. Run `rudder cloud` from a repo once to upload one.",
          replyThread,
        );
        return;
      }
      const lines = repos
        .slice(0, 20)
        .map((r) => `• *${r.repo}* — snapshot from ${r.updatedAt.slice(0, 10) || "unknown"}`);
      await postToSlack(
        `*Launchable repos*\n${lines.join("\n")}\nStart one with \`launch <repo> <task>\`.`,
        replyThread,
      );
      return;
    }
    case "launch": {
      const found = latestSnapshotForRepo(command.repo);
      if (!found) {
        await postToSlack(
          `No snapshot for *${command.repo}*. Run \`rudder cloud\` from that repo once, or say \`repos\` to see what's launchable.`,
          replyThread,
        );
        return;
      }
      try {
        const sail = await createSail(
          found.accountId,
          { runtime: "fly", task: command.task, repoName: command.repo },
          undefined,
          found.key,
        );
        // createSail already announces the new instance as its own thread root;
        // this reply just closes the loop on the command itself.
        await postToSlack(
          `🚀 Launching *${sail.id}* on *${command.repo}* — ${command.task}`,
          replyThread,
        );
      } catch (error) {
        await postToSlack(
          `Launch failed: ${error instanceof Error ? error.message : String(error)}`,
          replyThread,
        );
      }
      return;
    }
    case "talk":
    case "thread-reply": {
      const targetId = command.action === "talk" ? command.id : String(threadSail?.id || "");
      const message = command.message;
      if (!targetId) {
        await postToSlack("Which instance? Use `talk <id> <message>` or reply in an instance thread.", replyThread);
        return;
      }
      if (!message) {
        return;
      }
      // Route the echo into the instance's own thread when we know it.
      const sailRow = findSailById.get(targetId) as Record<string, unknown> | undefined;
      const ownThread = optionalString(sailRow?.slack_thread_ts) ?? replyThread;
      const delivered = sendInputToChannel("sail", targetId, message, true);
      if (!delivered) {
        // The instance is asleep (idle-suspended) or its worker dropped. For a
        // live-able Fly sail, wake the machine and deliver the message once its
        // worker reconnects — this is what makes 24/7 Slack control feel always-on.
        const accountId = sailRow ? String(sailRow.account_id) : "";
        const sail = accountId ? getAccountSail(targetId, accountId) : null;
        const wakeable = sail?.runtime === "fly"
          && Boolean(sail.machineId)
          && sail.status !== "completed"
          && sail.status !== "failed";
        if (sail && wakeable) {
          queuePendingSlackInput(targetId, message, ownThread);
          try {
            await mutateSail(sail, accountId, "resume");
            await postToSlack(
              `⏰ *${targetId}* was asleep — waking it; your message will be delivered when it reconnects.`,
              replyThread,
            );
          } catch (error) {
            pendingSlackInputs.delete(targetId);
            await postToSlack(
              `*${targetId}* is not connected and could not be woken: ${error instanceof Error ? error.message : String(error)}`,
              replyThread,
            );
          }
          return;
        }
        await postToSlack(`*${targetId}* is not connected right now.`, replyThread);
        return;
      }
      scheduleOutputEcho(targetId, ownThread);
      return;
    }
  }
}

function listAccountSails(accountId: string): Sail[] {
  return listSailsForAccount.all(accountId).map(rowToSail);
}

function getAccountSail(id: string, accountId: string): Sail | null {
  const row = findSail.get(id, accountId);
  return row ? rowToSail(row) : null;
}

function rowToSail(row: unknown): Sail {
  const value = row as Record<string, unknown>;
  return {
    id: String(value.id),
    status: String(value.status) as SailStatus,
    runtime: sailRuntimeValue(optionalString(value.runtime)),
    repoName: optionalString(value.repo_name),
    task: optionalString(value.task),
    branch: optionalString(value.branch),
    machineId: optionalString(value.machine_id),
    machineState: optionalString(value.machine_state),
    snapshotKey: optionalString(value.snapshot_key),
    lastHeartbeatAt: optionalString(value.last_heartbeat_at),
    slackThreadTs: optionalString(value.slack_thread_ts),
    failureReason: optionalString(value.failure_reason),
    createdAt: String(value.created_at),
    updatedAt: String(value.updated_at),
  };
}

function sailRuntimeFromBody(body: Json): SailRuntime {
  const worker = objectField(body, "worker");
  const raw = stringField(body, "runtime") || stringField(worker, "type") || stringField(worker, "runtime");
  return sailRuntimeValue(raw);
}

function sailRuntimeValue(raw: string | undefined): SailRuntime {
  const value = (raw || "fly").trim().toLowerCase();
  if (value === "fly" || value === "fly-machine" || value === "fly-machines") {
    return "fly";
  }
  if (value === "byo" || value === "byoc" || value === "byo-vm" || value === "manual" || value === "self-hosted" || value === "vm") {
    return "byo-vm";
  }
  throw badRequest(`unsupported cloud runtime: ${raw}`);
}

function flyStateToSailStatus(state: string | undefined): SailStatus {
  switch (state) {
    case "started":
    case "starting":
      return "running";
    case "suspended":
    case "stopped":
    case "stopping":
      return "paused";
    case "destroyed":
      return "completed";
    case "failed":
      return "failed";
    default:
      return "queued";
  }
}

function requireBearer(req: IncomingMessage): { accountId: string; email?: string } {
  const authHeader = req.headers.authorization || "";
  const token = authHeader.startsWith("Bearer ") ? authHeader.slice("Bearer ".length).trim() : "";
  if (!token.startsWith("rdr_")) {
    throw unauthorized();
  }
  const hash = tokenHash(token);
  const row = findToken.get(hash) as Record<string, unknown> | undefined;
  if (!row) {
    throw unauthorized();
  }
  touchToken.run(new Date().toISOString(), hash);
  return {
    accountId: String(row.account_id),
    email: optionalString(row.email),
  };
}

function requireAdmin(authContext: { email?: string }): void {
  const email = authContext.email?.toLowerCase();
  if (!email || !adminEmails.has(email)) {
    throw unauthorized();
  }
}

function requireWorkerBearer(req: IncomingMessage, sailRow: Record<string, unknown>): void {
  const authHeader = req.headers.authorization || "";
  const token = authHeader.startsWith("Bearer ") ? authHeader.slice("Bearer ".length).trim() : "";
  const expected = optionalString(sailRow.worker_token_hash);
  if (!token.startsWith("rdrw_") || !expected || tokenHash(token) !== expected) {
    throw unauthorized();
  }
}

function createBetterAuth() {
  return betterAuth({
    baseURL: authBaseURL,
    secret: requiredEnv("BETTER_AUTH_SECRET"),
    database,
    socialProviders: socialProviders(),
    // One person, several providers: a Google sign-in with the same verified
    // email as an existing GitHub account is the SAME Rudder account (and so
    // sees the same home machine), not a fresh empty one.
    account: {
      accountLinking: {
        enabled: true,
        trustedProviders: ["google", "github"],
      },
    },
  });
}

function refreshAuthHandler(force = false): void {
  const nextFingerprint = providerFingerprint();
  if (!force && nextFingerprint === authProviderFingerprint) {
    return;
  }
  authProviderFingerprint = nextFingerprint;
  auth = createBetterAuth();
  authHandler = toNodeHandler(auth.handler);
}

function providerFingerprint(): string {
  return JSON.stringify(configuredProviders());
}

function configuredProviders(): JsonRecord {
  return {
    google: Boolean(oauthValue("GOOGLE_CLIENT_ID", "google_client_id") && oauthValue("GOOGLE_CLIENT_SECRET", "google_client_secret")),
    github: Boolean(oauthValue("GITHUB_CLIENT_ID", "github_client_id") && oauthValue("GITHUB_CLIENT_SECRET", "github_client_secret")),
    githubDevice: Boolean(githubDeviceClientId),
  };
}

function socialProviders(): JsonRecord {
  const providers: JsonRecord = {};
  const googleClientId = oauthValue("GOOGLE_CLIENT_ID", "google_client_id");
  const googleClientSecret = oauthValue("GOOGLE_CLIENT_SECRET", "google_client_secret");
  const githubClientId = oauthValue("GITHUB_CLIENT_ID", "github_client_id");
  const githubClientSecret = oauthValue("GITHUB_CLIENT_SECRET", "github_client_secret");
  if (googleClientId && googleClientSecret) {
    providers.google = {
      clientId: googleClientId,
      clientSecret: googleClientSecret,
    };
  }
  if (githubClientId && githubClientSecret) {
    providers.github = {
      clientId: githubClientId,
      clientSecret: githubClientSecret,
    };
  }
  return providers;
}

function oauthValue(envName: string, settingKey: string): string | undefined {
  return process.env[envName] || settingValue(settingKey);
}

function settingValue(key: string): string | undefined {
  const row = getSetting.get(key) as Record<string, unknown> | undefined;
  return typeof row?.value === "string" && row.value.length > 0 ? row.value : undefined;
}

function setSetting(key: string, value: string): void {
  upsertSetting.run({ key, value, updatedAt: new Date().toISOString() });
}

function ensureColumn(table: string, column: string, definition: string): void {
  const rows = database.prepare(`pragma table_info(${table})`).all() as Array<{ name?: string }>;
  if (rows.some((row) => row.name === column)) {
    return;
  }
  database.prepare(`alter table ${table} add column ${column} ${definition}`).run();
}

function renderHome(res: ServerResponse): void {
  sendHtml(res, renderShell({
    title: "Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Rudder Cloud.</h1>
        <p class="lede">The control plane that lets Rudder hand off coding-agent runs to managed cloud workers. Sign in to connect a laptop.</p>
        <div class="card">
          <a class="provider" href="/cli/login"><span class="icon" aria-hidden="true">${BRAND_SVG}</span><span>Sign in to Rudder Cloud</span></a>
          <div class="device">Or run <code>rudder login</code> from the CLI to open this page with a device code attached.</div>
        </div>
      </section>
    `,
  }));
}

function renderSuccessPage(res: ServerResponse, email?: string): void {
  sendHtml(res, renderShell({
    title: "Signed in · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>You're in.</h1>
        <p class="lede">${email ? `Signed in as <strong>${escapeHtml(email)}</strong>. ` : ""}You can close this tab. The Rudder CLI will pick up the session in a moment.</p>
        <div class="card">
          <span class="pill">Logged in</span>
          <p class="muted" style="margin-top:14px">Try it next:</p>
          <p><code class="kbd">rudder cloud list</code> &middot; <code class="kbd">rudder sail "fix the failing tests"</code></p>
        </div>
      </section>
    `,
  }));
}

function renderExpiredPage(res: ServerResponse, status = 400): void {
  sendHtml(res, renderShell({
    title: "Login expired · Rudder Cloud",
    body: `
      <section class="hero">
        <h1>Session expired.</h1>
        <p class="lede">This login link has timed out.</p>
        <div class="card">
          <p class="muted" style="margin-top:0">Run the command again to start a fresh session:</p>
          <p><code class="kbd">rudder login</code></p>
        </div>
      </section>
    `,
  }), status);
}

async function readJsonBody(req: IncomingMessage, maxBytes = DEFAULT_MAX_BODY_BYTES): Promise<Json> {
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > maxBytes) {
      req.destroy();
      throw payloadTooLarge();
    }
    chunks.push(buf);
  }
  if (chunks.length === 0) {
    return {};
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
}

function objectField(value: Json | undefined, field: string): JsonRecord | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const next = value[field];
  return next && typeof next === "object" && !Array.isArray(next) ? next : undefined;
}

function stringField(value: Json | undefined, field: string): string | undefined {
  return value && typeof value === "object" && !Array.isArray(value) && typeof value[field] === "string"
    ? value[field]
    : undefined;
}
function numberField(value: Json | undefined, field: string): number | undefined {
  const raw = value && typeof value === "object" && !Array.isArray(value) ? value[field] : undefined;
  const num = typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN;
  return Number.isFinite(num) && num >= 0 ? Math.floor(num) : undefined;
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function sendJson(res: ServerResponse, status: number, body: Json): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function sendHtml(res: ServerResponse, body: string, status = 200): void {
  res.writeHead(status, { "content-type": "text/html; charset=utf-8" });
  res.end(body);
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function imageWithTag(image: string, tag: string): string {
  if (image.includes("@")) {
    return image;
  }
  const slashIndex = image.lastIndexOf("/");
  const tagIndex = image.lastIndexOf(":");
  if (tagIndex > slashIndex) {
    return `${image.slice(0, tagIndex + 1)}${tag}`;
  }
  return `${image}:${tag}`;
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

async function getBetterAuthSession(req: IncomingMessage): Promise<{ user?: { id?: string; email?: string } } | null> {
  try {
    const headers = new Headers();
    for (const [key, value] of Object.entries(req.headers)) {
      if (Array.isArray(value)) {
        headers.set(key, value.join(", "));
      } else if (value) {
        headers.set(key, value);
      }
    }
    return await (auth.api as unknown as {
      getSession(input: { headers: Headers }): Promise<{ user?: { id?: string; email?: string } } | null>;
    }).getSession({ headers });
  } catch {
    return null;
  }
}

function parseJson(text: string): Json {
  try {
    return JSON.parse(text) as Json;
  } catch {
    return text;
  }
}

function responseErrorMessage(value: Json | null): string | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  return typeof value.error === "string"
    ? value.error
    : typeof value.message === "string"
      ? value.message
      : undefined;
}

function appendHeader(existing: number | string | string[] | undefined, value: string): string[] {
  if (Array.isArray(existing)) {
    return [...existing, value];
  }
  if (typeof existing === "string") {
    return [existing, value];
  }
  if (typeof existing === "number") {
    return [String(existing), value];
  }
  return [value];
}

function splitSetCookieHeader(value: string | null): string[] {
  if (!value) {
    return [];
  }
  return value.split(/,(?=\s*[^;,]+=)/).map((cookie) => cookie.trim()).filter(Boolean);
}

function ensureCloudRuntimeConfigured(runtime: SailRuntime): void {
  ensureS3Configured();
  if (runtime === "fly") {
    ensureFlyConfigured();
  }
}

function ensureS3Configured(): void {
  if (!snapshotBucket) {
    throw new Error("RUDDER_S3_BUCKET is required for cloud snapshots");
  }
}

function ensureFlyConfigured(): void {
  if (!flyApiToken) {
    throw new Error("FLY_API_TOKEN is required to create Fly Machines");
  }
  if (!flyAppName) {
    throw new Error("FLY_APP_NAME is required to create Fly Machines");
  }
  if (!flyWorkerImage) {
    throw new Error("RUDDER_WORKER_IMAGE is required to create Fly Machines");
  }
}

function badRequest(message: string): Error {
  const error = new Error(message);
  (error as Error & { status?: number }).status = 400;
  return error;
}

function unauthorized(): Error {
  const error = new Error("unauthorized");
  (error as Error & { status?: number }).status = 401;
  return error;
}

function payloadTooLarge(): Error {
  const error = new Error("payload too large");
  (error as Error & { status?: number }).status = 413;
  return error;
}

function requiredEnv(name: string, fallback?: string): string {
  const value = process.env[name] || fallback;
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function tokenHash(token: string): string {
  return createHash("sha256").update(token).digest("hex").slice(0, 32);
}

// ---------------------------------------------------------------------------
// Home machine: one long-lived Fly machine per account holding every repo.
//
// The home row (source_kind "home") owns the machine and its volume. Each repo
// opened on it is a "home-repo" row: it owns no machine, shares the home's
// worker token, and keeps the ordinary workspace PTY channel so `rudder cloud`
// attaches to its dashboard exactly like a per-repo workspace. The supervisor
// on the machine holds one CONTROL socket (/api/rudder/home/<id>/control) that
// carries the repo list down, project state up, and RPCs both ways; the phone
// app is served from that state and those RPCs.
// ---------------------------------------------------------------------------

const HOME_WORKSPACE_KEY = "home";
const homeCpuKind = process.env.RUDDER_HOME_CPU_KIND || "performance";
const homeCpus = Number(process.env.RUDDER_HOME_CPUS || 4);
const homeMemoryMb = Number(process.env.RUDDER_HOME_MEMORY_MB || 16384);
const homeVolumeGb = Number(process.env.RUDDER_HOME_VOLUME_GB || 50);
const homeIdleMs = Number(process.env.RUDDER_HOME_IDLE_MS || 2 * 60 * 60 * 1000);
const HOME_RPC_TIMEOUT_MS = 20_000;

type HomeAgent = JsonRecord;
type HomeProject = {
  workspaceId: string;
  repo: string;
  name: string;
  path?: string;
  dashboard?: string;
  error?: string;
  agents?: HomeAgent[];
  ports?: JsonRecord[];
};
type HomeState = {
  projects: HomeProject[];
  tailnet?: JsonRecord;
  credentials?: JsonRecord;
  logins?: JsonRecord;
  updatedAt: string;
};
type PendingRpc = {
  resolve: (value: Json) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
};

const homeControls = new Map<string, WebSocket>();
const homeStates = new Map<string, HomeState>();
const homeRpcs = new Map<string, PendingRpc>();
const homeStarts = new Map<string, Promise<Workspace>>();

const listHomeRepoRows = database.prepare(
  "select * from rudder_workspaces where home_id = ? and source_kind = 'home-repo' and status <> 'deleting' order by repo_name",
);
const setHomeRepoPinned = database.prepare(
  "update rudder_workspaces set pinned_at = @pinnedAt where id = @id and account_id = @accountId",
);
const setHomeRepoStatus = database.prepare(`
  update rudder_workspaces set status = @status, updated_at = @updatedAt
  where home_id = @homeId and source_kind = 'home-repo' and status not in ('deleting')
`);
const setHomeRepoTokens = database.prepare(`
  update rudder_workspaces set worker_token_hash = @workerTokenHash, updated_at = @updatedAt
  where home_id = @homeId and source_kind = 'home-repo'
`);
const setRepoFailure = database.prepare(
  "update rudder_workspaces set failure_reason = ?, updated_at = ? where id = ? and source_kind = 'home-repo'",
);

function findHomeRow(accountId: string): Workspace | null {
  const row = findWorkspaceByKey.get(accountId, HOME_WORKSPACE_KEY) as Record<string, unknown> | undefined;
  return row ? rowToWorkspace(row) : null;
}

function homeRepoKey(slug: string): string {
  return `home:${slug.toLowerCase()}`;
}

function homeRepos(homeId: string): Workspace[] {
  return (listHomeRepoRows.all(homeId) as Record<string, unknown>[]).map(rowToWorkspace);
}

// The home machine is the expensive thing, so starting it is single-flight per
// account: the CLI, the phone and a second terminal can all ask at once.
async function ensureHomeRunning(accountId: string, region?: string | null): Promise<Workspace> {
  // A supervisor holding its control socket is proof the machine is up: no
  // need to ask Fly.
  const known = findHomeRow(accountId);
  if (known && known.status === "running" && isOpen(homeControls.get(known.id))) {
    return known;
  }
  const inFlight = homeStarts.get(accountId);
  if (inFlight) {
    return await inFlight;
  }
  const promise = startHome(accountId, region).finally(() => homeStarts.delete(accountId));
  homeStarts.set(accountId, promise);
  return await promise;
}

async function startHome(accountId: string, region?: string | null): Promise<Workspace> {
  ensureCloudRuntimeConfigured("fly");
  const existing = findHomeRow(accountId);
  if (!existing) {
    return await createHome(accountId, region ?? null);
  }
  if (existing.status === "deleting") {
    throw badRequest("the home machine is being deleted; try again in a minute");
  }
  let machine: FlyMachine | null = null;
  if (existing.machineId) {
    machine = await flyRequest<FlyMachine>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(existing.machineId)}`,
      { method: "GET" },
    ).catch(() => null);
  }
  const now = new Date().toISOString();
  if (machine && (machine.state === "started" || machine.state === "starting")) {
    updateWorkspaceMachine.run({ id: existing.id, status: "running", machineId: existing.machineId ?? null, machineState: machine.state ?? null, updatedAt: now });
    setHomeRepoStatus.run({ homeId: existing.id, status: "running", updatedAt: now });
    updateWorkspaceActivity.run({ id: existing.id, lastActivityAt: now, updatedAt: now });
    return rowToWorkspace(findWorkspaceById.get(existing.id));
  }
  if (machine?.id && machine.state !== "destroyed" && machine.state !== "destroying") {
    // Wait out a machine that is mid-stop: starting it then fails.
    for (let attempt = 0; attempt < 15 && machine?.state === "stopping"; attempt += 1) {
      await sleep(1000);
      machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machine.id!)}`,
        { method: "GET" },
      ).catch(() => machine);
    }
    try {
      const started = await warmRestartWorkspaceMachine({ machineId: machine!.id!, snapshotKey: "" });
      const result = workspaceStartResult(started?.state);
      updateWorkspaceMachine.run({ id: existing.id, status: result.status, machineId: started?.id ?? machine!.id!, machineState: result.machineState, updatedAt: now });
      setHomeRepoStatus.run({ homeId: existing.id, status: result.status, updatedAt: now });
      updateWorkspaceActivity.run({ id: existing.id, lastActivityAt: now, updatedAt: now });
      return rowToWorkspace(findWorkspaceById.get(existing.id));
    } catch (error) {
      recordStartFailure("workspace", existing.id, error);
      throw error;
    }
  }
  // The machine is gone (destroyed out of band, or never created): make a new
  // one on the same volume. A new machine means a new worker token, which every
  // repo row shares.
  return await recreateHomeMachine(existing);
}

function homeMachineParams(home: { id: string; accountId: string }, workerToken: string, region: string, volumeId?: string): CreateWorkspaceMachineParams {
  return {
    workspaceId: home.id,
    accountId: home.accountId,
    workerToken,
    repoName: "home",
    region,
    volumeId,
    guest: { cpu_kind: homeCpuKind, cpus: homeCpus, memory_mb: homeMemoryMb },
    env: { RUDDER_HOME: "1" },
    namePrefix: "rudder-home",
  };
}

async function createHome(accountId: string, region: string | null): Promise<Workspace> {
  const now = new Date().toISOString();
  const id = uniqueWorkspaceId("home");
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  insertWorkspace.run({
    id,
    accountId,
    workspaceKey: HOME_WORKSPACE_KEY,
    repoName: "home",
    status: "queued",
    machineId: null,
    machineState: null,
    snapshotKey: null,
    snapshotFingerprint: null,
    region,
    volumeId: null,
    workerTokenHash: tokenHash(workerToken),
    sourceKind: "home",
    repoUrl: null,
    gitRef: null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  const regionToUse = region ?? flyRegion;
  try {
    const volume = await createWorkspaceVolumeNamed(workspaceVolumeName(id), regionToUse, homeVolumeGb);
    updateWorkspaceVolume.run({ id, volumeId: volume.id ?? null, region: regionToUse, updatedAt: new Date().toISOString() });
    const machine = await createFlyWorkspaceMachine(homeMachineParams({ id, accountId }, workerToken, regionToUse, volume.id));
    const started = workspaceStartResult(machine.state);
    updateWorkspaceMachine.run({ id, status: started.status, machineId: machine.id ?? null, machineState: started.machineState, updatedAt: new Date().toISOString() });
  } catch (error) {
    recordStartFailure("workspace", id, error);
    throw error;
  }
  return rowToWorkspace(findWorkspaceById.get(id));
}

type HomeRegionMove = {
  from: string;
  to: string;
  state: "starting" | "stopping" | "copying" | "starting-machine" | "done" | "failed";
  startedAt: string;
  oldVolumeId?: string;
  newVolumeId?: string;
  error?: string;
};
const homeRegionMoves = new Map<string, HomeRegionMove>();

async function moveHomeRegion(home: Workspace, move: HomeRegionMove): Promise<void> {
  ensureFlyConfigured();
  if ((home.region ?? flyRegion) === move.to) {
    move.state = "done";
    return;
  }
  if (!home.volumeId) {
    throw new Error("the home machine has no volume to move");
  }
  move.oldVolumeId = home.volumeId;
  move.state = "stopping";
  if (home.machineId) {
    await flyRequest(
      `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(home.machineId)}/stop`,
      { method: "POST", body: { signal: "SIGTERM", timeout: "20s" } },
    ).catch(() => undefined);
    for (let attempt = 0; attempt < 60; attempt += 1) {
      const machine = await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(home.machineId)}`,
        { method: "GET" },
      ).catch(() => null);
      if (!machine || machine.state === "stopped" || machine.state === "destroyed") {
        break;
      }
      await sleep(2000);
    }
  }
  move.state = "copying";
  const fork = await flyRequest<FlyVolume>(`/v1/apps/${encodeURIComponent(flyAppName)}/volumes`, {
    method: "POST",
    body: {
      name: workspaceVolumeName(home.id, move.to),
      region: move.to,
      size_gb: homeVolumeGb,
      source_volume_id: home.volumeId,
    },
  });
  move.newVolumeId = fork.id;
  // A cross-region fork copies in the background; a machine can attach once
  // the volume is created (Fly hydrates it), but wait for that state.
  for (let attempt = 0; attempt < 180; attempt += 1) {
    const volume = await flyRequest<FlyVolume & { state?: string }>(
      `/v1/apps/${encodeURIComponent(flyAppName)}/volumes/${encodeURIComponent(fork.id ?? "")}`,
      { method: "GET" },
    ).catch(() => null);
    if (volume?.state === "created") {
      break;
    }
    await sleep(5000);
  }
  move.state = "starting-machine";
  if (home.machineId) {
    await destroyFlyMachine(home.machineId);
  }
  const now = new Date().toISOString();
  updateWorkspaceVolume.run({ id: home.id, volumeId: fork.id ?? null, region: move.to, updatedAt: now });
  closeHomeControl(home.id);
  await recreateHomeMachine(rowToWorkspace(findWorkspaceById.get(home.id)));
  move.state = "done";
  console.log(`home ${home.id} moved ${move.from} -> ${move.to}; old volume ${move.oldVolumeId} kept as a backup`);
}

function closeHomeControl(homeId: string): void {
  const control = homeControls.get(homeId);
  if (control) {
    homeControls.delete(homeId);
    try { control.close(); } catch { /* ignore */ }
  }
}

// ---------------------------------------------------------------------------
// Update when idle. A release pins a new worker image, but the home machine
// keeps the image it booted with, and restarting it cuts off any agent mid-turn.
// So the control plane waits for a quiet moment: nothing working or waiting on
// a prompt (a restart would drop the hook holding it), and nobody typing, for
// HOME_IDLE_UPDATE_MS. Then the machine is updated in place (same volume and
// config, new image); the boot reopen brings every agent back where it was.
// ---------------------------------------------------------------------------
const HOME_IDLE_UPDATE_MS = Number(process.env.RUDDER_HOME_IDLE_UPDATE_MS || 5 * 60 * 1000);
const homeBusyAt = new Map<string, number>();
const homeImageChecked = new Map<string, { at: number; image: string | null }>();
const homeUpdating = new Map<string, number>();
const listHomeRows = database.prepare("select * from rudder_workspaces where source_kind = 'home' and status <> 'deleting'");

function homeAgentsBusy(projects: HomeProject[]): boolean {
  return projects.some((project) => (project.agents ?? []).some((agent) => {
    const status = typeof agent.status === "string" ? agent.status : "";
    const asks = Array.isArray(agent.asks) ? agent.asks.length : 0;
    return status === "working" || status === "starting" || asks > 0;
  }));
}

function noteHomeBusy(homeId: string, projects: HomeProject[]): void {
  if (homeAgentsBusy(projects) || !homeBusyAt.has(homeId)) {
    homeBusyAt.set(homeId, Date.now());
  }
}

async function homeMachineImage(machineId: string): Promise<{ image: string | null; config: JsonRecord | null }> {
  const machine = await flyRequest<FlyMachine & { config?: JsonRecord }>(
    `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(machineId)}`,
    { method: "GET" },
  );
  const config = machine?.config && typeof machine.config === "object" ? machine.config : null;
  return { image: typeof config?.image === "string" ? config.image : null, config };
}

async function sweepIdleHomeUpdates(): Promise<void> {
  if (!flyApiToken || !flyAppName) {
    return;
  }
  const now = Date.now();
  for (const raw of listHomeRows.all() as Record<string, unknown>[]) {
    const home = rowToWorkspace(raw);
    if (!home.machineId || !isOpen(homeControls.get(home.id))) {
      continue;
    }
    const state = homeStates.get(home.id);
    const fresh = state && now - Date.parse(state.updatedAt) < 60_000;
    const quietSince = Math.max(homeBusyAt.get(home.id) ?? now, Date.parse(home.lastActivityAt ?? "") || 0);
    if (!fresh || now - quietSince < HOME_IDLE_UPDATE_MS || now - (homeUpdating.get(home.id) ?? 0) < 15 * 60_000) {
      continue;
    }
    const checked = homeImageChecked.get(home.id);
    if (checked && now - checked.at < 5 * 60_000 && checked.image === flyWorkerImage) {
      continue;
    }
    try {
      const { image, config } = await homeMachineImage(home.machineId);
      homeImageChecked.set(home.id, { at: now, image });
      if (!config || !image || image === flyWorkerImage) {
        continue;
      }
      homeUpdating.set(home.id, now);
      console.log(`[home] ${home.id} idle on ${image}; updating to ${flyWorkerImage}`);
      await flyRequest<FlyMachine>(
        `/v1/apps/${encodeURIComponent(flyAppName)}/machines/${encodeURIComponent(home.machineId)}`,
        { method: "POST", body: { config: { ...config, image: flyWorkerImage } } },
      );
      homeImageChecked.set(home.id, { at: now, image: flyWorkerImage });
    } catch (error) {
      console.log(`[home] ${home.id} idle update failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
}

const homeUpdateSweepMs = Number(process.env.RUDDER_HOME_UPDATE_SWEEP_MS || 60 * 1000);
if (homeUpdateSweepMs >= 1000) {
  setInterval(() => void sweepIdleHomeUpdates().catch(() => undefined), homeUpdateSweepMs).unref?.();
}

async function recreateHomeMachine(home: Workspace): Promise<Workspace> {
  const now = new Date().toISOString();
  const workerToken = `rdrw_${randomBytes(32).toString("base64url")}`;
  const hash = tokenHash(workerToken);
  updateWorkspaceWorkerToken.run({ id: home.id, workerTokenHash: hash, updatedAt: now });
  setHomeRepoTokens.run({ homeId: home.id, workerTokenHash: hash, updatedAt: now });
  const regionToUse = home.region ?? flyRegion;
  let volumeId = home.volumeId;
  if (!volumeId) {
    const volume = await createWorkspaceVolumeNamed(workspaceVolumeName(home.id, String(Date.now()).slice(-6)), regionToUse, homeVolumeGb);
    volumeId = volume.id;
    updateWorkspaceVolume.run({ id: home.id, volumeId: volumeId ?? null, region: regionToUse, updatedAt: now });
  }
  const machine = await createFlyWorkspaceMachine(homeMachineParams(home, workerToken, regionToUse, volumeId));
  const started = workspaceStartResult(machine.state);
  updateWorkspaceMachine.run({ id: home.id, status: started.status, machineId: machine.id ?? null, machineState: started.machineState, updatedAt: new Date().toISOString() });
  setHomeRepoStatus.run({ homeId: home.id, status: started.status, updatedAt: now });
  updateWorkspaceActivity.run({ id: home.id, lastActivityAt: now, updatedAt: now });
  return rowToWorkspace(findWorkspaceById.get(home.id));
}

// Directory name for a repo under ~/code on the home machine. Two repos with
// the same name from different owners get the owner as a prefix.
function homeRepoDirName(homeId: string, slug: string): string {
  const [owner, name] = slug.split("/");
  const taken = homeRepos(homeId).some((row) => row.repoName === name && row.repoUrl !== `https://github.com/${slug}.git`);
  return taken ? `${owner}-${name}` : name;
}

function ensureHomeRepoRow(home: Workspace, slug: string, branch?: string): Workspace {
  const key = homeRepoKey(slug);
  const existing = findWorkspaceByKey.get(home.accountId, key) as Record<string, unknown> | undefined;
  const now = new Date().toISOString();
  const homeRow = findWorkspaceById.get(home.id) as Record<string, unknown>;
  if (existing) {
    const row = rowToWorkspace(existing);
    if (branch && branch !== row.gitRef) {
      database.prepare("update rudder_workspaces set git_ref = ?, updated_at = ? where id = ?").run(branch, now, row.id);
    }
    database.prepare("update rudder_workspaces set status = ?, worker_token_hash = ?, home_id = ?, updated_at = ? where id = ?")
      .run(home.status, optionalString(homeRow.worker_token_hash) ?? null, home.id, now, row.id);
    return rowToWorkspace(findWorkspaceById.get(row.id));
  }
  const name = homeRepoDirName(home.id, slug);
  const id = uniqueWorkspaceId(name);
  insertWorkspace.run({
    id,
    accountId: home.accountId,
    workspaceKey: key,
    repoName: name,
    status: home.status,
    machineId: null,
    machineState: null,
    snapshotKey: null,
    snapshotFingerprint: null,
    region: home.region ?? null,
    volumeId: null,
    workerTokenHash: optionalString(homeRow.worker_token_hash) ?? null,
    sourceKind: "home-repo",
    repoUrl: `https://github.com/${slug}.git`,
    gitRef: branch ?? null,
    lastActivityAt: now,
    lastHeartbeatAt: null,
    createdAt: now,
    updatedAt: now,
  });
  database.prepare("update rudder_workspaces set home_id = ? where id = ?").run(home.id, id);
  return rowToWorkspace(findWorkspaceById.get(id));
}

function repoSlugFromUrl(url: string | undefined): string {
  const match = (url ?? "").match(/github\.com\/([\w.-]+\/[\w.-]+?)(?:\.git)?$/);
  return match ? match[1] : "";
}

function homeRepoDescriptor(row: Workspace): JsonRecord {
  return {
    workspaceId: row.id,
    name: row.repoName ?? "repo",
    repo: repoSlugFromUrl(row.repoUrl),
    remote: row.repoUrl ?? "",
    ref: row.gitRef ?? null,
  };
}

function pushHomeRepos(homeId: string): void {
  const control = homeControls.get(homeId);
  if (!isOpen(control)) {
    return;
  }
  sendSocket(control, JSON.stringify({ type: "repos", repos: homeRepos(homeId).map(homeRepoDescriptor) }));
}

async function openRepoOnHome(accountId: string, body: Json): Promise<JsonRecord> {
  const slug = (stringField(body, "repo") ?? "").trim().replace(/\.git$/, "");
  if (!REPO_SLUG_RE.test(slug)) {
    throw badRequest("repo must look like owner/name");
  }
  const branch = stringField(body, "branch")?.trim() || undefined;
  const region = sanitizeRegion(stringField(body, "region"));
  const home = await ensureHomeRunning(accountId, region);
  const repo = ensureHomeRepoRow(home, slug, branch);
  pushHomeRepos(home.id);
  schedulePersistDatabase();
  return { home: home as unknown as JsonRecord, workspace: repo as unknown as JsonRecord };
}

function handleHomeControlUpgrade(req: IncomingMessage, socket: Duplex, head: Buffer, homeId: string): void {
  const row = findWorkspaceById.get(homeId) as Record<string, unknown> | undefined;
  if (!row || optionalString(row.source_kind) !== "home") {
    destroyUpgrade(socket, 404);
    return;
  }
  if (String(row.status) === "deleting") {
    destroyUpgrade(socket, 410);
    return;
  }
  try {
    requireWorkerBearer(req, row);
  } catch {
    destroyUpgrade(socket, 401);
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => attachHomeControl(homeId, ws));
}

function attachHomeControl(homeId: string, ws: WebSocket): void {
  const previous = homeControls.get(homeId);
  if (previous && previous !== ws) {
    try { previous.close(4000, "replaced"); } catch { /* ignore */ }
  }
  homeControls.set(homeId, ws);
  enableSocketLiveness(ws);
  pushHomeRepos(homeId);
  flushPendingHomeDeletes(homeId);
  ws.on("message", (data) => {
    let message: Json;
    try {
      message = JSON.parse(Buffer.isBuffer(data) ? data.toString("utf8") : String(data)) as Json;
    } catch {
      return;
    }
    const type = stringField(message, "type");
    if (type === "state") {
      const projects = (message as JsonRecord).projects;
      noteHomeBusy(homeId, Array.isArray(projects) ? (projects as unknown as HomeProject[]) : []);
      homeStates.set(homeId, {
        projects: Array.isArray(projects) ? (projects as unknown as HomeProject[]) : [],
        tailnet: objectField(message, "tailnet"),
        credentials: objectField(message, "credentials"),
        logins: objectField(message, "logins"),
        updatedAt: new Date().toISOString(),
      });
      for (const project of homeStates.get(homeId)!.projects) {
        if (typeof project?.workspaceId === "string") {
          try {
            setRepoFailure.run(typeof project.error === "string" && project.error ? project.error.slice(0, 2000) : null, new Date().toISOString(), project.workspaceId);
          } catch {
            // best effort
          }
        }
      }
      return;
    }
    if (type === "rpc-result") {
      const id = stringField(message, "id") ?? "";
      const pending = homeRpcs.get(id);
      if (!pending) {
        return;
      }
      homeRpcs.delete(id);
      clearTimeout(pending.timer);
      const error = stringField(message, "error");
      if (error) {
        pending.reject(Object.assign(new Error(error), { status: 400 }));
      } else {
        pending.resolve((message as JsonRecord).result ?? null);
      }
    }
  });
  ws.on("close", () => {
    if (homeControls.get(homeId) === ws) {
      homeControls.delete(homeId);
    }
  });
  ws.on("error", () => undefined);
}

async function homeRpc(homeId: string, method: string, params: JsonRecord, timeoutMs = HOME_RPC_TIMEOUT_MS): Promise<Json> {
  const control = homeControls.get(homeId);
  if (!isOpen(control)) {
    throw Object.assign(new Error("the cloud machine is not connected yet; it may still be starting"), { status: 503 });
  }
  const id = randomUUID();
  return await new Promise<Json>((resolve, reject) => {
    const timer = setTimeout(() => {
      homeRpcs.delete(id);
      reject(Object.assign(new Error(`home machine did not answer ${method} in time`), { status: 504 }));
    }, timeoutMs);
    homeRpcs.set(id, { resolve, reject, timer });
    sendSocket(control, JSON.stringify({ type: "rpc", id, method, params }));
  });
}

// ---------------------------------------------------------------------------
// Moving laptop agents onto the home machine. The CLI uploads one bundle (see
// src/move-home.ts); this job starts the home if needed, waits for its control
// socket, and asks the supervisor to adopt the bundle. The supervisor pulls the
// bytes over HTTP with its worker token instead of through the control socket,
// whose frames stay small. Jobs live in memory: a restart mid-move loses the
// job, the laptop's poll gets a 404, and its agents stay where they are (a
// retried move is safe: the supervisor adopts each run at most once).
// ---------------------------------------------------------------------------

type HomeMove = {
  id: string;
  accountId: string;
  homeId: string | null;
  repo: string;
  branch?: string;
  status: "starting" | "waiting" | "staging" | "done" | "failed";
  detail: string;
  bundle: Buffer | null;
  result: Json | null;
  error: string | null;
  updatedAt: number;
};

const homeMoves = new Map<string, HomeMove>();
const HOME_MOVE_TTL_MS = 30 * 60 * 1000;
const HOME_MOVE_CONTROL_WAIT_MS = 6 * 60 * 1000;
const HOME_MOVE_ADOPT_TIMEOUT_MS = 12 * 60 * 1000;
const HOME_MOVE_PATH_RE = /^\/api\/rudder\/home\/move\/([^/]+)$/;
const HOME_MOVE_BUNDLE_RE = /^\/api\/rudder\/home\/([^/]+)\/moves\/([^/]+)\/bundle$/;

function startHomeMove(accountId: string, body: Json): HomeMove {
  const repo = (stringField(body, "repo") ?? "").trim().replace(/\.git$/, "");
  if (!REPO_SLUG_RE.test(repo)) {
    throw badRequest("repo must look like owner/name");
  }
  const bundle = Buffer.from(stringField(body, "bundle") ?? "", "base64");
  if (bundle.length === 0) {
    throw badRequest("bundle is required");
  }
  const now = Date.now();
  for (const [id, move] of homeMoves) {
    if (now - move.updatedAt > HOME_MOVE_TTL_MS) {
      homeMoves.delete(id);
    }
  }
  const move: HomeMove = {
    id: randomUUID(),
    accountId,
    homeId: null,
    repo,
    branch: stringField(body, "branch")?.trim() || undefined,
    status: "starting",
    detail: "Starting your home machine...",
    bundle,
    result: null,
    error: null,
    updatedAt: now,
  };
  homeMoves.set(move.id, move);
  void runHomeMove(move);
  return move;
}

function setHomeMove(move: HomeMove, status: HomeMove["status"], detail: string): void {
  move.status = status;
  move.detail = detail;
  move.updatedAt = Date.now();
}

async function runHomeMove(move: HomeMove): Promise<void> {
  try {
    const opened = await openRepoOnHome(move.accountId, { repo: move.repo, ...(move.branch ? { branch: move.branch } : {}) });
    const home = opened.home as unknown as Workspace;
    const repo = opened.workspace as unknown as Workspace;
    move.homeId = home.id;
    setHomeMove(move, "waiting", "Waiting for the home machine to come up...");
    const deadline = Date.now() + HOME_MOVE_CONTROL_WAIT_MS;
    while (!isOpen(homeControls.get(home.id))) {
      if (Date.now() > deadline) {
        throw new Error("the home machine did not come up in time");
      }
      await sleep(1000);
    }
    setHomeMove(move, "staging", `Handing the agents to ${move.repo} on the home machine...`);
    move.result = await homeRpc(home.id, "adopt", {
      workspaceId: repo.id,
      moveId: move.id,
      bundlePath: `/api/rudder/home/${encodeURIComponent(home.id)}/moves/${encodeURIComponent(move.id)}/bundle`,
    }, HOME_MOVE_ADOPT_TIMEOUT_MS);
    setHomeMove(move, "done", "");
  } catch (error) {
    move.error = error instanceof Error ? error.message : String(error);
    setHomeMove(move, "failed", "");
  } finally {
    move.bundle = null;
  }
}

// A laptop asking about agents it moved here: which still exist (so a delete
// on the phone removes the laptop's row too), or delete them (so `dd` on the
// laptop removes them here and from the phone). A delete for a machine that is
// asleep is queued and sent the moment its supervisor connects; it wakes the
// machine to do it.
const pendingHomeDeletes = new Map<string, Array<{ workspaceId: string; runIds: string[] }>>();

async function homeAgentsRequest(accountId: string, body: Json): Promise<JsonRecord> {
  const slug = (stringField(body, "repo") ?? "").trim().replace(/\.git$/, "");
  if (!REPO_SLUG_RE.test(slug)) {
    throw badRequest("repo must look like owner/name");
  }
  const action = stringField(body, "action") ?? "";
  const raw = (body as JsonRecord).runIds;
  const runIds = (Array.isArray(raw) ? raw : [])
    .filter((id): id is string => typeof id === "string" && /^[A-Za-z0-9._-]{1,128}$/.test(id))
    .slice(0, 200);
  if (runIds.length === 0) {
    throw badRequest("runIds is required");
  }
  const home = findHomeRow(accountId);
  const repo = home ? homeRepos(home.id).find((row) => repoSlugFromUrl(row.repoUrl)?.toLowerCase() === slug.toLowerCase()) : undefined;
  if (action === "check") {
    if (!home || !repo) {
      // No home, or this repo was closed there: none of its agents exist.
      return { known: true, existing: [] };
    }
    if (!isOpen(homeControls.get(home.id))) {
      return { known: false };
    }
    const result = await homeRpc(home.id, "runs", { workspaceId: repo.id, runIds }) as JsonRecord;
    if (result?.known !== true) {
      return { known: false };
    }
    return { known: true, existing: Array.isArray(result.existing) ? result.existing : [] };
  }
  if (action !== "delete") {
    throw badRequest("action must be check or delete");
  }
  if (!home || !repo) {
    return { status: "deleted" };
  }
  if (isOpen(homeControls.get(home.id))) {
    await homeRpc(home.id, "deleteRuns", { workspaceId: repo.id, runIds });
    return { status: "deleted" };
  }
  const queue = pendingHomeDeletes.get(home.id) ?? [];
  queue.push({ workspaceId: repo.id, runIds });
  pendingHomeDeletes.set(home.id, queue);
  void ensureHomeRunning(accountId).catch(() => undefined);
  return { status: "queued" };
}

function flushPendingHomeDeletes(homeId: string): void {
  const queue = pendingHomeDeletes.get(homeId);
  if (!queue || queue.length === 0) {
    return;
  }
  pendingHomeDeletes.delete(homeId);
  for (const entry of queue) {
    void homeRpc(homeId, "deleteRuns", { workspaceId: entry.workspaceId, runIds: entry.runIds }, HOME_MOVE_ADOPT_TIMEOUT_MS)
      .catch((error) => console.error(`home ${homeId}: queued delete failed`, error));
  }
}

function homeMoveView(move: HomeMove): JsonRecord {
  return {
    id: move.id,
    repo: move.repo,
    status: move.status,
    detail: move.detail || null,
    error: move.error,
    result: move.result,
  };
}

function serveHomeMoveBundle(req: IncomingMessage, res: ServerResponse, homeId: string, moveId: string): void {
  const row = findWorkspaceById.get(homeId) as Record<string, unknown> | undefined;
  if (!row || optionalString(row.source_kind) !== "home") {
    sendJson(res, 404, { error: "not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const move = homeMoves.get(moveId);
  if (!move || move.homeId !== homeId || !move.bundle) {
    sendJson(res, 404, { error: "no such move bundle" });
    return;
  }
  res.writeHead(200, { "content-type": "application/gzip", "content-length": String(move.bundle.length) });
  res.end(move.bundle);
}

// Photos and files sent from the phone. The bytes wait here (in memory, for
// one upload) while the home machine is asked to fetch them with its worker
// token, like a move bundle: never base64 over the control socket.
type HomeUpload = { homeId: string; bytes: Buffer; expiresAt: number };
const homeUploads = new Map<string, HomeUpload>();
const APP_UPLOAD_MAX_BYTES = 25 * 1024 * 1024;
const HOME_UPLOAD_TTL_MS = 5 * 60 * 1000;
const HOME_UPLOAD_RE = /^\/api\/rudder\/home\/([^/]+)\/uploads\/([^/]+)$/;

export function uploadFileName(raw: string): string {
  const base = String(raw || "").split(/[\\/]/).pop() ?? "";
  const cleaned = base.normalize("NFKD").replace(/[^A-Za-z0-9._-]+/g, "-").replace(/-{2,}/g, "-").replace(/^[-.]+/, "");
  const dot = cleaned.lastIndexOf(".");
  const ext = dot > 0 ? cleaned.slice(dot).slice(0, 12) : "";
  const stem = (dot > 0 ? cleaned.slice(0, dot) : cleaned).replace(/[-.]+$/, "").slice(0, 80);
  return `${stem || "file"}${ext}`;
}

async function uploadToHome(homeId: string, workspaceId: string, rawName: string, req: IncomingMessage): Promise<JsonRecord> {
  // Refuse on the declared length before reading: cutting the body off mid-way
  // resets the connection, and the phone would show a network error instead.
  if (Number(req.headers["content-length"]) > APP_UPLOAD_MAX_BYTES) {
    throw Object.assign(new Error("that file is over 25 MB"), { status: 413 });
  }
  let bytes: Buffer;
  try {
    bytes = await readBodyBytes(req, APP_UPLOAD_MAX_BYTES);
  } catch (error) {
    if ((error as { status?: number }).status === 413) {
      throw Object.assign(new Error("that file is over 25 MB"), { status: 413 });
    }
    throw error;
  }
  if (bytes.length === 0) {
    throw badRequest("the file is empty");
  }
  const now = Date.now();
  for (const [id, upload] of homeUploads) {
    if (upload.expiresAt < now) {
      homeUploads.delete(id);
    }
  }
  const id = randomUUID();
  homeUploads.set(id, { homeId, bytes, expiresAt: now + HOME_UPLOAD_TTL_MS });
  try {
    const saved = await homeRpc(homeId, "upload", {
      workspaceId,
      name: uploadFileName(rawName),
      size: bytes.length,
      uploadPath: `/api/rudder/home/${encodeURIComponent(homeId)}/uploads/${encodeURIComponent(id)}`,
    }, 120_000);
    return saved as JsonRecord;
  } finally {
    homeUploads.delete(id);
  }
}

function serveHomeUpload(req: IncomingMessage, res: ServerResponse, homeId: string, uploadId: string): void {
  const row = findWorkspaceById.get(homeId) as Record<string, unknown> | undefined;
  if (!row || optionalString(row.source_kind) !== "home") {
    sendJson(res, 404, { error: "not found" });
    return;
  }
  requireWorkerBearer(req, row);
  const upload = homeUploads.get(uploadId);
  if (!upload || upload.homeId !== homeId) {
    sendJson(res, 404, { error: "no such upload" });
    return;
  }
  res.writeHead(200, { "content-type": "application/octet-stream", "content-length": String(upload.bytes.length) });
  res.end(upload.bytes);
}

function homeSummary(home: Workspace | null): JsonRecord | null {
  if (!home) {
    return null;
  }
  const state = homeStates.get(home.id);
  return {
    id: home.id,
    status: home.status,
    machineState: home.machineState ?? null,
    connected: isOpen(homeControls.get(home.id)),
    failureReason: home.failureReason ?? null,
    activeAgents: home.activeAgents ?? 0,
    lastActivityAt: home.lastActivityAt ?? null,
    tailnet: state?.tailnet ?? null,
    credentials: state?.credentials ?? null,
    logins: state?.logins ?? null,
    stateUpdatedAt: state?.updatedAt ?? null,
  };
}

// Repo rows merged with what the supervisor last reported for each.
function homeProjects(home: Workspace): JsonRecord[] {
  const live = new Map((homeStates.get(home.id)?.projects ?? []).map((project) => [project.workspaceId, project]));
  return homeRepos(home.id).map((row) => {
    const project = live.get(row.id);
    return {
      workspaceId: row.id,
      name: row.repoName ?? "repo",
      repo: repoSlugFromUrl(row.repoUrl),
      ref: row.gitRef ?? null,
      status: row.status,
      failureReason: project?.error ?? row.failureReason ?? null,
      dashboard: project?.dashboard ?? null,
      agents: (project?.agents ?? []) as unknown as Json,
      ports: (project?.ports ?? []) as unknown as Json,
      clients: Array.from(workspaceChannels.get(row.id)?.clients ?? []).filter(isOpen).length,
      pinnedAt: row.pinnedAt ?? null,
      lastActivityAt: row.lastActivityAt ?? null,
    };
  });
}

function requireHomeRepo(accountId: string, workspaceId: string): { home: Workspace; repo: Workspace } {
  const repo = getAccountWorkspace(workspaceId, accountId);
  if (!repo || repo.sourceKind !== "home-repo" || !repo.homeId) {
    throw Object.assign(new Error("project not found"), { status: 404 });
  }
  const home = getAccountWorkspace(repo.homeId, accountId);
  if (!home) {
    throw Object.assign(new Error("home machine not found"), { status: 404 });
  }
  return { home, repo };
}

function touchHome(homeId: string | undefined): void {
  if (homeId) {
    touchWorkspaceActivity(homeId);
  }
}

async function handleHomeApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  // The supervisor fetching a move bundle authenticates as the machine, not
  // as the user, so it is matched before the user-token check.
  const bundleMatch = req.method === "GET" ? HOME_MOVE_BUNDLE_RE.exec(url.pathname) : null;
  if (bundleMatch) {
    serveHomeMoveBundle(req, res, decodeURIComponent(bundleMatch[1]), decodeURIComponent(bundleMatch[2]));
    return;
  }
  const uploadMatch = req.method === "GET" ? HOME_UPLOAD_RE.exec(url.pathname) : null;
  if (uploadMatch) {
    serveHomeUpload(req, res, decodeURIComponent(uploadMatch[1]), decodeURIComponent(uploadMatch[2]));
    return;
  }
  const authContext = requireBearer(req);
  if (req.method === "POST" && url.pathname === "/api/rudder/home/move") {
    const body = await readJsonBody(req, MAX_SNAPSHOT_BODY_BYTES);
    const move = startHomeMove(authContext.accountId, body);
    sendJson(res, 202, { move: homeMoveView(move) });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/home/agents") {
    const body = await readJsonBody(req);
    sendJson(res, 200, await homeAgentsRequest(authContext.accountId, body));
    return;
  }
  const moveMatch = req.method === "GET" ? HOME_MOVE_PATH_RE.exec(url.pathname) : null;
  if (moveMatch) {
    const move = homeMoves.get(decodeURIComponent(moveMatch[1]));
    if (!move || move.accountId !== authContext.accountId) {
      sendJson(res, 404, { error: "no such move (the control plane may have restarted; the agents stayed on the laptop)" });
      return;
    }
    sendJson(res, 200, { move: homeMoveView(move) });
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/rudder/home") {
    const home = findHomeRow(authContext.accountId);
    sendJson(res, 200, { home: homeSummary(home), projects: home ? homeProjects(home) : [] });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/home/open") {
    const body = await readJsonBody(req);
    sendJson(res, 200, await openRepoOnHome(authContext.accountId, body));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/home/sync") {
    // `rudder cloud sync`: each named repo (default: every repo open there)
    // fetches and moves to its newest default branch on the home machine.
    const body = await readJsonBody(req);
    const raw = (body as JsonRecord).repos;
    const wanted = (Array.isArray(raw) ? raw : [])
      .filter((slug): slug is string => typeof slug === "string" && REPO_SLUG_RE.test(slug))
      .map((slug) => slug.toLowerCase());
    const home = await homeWithControl(authContext.accountId);
    const repos = homeRepos(home.id).filter((row) => wanted.length === 0 || wanted.includes(repoSlugFromUrl(row.repoUrl).toLowerCase()));
    const results: JsonRecord[] = [];
    for (const repo of repos) {
      const slug = repoSlugFromUrl(repo.repoUrl);
      try {
        results.push({ repo: slug, report: await homeRpc(home.id, "sync", { workspaceId: repo.id }, 120_000) });
      } catch (error) {
        results.push({ repo: slug, error: error instanceof Error ? error.message : String(error) });
      }
    }
    sendJson(res, 200, { results });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/home/start") {
    const home = await ensureHomeRunning(authContext.accountId);
    sendJson(res, 200, { home: homeSummary(home) });
    return;
  }
  // Move the home machine to another region (closer to the person using it).
  // The disk is COPIED (a Fly volume fork), and the old one is kept as a
  // backup: nothing on it is deleted here. Runs in the background; GET reports.
  if (url.pathname === "/api/rudder/home/region") {
    if (req.method === "GET") {
      sendJson(res, 200, { move: homeRegionMoves.get(authContext.accountId) ?? null });
      return;
    }
    if (req.method === "POST") {
      const body = await readJsonBody(req);
      const region = sanitizeRegion(stringField(body, "region") ?? "");
      if (!region) {
        throw badRequest("region must be a Fly region code like sjc or iad");
      }
      const home = findHomeRow(authContext.accountId);
      if (!home) {
        sendJson(res, 404, { error: "no home machine" });
        return;
      }
      const running = homeRegionMoves.get(authContext.accountId);
      if (running && running.state !== "done" && running.state !== "failed") {
        sendJson(res, 409, { error: "a region move is already running", move: running });
        return;
      }
      const move: HomeRegionMove = { from: home.region ?? flyRegion, to: region, state: "starting", startedAt: new Date().toISOString() };
      homeRegionMoves.set(authContext.accountId, move);
      void moveHomeRegion(home, move).catch((error) => {
        move.state = "failed";
        move.error = error instanceof Error ? error.message : String(error);
        console.warn(`home region move failed: ${move.error}`);
      });
      sendJson(res, 202, { move });
      return;
    }
  }
  if (req.method === "POST" && url.pathname === "/api/rudder/home/stop") {
    const home = findHomeRow(authContext.accountId);
    if (!home) {
      sendJson(res, 404, { error: "no home machine" });
      return;
    }
    const next = await mutateWorkspace(home, "stop");
    setHomeRepoStatus.run({ homeId: home.id, status: "stopped", updatedAt: new Date().toISOString() });
    sendJson(res, 200, { home: homeSummary(next) });
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

// ---------------------------------------------------------------------------
// Phone app: /app (static) + /api/app/* (JSON). The browser signs in with the
// same Google/GitHub better-auth user the CLI's `rudder login` approves, so
// account ids line up; a CLI bearer token works on these routes too.
// ---------------------------------------------------------------------------

const appDir = process.env.RUDDER_APP_DIR || path.join(process.cwd(), "app");
const APP_CONTENT_TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".webmanifest": "application/manifest+json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
};

async function requireAccount(req: IncomingMessage): Promise<{ accountId: string; email?: string }> {
  const authHeader = req.headers.authorization || "";
  if (authHeader.startsWith("Bearer ")) {
    return requireBearer(req);
  }
  const session = await getBetterAuthSession(req);
  const id = session?.user?.id;
  if (!id) {
    throw unauthorized();
  }
  return { accountId: String(id), email: typeof session?.user?.email === "string" ? session.user.email : undefined };
}

async function serveAppFile(res: ServerResponse, pathname: string): Promise<void> {
  const relative = pathname.replace(/^\/app\/?/, "") || "index.html";
  const resolved = path.resolve(appDir, relative);
  const insideApp = resolved === appDir || resolved.startsWith(`${appDir}${path.sep}`);
  let target = insideApp ? resolved : path.join(appDir, "index.html");
  let body: Buffer;
  try {
    body = await fs.readFile(target);
  } catch {
    // Client-side routes (/app/p/<id>) fall back to the shell.
    target = path.join(appDir, "index.html");
    body = await fs.readFile(target);
  }
  const type = APP_CONTENT_TYPES[path.extname(target)] ?? "application/octet-stream";
  res.statusCode = 200;
  res.setHeader("content-type", type);
  // The shell and the service worker must revalidate so a deploy shows up on
  // the next open; hashed assets are not used, so everything revalidates.
  res.setHeader("cache-control", "no-cache");
  if (path.basename(target) === "sw.js") {
    res.setHeader("service-worker-allowed", "/app");
  }
  res.end(body);
}

async function handleAppLogin(url: URL, res: ServerResponse): Promise<void> {
  const provider = url.searchParams.get("provider") === "github" ? "github" : "google";
  const providers = configuredProviders();
  if (!providers[provider]) {
    sendHtml(res, renderShell({
      title: "Sign in · Rudder",
      body: `<section class="hero"><h1>${escapeHtml(provider)} sign-in is not configured.</h1></section>`,
    }), 404);
    return;
  }
  refreshAuthHandler();
  const response = await auth.handler(new Request(`${authBaseURL}/sign-in/social`, {
    method: "POST",
    headers: { "content-type": "application/json", origin: baseURL },
    // The iOS app signs in through ASWebAuthenticationSession and comes back
    // via /app/native-done, which hands it a bearer token on its own scheme.
    body: JSON.stringify({
      provider,
      callbackURL: url.searchParams.get("native") === "1" ? `${baseURL}/app/native-done` : `${baseURL}/app`,
      disableRedirect: true,
    }),
  }));
  const text = await response.text();
  const parsed: Json = text ? parseJson(text) : null;
  const redirectURL = parsed && typeof parsed === "object" && !Array.isArray(parsed) && typeof parsed.url === "string"
    ? parsed.url
    : undefined;
  if (!response.ok || !redirectURL) {
    sendHtml(res, renderShell({
      title: "Sign in failed · Rudder",
      body: `<section class="hero"><h1>Sign in failed.</h1><p class="lede">${escapeHtml(responseErrorMessage(parsed) ?? text ?? String(response.status))}</p></section>`,
    }), response.ok ? 502 : response.status);
    return;
  }
  const responseHeaders = response.headers as Headers & { getSetCookie?: () => string[] };
  for (const cookie of responseHeaders.getSetCookie?.() ?? splitSetCookieHeader(response.headers.get("set-cookie"))) {
    res.setHeader("Set-Cookie", appendHeader(res.getHeader("Set-Cookie"), cookie));
  }
  res.statusCode = 302;
  res.setHeader("Location", redirectURL);
  res.end();
}

async function handleAppApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  const account = await requireAccount(req);
  const home = findHomeRow(account.accountId);
  if (req.method === "GET" && url.pathname === "/api/app/state") {
    if (home) {
      touchHome(home.id);
      // Opening the app is the wake-up: a home that went to sleep (or lost
      // its control socket) is started without anyone finding a Wake button.
      if (home.status !== "deleting" && !isOpen(homeControls.get(home.id))) {
        wakeHomeInBackground(account.accountId);
      }
    }
    sendJson(res, 200, {
      email: account.email ?? null,
      home: homeSummary(home),
      projects: home ? homeProjects(home) : [],
    });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/wake") {
    const next = await ensureHomeRunning(account.accountId);
    pushHomeRepos(next.id);
    sendJson(res, 200, { home: homeSummary(next) });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/open") {
    sendJson(res, 200, await openRepoOnHome(account.accountId, await readJsonBody(req)));
    return;
  }
  // Add project: the GitHub repos the home machine's login can open, and
  // creating a new one. Both run on the home machine (it holds the GitHub
  // token), so a sleeping machine is woken first.
  // `/model` on the phone: the models the home machine's dashboards offer, and
  // which one new agents start on.
  if (req.method === "GET" && url.pathname === "/api/app/models") {
    const live = await homeWithControl(account.accountId);
    sendJson(res, 200, await homeRpc(live.id, "models", {}, 40_000));
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/app/repos") {
    const live = await homeWithControl(account.accountId);
    sendJson(res, 200, await homeRpc(live.id, "repos", {}, 70_000));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/create") {
    const body = await readJsonBody(req);
    const live = await homeWithControl(account.accountId);
    const created = await homeRpc(live.id, "createRepo", {
      owner: stringField(body, "owner") ?? "",
      name: stringField(body, "name") ?? "",
      private: (body as JsonRecord)?.private !== false,
      description: stringField(body, "description") ?? "",
    }, 70_000);
    const repo = (created as JsonRecord)?.repo;
    sendJson(res, 200, await openRepoOnHome(account.accountId, { repo: typeof repo === "string" ? repo : "" }));
    return;
  }
  // Signing Claude Code or Codex in on the cloud machine from the phone
  // (cloud/worker/logins.mjs): the CLI's own login runs there, the phone opens
  // its link and hands back the callback or code. Starting one wakes the
  // machine, or creates it for a new account.
  if (req.method === "POST" && url.pathname === "/api/app/login/start") {
    const body = await readJsonBody(req);
    const provider = loginProvider(body);
    const live = await homeWithControl(account.accountId);
    sendJson(res, 200, await homeRpc(live.id, "loginStart", { provider, device: (body as JsonRecord)?.device === true }, 60_000));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/login/finish") {
    const body = await readJsonBody(req);
    const provider = loginProvider(body);
    const callback = stringField(body, "callback") ?? "";
    const code = stringField(body, "code") ?? "";
    if ((!callback && !code) || callback.length > 8000 || code.length > 8000) {
      throw badRequest("callback or code is required");
    }
    const live = await homeWithControl(account.accountId);
    sendJson(res, 200, await homeRpc(live.id, "loginFinish", {
      provider,
      id: stringField(body, "id") ?? "",
      ...(callback ? { callback } : { code }),
    }, 100_000));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/login/cancel") {
    const body = await readJsonBody(req);
    const provider = loginProvider(body);
    if (home && isOpen(homeControls.get(home.id))) {
      await homeRpc(home.id, "loginCancel", { provider, id: stringField(body, "id") ?? "" }).catch(() => null);
    }
    sendJson(res, 200, { ok: true });
    return;
  }
  if (!home) {
    sendJson(res, 404, { error: "no home machine yet; run `rudder cloud` in a repo, or add a project" });
    return;
  }
  touchHome(home.id);
  // A sleeping or still-booting home never answers with an error the user has
  // to act on. Reads (polled every second or two) kick the wake off in the
  // background and say so; writes wait for the machine, so a message sent to
  // a sleeping home is delivered once it is up instead of being lost.
  const homeConnected = isOpen(homeControls.get(home.id));
  if (!homeConnected && req.method === "GET") {
    wakeHomeInBackground(account.accountId);
    sendJson(res, 503, { error: "Waking the cloud machine…", waking: true });
    return;
  }
  if (!homeConnected) {
    try {
      await homeWithControl(account.accountId);
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      throw Object.assign(new Error(`the cloud machine is not connected yet (${reason})`), { status: 503 });
    }
  }
  if (req.method === "GET" && url.pathname === "/api/app/transcript") {
    const { repo } = requireHomeRepo(account.accountId, url.searchParams.get("workspace") ?? "");
    const result = await homeRpc(home.id, "transcript", {
      workspaceId: repo.id,
      runId: url.searchParams.get("run") ?? "",
      limit: Math.min(800, Math.max(20, Number(url.searchParams.get("limit") || 300))),
    });
    sendJson(res, 200, result);
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/send") {
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const text = (stringField(body, "text") ?? "").trim();
    if (!text) {
      throw badRequest("text is required");
    }
    sendJson(res, 200, await homeRpc(home.id, "send", {
      workspaceId: repo.id,
      runId: stringField(body, "run") ?? "",
      text: text.slice(0, 16_000),
    }));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/upload") {
    const { repo } = requireHomeRepo(account.accountId, url.searchParams.get("workspace") ?? "");
    sendJson(res, 200, await uploadToHome(home.id, repo.id, url.searchParams.get("name") ?? "", req));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/task") {
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const text = (stringField(body, "text") ?? "").trim();
    if (!text) {
      throw badRequest("text is required");
    }
    sendJson(res, 200, await homeRpc(home.id, "task", { workspaceId: repo.id, text: text.slice(0, 16_000) }));
    return;
  }
  if (req.method === "GET" && url.pathname === "/api/app/screen") {
    const { repo } = requireHomeRepo(account.accountId, url.searchParams.get("workspace") ?? "");
    sendJson(res, 200, await homeRpc(home.id, "screen", { workspaceId: repo.id, runId: url.searchParams.get("run") ?? "" }));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/keys") {
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const keys = (stringField(body, "keys") ?? "").trim();
    if (!/^[a-z0-9-]+(,[a-z0-9-]+)*$/i.test(keys) || keys.length > 200) {
      throw badRequest("keys must be names like esc,up,enter");
    }
    sendJson(res, 200, await homeRpc(home.id, "keys", { workspaceId: repo.id, runId: stringField(body, "run") ?? "", keys }));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/answer") {
    // The phone answering a prompt an agent is blocked on: a tool approval,
    // Claude's questions, or a plan (cloud/worker/asks.mjs builds the decision).
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const ask = stringField(body, "ask") ?? "";
    if (!/^[0-9]+-[0-9a-f]{8}$/.test(ask)) {
      throw badRequest("ask is required");
    }
    const answer = body && typeof body === "object" ? (body as Record<string, unknown>).answer : undefined;
    if (!answer || typeof answer !== "object" || JSON.stringify(answer).length > 64_000) {
      throw badRequest("answer is required");
    }
    sendJson(res, 200, await homeRpc(home.id, "answer", { workspaceId: repo.id, runId: stringField(body, "run") ?? "", askId: ask, answer: answer as Json }));
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/pin") {
    // Pinned projects sit at the top of the phone's list, in the order they
    // were pinned. Kept on the row, so every device signed in sees the same.
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const pinned = !!body && typeof body === "object" && !Array.isArray(body) && body.pinned === true;
    const pinnedAt = pinned ? repo.pinnedAt ?? new Date().toISOString() : null;
    setHomeRepoPinned.run({ id: repo.id, accountId: account.accountId, pinnedAt });
    sendJson(res, 200, { workspaceId: repo.id, pinnedAt });
    return;
  }
  if (req.method === "POST" && url.pathname === "/api/app/action") {
    // Dashboard actions the phone can take on one agent: stop, merge, delete.
    const body = await readJsonBody(req);
    const { repo } = requireHomeRepo(account.accountId, stringField(body, "workspace") ?? "");
    const action = stringField(body, "action") ?? "";
    if (action !== "stop" && action !== "merge" && action !== "delete") {
      throw badRequest("action must be stop, merge or delete");
    }
    sendJson(res, 200, await homeRpc(home.id, "action", { workspaceId: repo.id, runId: stringField(body, "run") ?? "", action }));
    return;
  }
  sendJson(res, 404, { error: "not found" });
}

// End of the iOS app's sign-in: the browser session just created is exchanged
// for a Rudder bearer token, handed back on the app's own URL scheme. The token
// is the same kind `rudder login` stores, scoped to the same account.
async function handleAppNativeDone(req: IncomingMessage, res: ServerResponse): Promise<void> {
  const session = await getBetterAuthSession(req);
  if (!session?.user?.id) {
    res.statusCode = 302;
    res.setHeader("Location", "rudder://auth?error=signin");
    res.end();
    return;
  }
  const verifiedEmail =
    typeof session.user.email === "string" &&
    (session.user as { emailVerified?: unknown }).emailVerified === true
      ? session.user.email
      : undefined;
  const issued = issueRudderToken(String(session.user.id), verifiedEmail);
  res.statusCode = 302;
  res.setHeader("Location", `rudder://auth?token=${encodeURIComponent(issued.token)}`);
  res.end();
}

// ---------------------------------------------------------------------------
// Pairing: the laptop (already signed in) vouches for the phone. `rudder cloud
// app` asks for a short code; the phone trades it for a bearer token on the same
// account. No OAuth provider involved, so it works whatever Google or GitHub
// think of the phone's browser.
// ---------------------------------------------------------------------------

const PAIR_TTL_MS = 10 * 60 * 1000;
const PAIR_ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789"; // no 0/O, 1/I/L
const pairings = new Map<string, { accountId: string; email?: string; expiresAt: number }>();
const pairFailures = new Map<string, { count: number; resetAt: number }>();

function newPairCode(): string {
  const bytes = randomBytes(8);
  let code = "";
  for (const byte of bytes) {
    code += PAIR_ALPHABET[byte % PAIR_ALPHABET.length];
  }
  return `${code.slice(0, 4)}-${code.slice(4)}`;
}

function normalizePairCode(value: string): string {
  const raw = value.toUpperCase().replace(/[^A-Z0-9]/g, "");
  return raw.length === 8 ? `${raw.slice(0, 4)}-${raw.slice(4)}` : "";
}

function clientAddress(req: IncomingMessage): string {
  const forwarded = req.headers["fly-client-ip"] ?? req.headers["x-forwarded-for"];
  const value = Array.isArray(forwarded) ? forwarded[0] : forwarded;
  return (value ?? req.socket.remoteAddress ?? "unknown").split(",")[0].trim();
}

async function handleAppPairing(req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> {
  if (req.method !== "POST") {
    sendJson(res, 405, { error: "method not allowed" });
    return;
  }
  const now = Date.now();
  for (const [code, pairing] of pairings) {
    if (pairing.expiresAt < now) {
      pairings.delete(code);
    }
  }
  if (url.pathname === "/api/app/pair/start") {
    // Only a signed-in device can vouch for another one.
    const account = await requireAccount(req);
    let code = newPairCode();
    while (pairings.has(code)) {
      code = newPairCode();
    }
    pairings.set(code, { accountId: account.accountId, email: account.email, expiresAt: now + PAIR_TTL_MS });
    sendJson(res, 200, {
      code,
      expiresAt: new Date(now + PAIR_TTL_MS).toISOString(),
      link: `rudder://pair?code=${code}`,
      url: `${baseURL}/app/pair?code=${code}`,
    });
    return;
  }
  // finish: unauthenticated by design, so it is single-use, short-lived and
  // rate limited per address (8 characters of a 31-letter alphabet ~ 2^39).
  const address = clientAddress(req);
  const failures = pairFailures.get(address);
  if (failures && failures.resetAt > now && failures.count >= 10) {
    sendJson(res, 429, { error: "too many attempts; wait a minute" });
    return;
  }
  const body = await readJsonBody(req);
  const code = normalizePairCode(stringField(body, "code") ?? "");
  const pairing = code ? pairings.get(code) : undefined;
  if (!pairing || pairing.expiresAt < now) {
    const next = failures && failures.resetAt > now ? failures : { count: 0, resetAt: now + 60_000 };
    next.count += 1;
    pairFailures.set(address, next);
    sendJson(res, 400, { error: "that code is wrong or expired; run `rudder cloud app` for a new one" });
    return;
  }
  pairings.delete(code);
  const issued = issueRudderToken(pairing.accountId, pairing.email);
  schedulePersistDatabase();
  sendJson(res, 200, { token: issued.token, email: pairing.email ?? null });
}

// The home machine, running and with its supervisor connected (woken and
// waited for when it was asleep), for requests only it can answer.
// Polled reads wake the home without waiting for it. A wake that failed is
// not retried on every poll: the next try waits out a cooldown.
const homeWakeFailedAt = new Map<string, number>();
function wakeHomeInBackground(accountId: string): void {
  const failedAt = homeWakeFailedAt.get(accountId) ?? 0;
  if (Date.now() - failedAt < 60_000) {
    return;
  }
  void ensureHomeRunning(accountId)
    .then((next) => {
      homeWakeFailedAt.delete(accountId);
      pushHomeRepos(next.id);
    })
    .catch(() => {
      homeWakeFailedAt.set(accountId, Date.now());
    });
}

function loginProvider(body: Json): "claude" | "codex" {
  const provider = stringField(body, "provider");
  if (provider !== "claude" && provider !== "codex") {
    throw badRequest("provider must be claude or codex");
  }
  return provider;
}

async function homeWithControl(accountId: string): Promise<Workspace> {
  const home = await ensureHomeRunning(accountId);
  const deadline = Date.now() + 90_000;
  while (!isOpen(homeControls.get(home.id))) {
    if (Date.now() > deadline) {
      throw Object.assign(new Error("the cloud machine is still starting; try again in a moment"), { status: 503 });
    }
    await sleep(1000);
  }
  return home;
}
