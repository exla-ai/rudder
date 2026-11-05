// Slack and Linear connections for an org or a single account.
//
// A connection belongs to an owner: an org (any member reads it, admins connect
// and disconnect) or an account (personal). Tokens are sealed with AES-256-GCM
// under a key derived from RUDDER_SECRETS_KEY, so the SQLite rows (and the S3
// copy of the database) only hold ciphertext. OAuth state is HMAC-signed with a
// second key from the same secret, so a callback cannot be replayed into
// another account or org.
//
// The server wires three entry points: handleAppApi (signed-in routes),
// handlePublicApi (OAuth callbacks, the Slack slash command and the Linear
// webhook) and onHomeState (agent status changes, posted to Slack and synced
// back to Linear issues). Everything it needs from the rest of the control
// plane is injected.
//
// Linear flow: a Linear project (or team) maps to a Rudder project on
// someone's home.
// - Tracking (Rudder -> Linear): each new agent in a linked project is decided
//   once (linear-track.ts: attach to an open issue, create one, or skip), and
//   the decision is kept per run (rudder_linear_tracked_runs). A tracked issue
//   moves to started, gets one "Merged:" comment when the agent merges, and
//   moves to completed only if Rudder created it. Created issues never carry
//   the `rudder` label, so they never start an agent.
// - Starting (Linear -> Rudder): an issue the user labels `rudder` reaches the
//   webhook, and the mapped project starts an agent on it (once per issue,
//   rudder_linear_issue_runs). As that run starts, needs you, and merges, the
//   issue moves to started, gets a comment, and moves to completed.
// - "Generate tasks" asks a model for small tasks and adds them as unlabeled
//   issues; labeling one starts an agent through the webhook.

import { createCipheriv, createDecipheriv, createHash, createHmac, hkdfSync, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Database } from "better-sqlite3";
import { agentNeeds, normalizedStatus, type StatusAgent } from "./agent-status.js";
import { verifySlackSignature } from "./slack.js";
import {
  exchangeSlackCode,
  listSlackChannels,
  matchProject,
  parseSlashCommand,
  postSlack,
  revokeSlackToken,
  SLASH_HELP_TEXT,
  slackAuthorizeUrl,
  slackEscape,
  slackLink,
  type SlackInstall,
} from "./slack-app.js";
import {
  commentOnLinearIssue,
  createLinearIssue,
  exchangeLinearCode,
  firstStateOfType,
  getLinearIssue,
  linearAuthorizeUrl,
  linearIssueLabelNames,
  linearIssueWorkflow,
  linearTaskText,
  linearWebhookFresh,
  linearWorkspace,
  listLinearIssues,
  listLinearProjects,
  listOpenLinearIssues,
  listLinearTeams,
  refreshLinearToken,
  revokeLinearToken,
  RUDDER_LABEL,
  setLinearIssueState,
  suggestRudderProject,
  verifyLinearSignature,
  type CreatedLinearIssue,
  type LinearIssue,
  type LinearToken,
} from "./linear.js";
import { generateTasks, GOAL_LIMIT, linearIssueDescription, TaskgenError, taskgenConfigured } from "./linear-tasks.js";
import { classifyWork, mergeLine, trackedIssueDescription, untrackedReason, userTask, type TrackDecision } from "./linear-track.js";

export type Provider = "slack" | "linear";
export type OrgRole = "admin" | "member";

export type IntegrationAgent = {
  id: string;
  title?: string | null;
  status?: string | null;
  asks?: unknown[] | null;
  /** Stamped by the control plane (agent-status.ts); derived when absent. */
  needsYou?: boolean | null;
  reason?: string | null;
  phase?: string | null;
  attention?: string | null;
  conflict?: boolean | null;
  pr?: unknown;
  /** "private" (team.ts): an org channel gets no title for it. */
  visibility?: string | null;
  /** The task text the agent was started with (a Linear task starts "Linear ENG-7: ..."). */
  task?: string | null;
  /** execute, plan, main, rudder-plan, ... (native AgentMode). */
  mode?: string | null;
  /** A /swarm experiment: one of many competing attempts, never tracked. */
  swarmExperiment?: boolean | null;
  /** The agent's done summary and its last message, for the merge comment. */
  summary?: string | null;
  lastMessage?: string | null;
  createdAt?: number | null;
  /** A task typed a moment ago that has no run yet; its id is not the run's. */
  pending?: boolean | null;
};

export type IntegrationProject = {
  workspaceId: string;
  name: string;
  repo?: string | null;
  agents?: IntegrationAgent[] | null;
};

export type IntegrationsDeps = {
  database: Database;
  env: NodeJS.ProcessEnv;
  fetch: typeof fetch;
  baseURL: string;
  now?: () => number;
  orgRole: (accountId: string, orgId: string) => OrgRole | null;
  orgIdsForAccount: (accountId: string) => string[];
  listProjects: (accountId: string) => IntegrationProject[];
  deliverTask: (accountId: string, workspaceId: string, text: string) => Promise<void>;
  capture?: (accountId: string, name: string, props?: Record<string, unknown>) => void;
  log?: (message: string) => void;
  /** How long a first needs-you waits before it posts (tests pass 0 or a few ms). */
  needsDebounceMs?: number;
};

export type Account = { accountId: string; email?: string };

const STATE_TTL_MS = 10 * 60 * 1000;
const LINK_CODE_TTL_MS = 10 * 60 * 1000;
// The same agent reaching the same status again inside this window (a flap
// between waiting and working, say) is not news.
const NOTIFY_REPEAT_MS = 60 * 1000;
// A first "needs you" waits this long before it posts: an Esc interrupt or a
// permission prompt answered in the terminal reads as needs-you for a second
// or two, and that blip is not news.
const NEEDS_DEBOUNCE_MS = 2500;
const MAX_BODY_BYTES = 256 * 1024;
// A webhook claim whose delivery never finished (a restart mid-start) can be
// retried after this long.
const LINEAR_CLAIM_STALE_MS = 10 * 60 * 1000;
// Issue runs are synced back to Linear for this long after they start.
const LINEAR_SYNC_WINDOW_MS = 14 * 24 * 60 * 60 * 1000;
// A run started before the issue was handed over is some earlier attempt.
const LINEAR_RUN_MATCH_SLACK_MS = 2 * 60 * 1000;
// A linked project's open issues are read again after this long.
const TRACK_ISSUE_CACHE_MS = 5 * 60 * 1000;
// A tracking decision that never finished (a restart mid-call) is retried after this long.
const TRACK_CLAIM_STALE_MS = 10 * 60 * 1000;
// Agents tracked this recently are shown to the classifier, so a follow-up is recognized.
const TRACK_RECENT_MS = 24 * 60 * 60 * 1000;
const TRACK_TITLE_LIMIT = 200;

class HttpError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

type Row = Record<string, unknown>;

type Owner = { kind: "org" | "account"; id: string; canManage: boolean };

type Install = {
  ownerKind: "org" | "account";
  ownerId: string;
  provider: Provider;
  token: Record<string, unknown>;
  teamId: string | null;
  teamName: string | null;
  workspaceName: string | null;
  channelId: string | null;
  channelName: string | null;
};

type NotifyCategory = "needs" | "ready" | "failed" | "working" | "other";
type AgentMemory = {
  category: NotifyCategory;
  posted: Map<NotifyCategory, number>;
  turnAt: number;
  /** A needs-you post waiting out NEEDS_DEBOUNCE_MS; cancel() drops it. */
  held?: { cancel: () => void };
};

export function deriveIntegrationKey(secret: string | undefined, label: string): Buffer | null {
  const raw = (secret ?? "").trim();
  if (!raw) {
    return null;
  }
  return Buffer.from(hkdfSync("sha256", Buffer.from(raw, "utf8"), Buffer.alloc(0), label, 32));
}

export function sealJson(key: Buffer, value: unknown, aad: string): string {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  cipher.setAAD(Buffer.from(aad, "utf8"));
  const ciphertext = Buffer.concat([cipher.update(JSON.stringify(value), "utf8"), cipher.final()]);
  return Buffer.concat([iv, cipher.getAuthTag(), ciphertext]).toString("base64");
}

export function openJson<T = Record<string, unknown>>(key: Buffer, sealed: string, aad: string): T {
  const raw = Buffer.from(sealed, "base64");
  const decipher = createDecipheriv("aes-256-gcm", key, raw.subarray(0, 12));
  decipher.setAAD(Buffer.from(aad, "utf8"));
  decipher.setAuthTag(raw.subarray(12, 28));
  return JSON.parse(Buffer.concat([decipher.update(raw.subarray(28)), decipher.final()]).toString("utf8")) as T;
}

type StatePayload = { a: string; o: string | null; p: Provider; e: number; n: string };

export function signState(key: Buffer, payload: StatePayload): string {
  const body = Buffer.from(JSON.stringify(payload)).toString("base64url");
  const mac = createHmac("sha256", key).update(body).digest("base64url");
  return `${body}.${mac}`;
}

export function verifyState(key: Buffer, state: string, now: number): StatePayload | null {
  const [body, mac] = state.split(".");
  if (!body || !mac) {
    return null;
  }
  const expected = Buffer.from(createHmac("sha256", key).update(body).digest("base64url"));
  const given = Buffer.from(mac);
  if (expected.length !== given.length || !timingSafeEqual(expected, given)) {
    return null;
  }
  try {
    const payload = JSON.parse(Buffer.from(body, "base64url").toString("utf8")) as StatePayload;
    if (typeof payload.a !== "string" || (payload.p !== "slack" && payload.p !== "linear") || !(payload.e > now)) {
      return null;
    }
    return payload;
  } catch {
    return null;
  }
}

function sha256(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function redirect(res: ServerResponse, location: string): void {
  res.writeHead(302, { location, "cache-control": "no-store" });
  res.end();
}

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += buf.length;
    if (total > MAX_BODY_BYTES) {
      req.destroy();
      throw new HttpError(413, "request body too large");
    }
    chunks.push(buf);
  }
  return Buffer.concat(chunks).toString("utf8");
}

async function readJson(req: IncomingMessage): Promise<Record<string, unknown>> {
  const raw = await readBody(req);
  if (!raw.trim()) {
    return {};
  }
  try {
    const parsed = JSON.parse(raw) as unknown;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : {};
  } catch {
    throw new HttpError(400, "invalid JSON body");
  }
}

function str(value: unknown): string {
  return typeof value === "string" ? value.trim() : "";
}

function header(req: IncomingMessage, name: string): string | undefined {
  const value = req.headers[name];
  return Array.isArray(value) ? value[0] : value;
}

function needsOf(agent: IntegrationAgent): { needsYou: boolean; reason: string | null } {
  return typeof agent.needsYou === "boolean"
    ? { needsYou: agent.needsYou, reason: agent.reason ?? null }
    : agentNeeds(agent as StatusAgent);
}

// Slack's categories, from the control plane's one needs-you rule: finished
// work awaiting review is "ready", a failure is "failed", anything else that
// needs you (a prompt, input, an interrupt, a conflict) is "needs".
function agentCategory(agent: IntegrationAgent): NotifyCategory {
  const status = String(agent.status ?? "");
  const needs = needsOf(agent);
  if (status === "failed" || needs.reason === "failed" || needs.reason === "delivery-blocked") {
    return "failed";
  }
  if (needs.needsYou && needs.reason !== "review") {
    return "needs";
  }
  if (status === "review" || status === "done") {
    return "ready";
  }
  if (status === "working" || status === "starting") {
    return "working";
  }
  return "other";
}

const NEEDS_WORDS: Record<string, string> = {
  permission: "needs your permission",
  question: "asked you a question",
  plan: "has a plan for you",
  input: "needs your input",
  interrupted: "was interrupted and is waiting for you",
  error: "hit an error and is waiting for you",
  conflict: "hit a merge conflict",
  "not-in-trunk": "merged work is no longer in main",
  delivery: "needs delivery proof",
};

function statusLine(agent: IntegrationAgent, category: NotifyCategory): string {
  if (category === "needs") {
    const words = NEEDS_WORDS[needsOf(agent).reason ?? ""];
    return words ? `needs you: ${words}` : "needs you";
  }
  if (category === "failed") {
    return "failed";
  }
  const pr = agent.pr && typeof agent.pr === "object" ? (agent.pr as { state?: unknown }).state : null;
  if (pr === "open" || pr === "draft") {
    return "done, PR open";
  }
  return agent.status === "review" ? "ready for review" : "done";
}

export type Integrations = ReturnType<typeof createIntegrations>;

export function createIntegrations(deps: IntegrationsDeps) {
  const { database } = deps;
  const now = deps.now ?? Date.now;
  const log = deps.log ?? ((message: string) => console.warn(message));
  const needsDebounceMs = Math.max(0, deps.needsDebounceMs ?? NEEDS_DEBOUNCE_MS);
  const baseURL = deps.baseURL.replace(/\/+$/, "");
  const tokenKey = deriveIntegrationKey(deps.env.RUDDER_SECRETS_KEY, "rudder-integrations-token-v1");
  const stateKey = deriveIntegrationKey(deps.env.RUDDER_SECRETS_KEY, "rudder-integrations-state-v1");
  const slackConfig = {
    clientId: str(deps.env.RUDDER_SLACK_CLIENT_ID),
    clientSecret: str(deps.env.RUDDER_SLACK_CLIENT_SECRET),
    signingSecret: str(deps.env.RUDDER_SLACK_SIGNING_SECRET),
  };
  const linearConfig = {
    clientId: str(deps.env.RUDDER_LINEAR_CLIENT_ID),
    clientSecret: str(deps.env.RUDDER_LINEAR_CLIENT_SECRET),
    webhookSecret: str(deps.env.RUDDER_LINEAR_WEBHOOK_SECRET),
  };
  const configured = {
    slack: Boolean(slackConfig.clientId && slackConfig.clientSecret),
    linear: Boolean(linearConfig.clientId && linearConfig.clientSecret),
  };
  const redirectUri = (provider: Provider) => `${baseURL}/api/integrations/${provider}/callback`;
  const pending = new Set<Promise<unknown>>();
  const memory = new Map<string, Map<string, AgentMemory>>();

  database.exec(`
    create table if not exists rudder_integrations (
      id text primary key,
      owner_kind text not null,
      owner_id text not null,
      provider text not null,
      token_ciphertext text not null,
      team_id text,
      team_name text,
      workspace_name text,
      channel_id text,
      channel_name text,
      installed_by text,
      created_at text not null,
      updated_at text not null,
      unique (owner_kind, owner_id, provider)
    );
    create index if not exists rudder_integrations_team on rudder_integrations (provider, team_id);
    create table if not exists rudder_slack_links (
      team_id text not null,
      user_id text not null,
      account_id text not null,
      created_at text not null,
      primary key (team_id, user_id)
    );
    create table if not exists rudder_slack_link_codes (
      code_hash text primary key,
      team_id text not null,
      team_name text,
      user_id text not null,
      user_name text,
      expires_at integer not null
    );
    -- A Linear project or team -> the Rudder project its issues run in, per
    -- install. account_id is whose home holds that project.
    create table if not exists rudder_linear_links (
      owner_kind text not null,
      owner_id text not null,
      linear_kind text not null,
      linear_id text not null,
      linear_name text,
      team_id text,
      workspace_id text not null,
      project_name text,
      account_id text not null,
      updated_at text not null,
      primary key (owner_kind, owner_id, linear_kind, linear_id)
    );
    -- One row per Linear issue Rudder started: the dedupe for webhooks, and
    -- the issue -> run mapping that status sync reads. phase: starting,
    -- delivered (task handed over, run not seen yet), running, merged.
    create table if not exists rudder_linear_issue_runs (
      issue_id text primary key,
      identifier text,
      owner_kind text not null,
      owner_id text not null,
      account_id text not null,
      workspace_id text not null,
      project_name text,
      source text not null,
      phase text not null,
      run_id text,
      start_commented integer not null default 0,
      needs_commented integer not null default 0,
      created_at integer not null,
      updated_at integer not null
    );
    create index if not exists rudder_linear_issue_runs_account on rudder_linear_issue_runs (account_id, phase);
    create index if not exists rudder_linear_issue_runs_run on rudder_linear_issue_runs (run_id);
    create index if not exists rudder_linear_links_account on rudder_linear_links (account_id, workspace_id);
    -- One row per agent run Rudder decided to track (or not) in Linear, skips
    -- included, so a run is classified once. action: deciding (model call in
    -- flight), attach, create, skip. created_issue: this run created the issue
    -- (only then does its merge complete it). started_synced / merged_synced:
    -- the progress steps already sent to Linear.
    create table if not exists rudder_linear_tracked_runs (
      workspace_id text not null,
      run_id text not null,
      account_id text not null,
      owner_kind text not null,
      owner_id text not null,
      action text not null,
      issue_id text,
      identifier text,
      created_issue integer not null default 0,
      started_synced integer not null default 0,
      merged_synced integer not null default 0,
      agent_title text,
      reason text,
      created_at integer not null,
      updated_at integer not null,
      primary key (workspace_id, run_id)
    );
    create index if not exists rudder_linear_tracked_runs_account on rudder_linear_tracked_runs (account_id, action);
    create table if not exists rudder_linear_meta (
      key text primary key,
      value text not null
    );
  `);
  // Tracking starts for runs created after this, so existing runs are never backfilled.
  database.prepare("insert or ignore into rudder_linear_meta (key, value) values ('track_started_at', ?)").run(String(now()));
  const trackStartedAt =
    Number((database.prepare("select value from rudder_linear_meta where key = 'track_started_at'").get() as Row | undefined)?.value) || now();

  const findInstallRow = database.prepare(
    "select * from rudder_integrations where owner_kind = ? and owner_id = ? and provider = ?",
  );
  const upsertInstall = database.prepare(`
    insert into rudder_integrations (id, owner_kind, owner_id, provider, token_ciphertext, team_id, team_name,
      workspace_name, channel_id, channel_name, installed_by, created_at, updated_at)
    values (@id, @ownerKind, @ownerId, @provider, @token, @teamId, @teamName, @workspaceName, null, null,
      @installedBy, @now, @now)
    on conflict (owner_kind, owner_id, provider) do update set
      token_ciphertext = excluded.token_ciphertext,
      channel_id = case when team_id is excluded.team_id then channel_id else null end,
      channel_name = case when team_id is excluded.team_id then channel_name else null end,
      team_id = excluded.team_id,
      team_name = excluded.team_name,
      workspace_name = excluded.workspace_name,
      installed_by = excluded.installed_by,
      updated_at = excluded.updated_at
  `);
  const updateToken = database.prepare(
    "update rudder_integrations set token_ciphertext = ?, updated_at = ? where owner_kind = ? and owner_id = ? and provider = ?",
  );
  const updateChannel = database.prepare(
    "update rudder_integrations set channel_id = ?, channel_name = ?, updated_at = ? where owner_kind = ? and owner_id = ? and provider = 'slack'",
  );
  const deleteInstall = database.prepare(
    "delete from rudder_integrations where owner_kind = ? and owner_id = ? and provider = ?",
  );
  const slackInstallsForTeam = database.prepare("select * from rudder_integrations where provider = 'slack' and team_id = ?");
  const findLink = database.prepare("select account_id from rudder_slack_links where team_id = ? and user_id = ?");
  const upsertLink = database.prepare(`
    insert into rudder_slack_links (team_id, user_id, account_id, created_at) values (?, ?, ?, ?)
    on conflict (team_id, user_id) do update set account_id = excluded.account_id, created_at = excluded.created_at
  `);
  const insertLinkCode = database.prepare(
    "insert into rudder_slack_link_codes (code_hash, team_id, team_name, user_id, user_name, expires_at) values (?, ?, ?, ?, ?, ?)",
  );
  const findLinkCode = database.prepare("select * from rudder_slack_link_codes where code_hash = ?");
  const deleteLinkCode = database.prepare("delete from rudder_slack_link_codes where code_hash = ?");
  const pruneLinkCodes = database.prepare("delete from rudder_slack_link_codes where expires_at < ?");
  const setLinearOrg = database.prepare(
    "update rudder_integrations set team_id = ?, team_name = ?, updated_at = ? where owner_kind = ? and owner_id = ? and provider = 'linear'",
  );
  // Org installs first: an org and a person can both connect one Linear workspace.
  const linearInstallsForOrg = database.prepare(
    "select * from rudder_integrations where provider = 'linear' and team_id = ? order by owner_kind desc, updated_at desc",
  );
  const linksForOwner = database.prepare(
    "select * from rudder_linear_links where owner_kind = ? and owner_id = ? order by linear_kind, linear_name",
  );
  const findLinearLink = database.prepare(
    "select * from rudder_linear_links where owner_kind = ? and owner_id = ? and linear_kind = ? and linear_id = ?",
  );
  const upsertLinearLink = database.prepare(`
    insert into rudder_linear_links (owner_kind, owner_id, linear_kind, linear_id, linear_name, team_id, workspace_id,
      project_name, account_id, updated_at)
    values (@ownerKind, @ownerId, @kind, @linearId, @linearName, @teamId, @workspaceId, @projectName, @accountId, @now)
    on conflict (owner_kind, owner_id, linear_kind, linear_id) do update set
      linear_name = excluded.linear_name,
      team_id = excluded.team_id,
      workspace_id = excluded.workspace_id,
      project_name = excluded.project_name,
      account_id = excluded.account_id,
      updated_at = excluded.updated_at
  `);
  const deleteLinearLink = database.prepare(
    "delete from rudder_linear_links where owner_kind = ? and owner_id = ? and linear_kind = ? and linear_id = ?",
  );
  const findIssueRun = database.prepare("select * from rudder_linear_issue_runs where issue_id = ?");
  const dropStaleClaim = database.prepare(
    "delete from rudder_linear_issue_runs where issue_id = ? and phase = 'starting' and updated_at < ?",
  );
  const claimIssueRun = database.prepare(`
    insert or ignore into rudder_linear_issue_runs (issue_id, identifier, owner_kind, owner_id, account_id, workspace_id,
      project_name, source, phase, run_id, start_commented, needs_commented, created_at, updated_at)
    values (@issueId, null, @ownerKind, @ownerId, @accountId, @workspaceId, @projectName, 'webhook', 'starting', null, 0, 0, @now, @now)
  `);
  const markIssueDelivered = database.prepare(
    "update rudder_linear_issue_runs set phase = 'delivered', identifier = ?, project_name = ?, updated_at = ? where issue_id = ?",
  );
  // A start from the app always hands the issue over (it may be a second try),
  // so it replaces any earlier row for the issue.
  const recordAppStart = database.prepare(`
    insert into rudder_linear_issue_runs (issue_id, identifier, owner_kind, owner_id, account_id, workspace_id,
      project_name, source, phase, run_id, start_commented, needs_commented, created_at, updated_at)
    values (@issueId, @identifier, @ownerKind, @ownerId, @accountId, @workspaceId, @projectName, 'app', 'delivered', null, 1, 0, @now, @now)
    on conflict (issue_id) do update set
      identifier = excluded.identifier, owner_kind = excluded.owner_kind, owner_id = excluded.owner_id,
      account_id = excluded.account_id, workspace_id = excluded.workspace_id, project_name = excluded.project_name,
      source = excluded.source, phase = excluded.phase, run_id = null, start_commented = 1, needs_commented = 0,
      created_at = excluded.created_at, updated_at = excluded.updated_at
  `);
  const deleteIssueRun = database.prepare("delete from rudder_linear_issue_runs where issue_id = ?");
  const activeIssueRuns = database.prepare(
    "select * from rudder_linear_issue_runs where account_id = ? and phase in ('delivered', 'running') and created_at > ?",
  );
  const setIssueRunId = database.prepare(
    "update rudder_linear_issue_runs set run_id = ?, phase = 'running', updated_at = ? where issue_id = ? and run_id is null",
  );
  const markIssueMerged = database.prepare(
    "update rudder_linear_issue_runs set phase = 'merged', updated_at = ? where issue_id = ? and phase != 'merged'",
  );
  const claimStartComment = database.prepare(
    "update rudder_linear_issue_runs set start_commented = 1, updated_at = ? where issue_id = ? and start_commented = 0",
  );
  const claimNeedsComment = database.prepare(
    "update rudder_linear_issue_runs set needs_commented = 1, updated_at = ? where issue_id = ? and needs_commented = 0",
  );
  const issueRunForRun = database.prepare("select 1 from rudder_linear_issue_runs where run_id = ? limit 1");
  // Org links first, then a Linear project before a team (the webhook's order).
  const linksForAccount = database.prepare(
    "select * from rudder_linear_links where account_id = ? order by owner_kind desc, linear_kind, updated_at desc",
  );
  const findTracked = database.prepare("select * from rudder_linear_tracked_runs where workspace_id = ? and run_id = ?");
  const dropStaleTrackClaim = database.prepare(
    "delete from rudder_linear_tracked_runs where workspace_id = ? and run_id = ? and action = 'deciding' and updated_at < ?",
  );
  const claimTracked = database.prepare(`
    insert or ignore into rudder_linear_tracked_runs (workspace_id, run_id, account_id, owner_kind, owner_id, action,
      agent_title, created_at, updated_at)
    values (@workspaceId, @runId, @accountId, @ownerKind, @ownerId, 'deciding', @title, @now, @now)
  `);
  const decideTracked = database.prepare(`
    update rudder_linear_tracked_runs set action = @action, issue_id = @issueId, identifier = @identifier,
      created_issue = @created, reason = @reason, updated_at = @now
    where workspace_id = @workspaceId and run_id = @runId and action = 'deciding'
  `);
  const activeTrackedRuns = database.prepare(
    "select * from rudder_linear_tracked_runs where account_id = ? and action in ('attach', 'create') and merged_synced = 0 and created_at > ?",
  );
  const recentTracked = database.prepare(
    "select agent_title, identifier from rudder_linear_tracked_runs where workspace_id = ? and action in ('attach', 'create') and created_at > ? order by created_at desc limit 8",
  );
  const claimTrackStarted = database.prepare(
    "update rudder_linear_tracked_runs set started_synced = 1, updated_at = ? where workspace_id = ? and run_id = ? and started_synced = 0",
  );
  const claimTrackMerged = database.prepare(
    "update rudder_linear_tracked_runs set merged_synced = 1, updated_at = ? where workspace_id = ? and run_id = ? and merged_synced = 0",
  );

  function aad(ownerKind: string, ownerId: string, provider: string): string {
    return `${ownerKind}:${ownerId}:${provider}`;
  }

  function requireKey(): Buffer {
    if (!tokenKey || !stateKey) {
      throw new HttpError(503, "integrations need RUDDER_SECRETS_KEY on this server to store tokens");
    }
    return tokenKey;
  }

  function installFromRow(row: Row | undefined): Install | null {
    if (!row || !tokenKey) {
      return null;
    }
    const ownerKind = String(row.owner_kind) as Install["ownerKind"];
    const ownerId = String(row.owner_id);
    const provider = String(row.provider) as Provider;
    let token: Record<string, unknown>;
    try {
      token = openJson(tokenKey, String(row.token_ciphertext), aad(ownerKind, ownerId, provider));
    } catch {
      log(`integrations: cannot decrypt ${provider} token for ${ownerKind}:${ownerId}`);
      return null;
    }
    return {
      ownerKind,
      ownerId,
      provider,
      token,
      teamId: row.team_id ? String(row.team_id) : null,
      teamName: row.team_name ? String(row.team_name) : null,
      workspaceName: row.workspace_name ? String(row.workspace_name) : null,
      channelId: row.channel_id ? String(row.channel_id) : null,
      channelName: row.channel_name ? String(row.channel_name) : null,
    };
  }

  function getInstall(owner: Pick<Owner, "kind" | "id">, provider: Provider): Install | null {
    return installFromRow(findInstallRow.get(owner.kind, owner.id, provider) as Row | undefined);
  }

  function storeInstall(params: {
    owner: Pick<Owner, "kind" | "id">;
    provider: Provider;
    token: unknown;
    teamId?: string | null;
    teamName?: string | null;
    workspaceName?: string | null;
    installedBy: string;
  }): void {
    const key = requireKey();
    upsertInstall.run({
      id: randomUUID(),
      ownerKind: params.owner.kind,
      ownerId: params.owner.id,
      provider: params.provider,
      token: sealJson(key, params.token, aad(params.owner.kind, params.owner.id, params.provider)),
      teamId: params.teamId ?? null,
      teamName: params.teamName ?? null,
      workspaceName: params.workspaceName ?? null,
      installedBy: params.installedBy,
      now: new Date(now()).toISOString(),
    });
  }

  function resolveOwner(account: Account, orgId: string, need: "read" | "manage"): Owner {
    if (!orgId) {
      return { kind: "account", id: account.accountId, canManage: true };
    }
    const role = deps.orgRole(account.accountId, orgId);
    if (!role) {
      throw new HttpError(404, "org not found");
    }
    if (need === "manage" && role !== "admin") {
      throw new HttpError(403, "only an org admin can change its integrations");
    }
    return { kind: "org", id: orgId, canManage: role === "admin" };
  }

  function track<T>(promise: Promise<T>): Promise<T> {
    pending.add(promise);
    void promise.finally(() => pending.delete(promise)).catch(() => undefined);
    return promise;
  }

  function capture(accountId: string, name: string, props?: Record<string, unknown>): void {
    try {
      deps.capture?.(accountId, name, props);
    } catch {
      // Analytics never breaks a request.
    }
  }

  // Linear access tokens may expire; refresh a minute early and store the new pair.
  async function linearAccessToken(install: Install): Promise<string> {
    const token = install.token as unknown as LinearToken;
    if (!token.expiresAt || token.expiresAt - 60_000 > now()) {
      return token.accessToken;
    }
    const fresh = await refreshLinearToken(deps.fetch, {
      clientId: linearConfig.clientId,
      clientSecret: linearConfig.clientSecret,
      token,
      now: now(),
    });
    const key = requireKey();
    updateToken.run(
      sealJson(key, fresh, aad(install.ownerKind, install.ownerId, "linear")),
      new Date(now()).toISOString(),
      install.ownerKind,
      install.ownerId,
      "linear",
    );
    return fresh.accessToken;
  }

  function requireLinear(owner: Owner): Install {
    const install = getInstall(owner, "linear");
    if (!install) {
      throw new HttpError(404, "Linear is not connected");
    }
    return install;
  }

  function requireSlack(owner: Owner): Install {
    const install = getInstall(owner, "slack");
    if (!install) {
      throw new HttpError(404, "Slack is not connected");
    }
    return install;
  }

  function summary(account: Account, orgId: string) {
    const owner = resolveOwner(account, orgId, "read");
    const slack = getInstall(owner, "slack");
    const linear = getInstall(owner, "linear");
    return {
      slack: {
        configured: configured.slack,
        connected: Boolean(slack),
        team: slack?.teamId ? { id: slack.teamId, name: slack.teamName ?? "Slack" } : null,
        channel: slack?.channelId ? { id: slack.channelId, name: slack.channelName ?? slack.channelId } : null,
      },
      linear: {
        configured: configured.linear,
        connected: Boolean(linear),
        workspace: linear ? { name: linear.workspaceName ?? "Linear" } : null,
      },
      canManage: owner.canManage,
    };
  }

  function startOAuth(account: Account, orgId: string, provider: Provider): string {
    if (!configured[provider]) {
      throw new HttpError(503, `${provider === "slack" ? "Slack" : "Linear"} is not set up on this server`);
    }
    requireKey();
    const owner = resolveOwner(account, orgId, "manage");
    const state = signState(stateKey!, {
      a: account.accountId,
      o: owner.kind === "org" ? owner.id : null,
      p: provider,
      e: now() + STATE_TTL_MS,
      n: randomBytes(8).toString("base64url"),
    });
    const config = provider === "slack" ? slackConfig : linearConfig;
    const authorize = provider === "slack" ? slackAuthorizeUrl : linearAuthorizeUrl;
    return authorize({ clientId: config.clientId, redirectUri: redirectUri(provider), state });
  }

  async function finishOAuth(provider: Provider, url: URL): Promise<string> {
    const settings = (params: Record<string, string>) => `/app/settings?${new URLSearchParams(params).toString()}`;
    const failed = (reason: string) => settings({ error: reason, provider });
    if (url.searchParams.get("error")) {
      return failed(url.searchParams.get("error") === "access_denied" ? "cancelled" : "denied");
    }
    if (!stateKey || !configured[provider]) {
      return failed("not_configured");
    }
    const state = verifyState(stateKey, url.searchParams.get("state") ?? "", now());
    if (!state || state.p !== provider) {
      return failed("bad_state");
    }
    const code = url.searchParams.get("code") ?? "";
    if (!code) {
      return failed("no_code");
    }
    let owner: Owner;
    try {
      // Rights are checked again: an admin demoted since starting cannot finish.
      owner = resolveOwner({ accountId: state.a }, state.o ?? "", "manage");
    } catch {
      return failed("forbidden");
    }
    try {
      if (provider === "slack") {
        const install: SlackInstall = await exchangeSlackCode(deps.fetch, {
          clientId: slackConfig.clientId,
          clientSecret: slackConfig.clientSecret,
          code,
          redirectUri: redirectUri("slack"),
        });
        storeInstall({
          owner,
          provider,
          token: { accessToken: install.accessToken, botUserId: install.botUserId, scope: install.scope },
          teamId: install.teamId,
          teamName: install.teamName,
          installedBy: state.a,
        });
      } else {
        const token = await exchangeLinearCode(deps.fetch, {
          clientId: linearConfig.clientId,
          clientSecret: linearConfig.clientSecret,
          code,
          redirectUri: redirectUri("linear"),
          now: now(),
        });
        const workspace = await linearWorkspace(deps.fetch, token.accessToken).catch(() => ({ id: null, name: "Linear", urlKey: null }));
        // team_id holds the Linear workspace id: webhooks name it as organizationId.
        storeInstall({
          owner,
          provider,
          token,
          teamId: workspace.id,
          teamName: workspace.id ? workspace.name : null,
          workspaceName: workspace.name,
          installedBy: state.a,
        });
      }
    } catch (error) {
      log(`integrations: ${provider} oauth exchange failed: ${error instanceof Error ? error.message : String(error)}`);
      return failed("exchange_failed");
    }
    capture(state.a, "integration_connected", { provider, scope: owner.kind });
    return settings(owner.kind === "org" ? { connected: provider, org: owner.id } : { connected: provider });
  }

  async function disconnect(account: Account, orgId: string, provider: Provider): Promise<void> {
    const owner = resolveOwner(account, orgId, "manage");
    const install = getInstall(owner, provider);
    deleteInstall.run(owner.kind, owner.id, provider);
    const accessToken = install ? str(install.token.accessToken) : "";
    if (accessToken) {
      // Best effort: the row is already gone, so a failed revoke only leaves
      // a token nobody here can read.
      const revoke = provider === "slack" ? revokeSlackToken : revokeLinearToken;
      await revoke(deps.fetch, accessToken).catch(() => undefined);
    }
    capture(account.accountId, "integration_disconnected", { provider, scope: owner.kind });
  }

  function errorText(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
  }

  function findProject(accountId: string, workspaceId: string): IntegrationProject | undefined {
    return deps.listProjects(accountId).find((candidate) => candidate.workspaceId === workspaceId);
  }

  // Hands a Linear issue to an agent in a project on `accountId`'s home.
  async function deliverLinearIssue(params: { accountId: string; install: Install; issueId: string; workspaceId: string }): Promise<{
    issue: LinearIssue;
    project: IntegrationProject;
    text: string;
    token: string;
  }> {
    const project = findProject(params.accountId, params.workspaceId);
    if (!project) {
      throw new HttpError(404, "project not found");
    }
    const token = await linearAccessToken(params.install);
    const issue = await getLinearIssue(deps.fetch, token, params.issueId);
    if (!issue) {
      throw new HttpError(404, "Linear issue not found");
    }
    const text = linearTaskText(issue);
    await deps.deliverTask(params.accountId, params.workspaceId, text);
    return { issue, project, text, token };
  }

  async function startLinearIssue(account: Account, body: Record<string, unknown>) {
    const owner = resolveOwner(account, str(body.org), "read");
    const issueId = str(body.issueId);
    const workspaceId = str(body.workspace);
    if (!issueId || !workspaceId) {
      throw new HttpError(400, "issueId and workspace are required");
    }
    if (!findProject(account.accountId, workspaceId)) {
      throw new HttpError(404, "project not found");
    }
    const install = requireLinear(owner);
    const { issue, project, text, token } = await deliverLinearIssue({ accountId: account.accountId, install, issueId, workspaceId });
    try {
      await commentOnLinearIssue(deps.fetch, token, issue.id, `Rudder started an agent on this in ${project.name}.`);
    } catch (error) {
      log(`integrations: linear comment failed: ${errorText(error)}`);
    }
    // Remembered so its status syncs back, and so the webhook does not start it again.
    recordAppStart.run({
      issueId: issue.id,
      identifier: issue.identifier,
      ownerKind: owner.kind,
      ownerId: owner.id,
      accountId: account.accountId,
      workspaceId,
      projectName: project.name,
      now: now(),
    });
    capture(account.accountId, "linear_issue_started", { scope: owner.kind });
    return { ok: true, text };
  }

  // ---- Linear: projects and teams mapped to Rudder projects ----

  type LinearKind = "project" | "team";

  // Installs connected before webhooks existed have no workspace id yet.
  async function ensureLinearOrg(install: Install, token: string): Promise<void> {
    if (install.teamId) {
      return;
    }
    try {
      const workspace = await linearWorkspace(deps.fetch, token);
      if (workspace.id) {
        setLinearOrg.run(workspace.id, workspace.name, new Date(now()).toISOString(), install.ownerKind, install.ownerId);
        install.teamId = workspace.id;
      }
    } catch (error) {
      log(`integrations: linear workspace lookup failed: ${errorText(error)}`);
    }
  }

  function linkView(row: Row | undefined, account: Account) {
    if (!row) {
      return null;
    }
    return {
      workspaceId: String(row.workspace_id),
      projectName: row.project_name ? String(row.project_name) : null,
      teamId: row.team_id ? String(row.team_id) : null,
      mine: String(row.account_id) === account.accountId,
    };
  }

  async function linearSetup(account: Account, orgId: string) {
    const owner = resolveOwner(account, orgId, "read");
    const install = requireLinear(owner);
    const token = await linearAccessToken(install);
    await ensureLinearOrg(install, token);
    const [linearProjects, teams] = await Promise.all([listLinearProjects(deps.fetch, token), listLinearTeams(deps.fetch, token)]);
    const projects = deps.listProjects(account.accountId).map((project) => ({
      workspaceId: project.workspaceId,
      name: project.name,
      repo: project.repo ?? null,
    }));
    const links = linksForOwner.all(owner.kind, owner.id) as Row[];
    const linkOf = (kind: LinearKind, id: string) =>
      links.find((row) => row.linear_kind === kind && String(row.linear_id) === id);
    const suggestion = (name: string) => {
      const match = suggestRudderProject(name, projects);
      return match ? { workspaceId: match.workspaceId, name: match.name } : null;
    };
    return {
      linearProjects: linearProjects.map((project) => {
        const link = linkOf("project", project.id);
        return { ...project, link: linkView(link, account), suggestion: link ? null : suggestion(project.name) };
      }),
      teams: teams.map((team) => {
        const link = linkOf("team", team.id);
        return { ...team, link: linkView(link, account), suggestion: link ? null : suggestion(team.name) };
      }),
      projects,
      canManage: owner.canManage,
      generate: taskgenConfigured(deps.env),
      autoStart: Boolean(linearConfig.webhookSecret),
    };
  }

  // The Linear project or team by id, and the team its new issues go to.
  async function resolveLinearTarget(token: string, kind: LinearKind, linearId: string, teamHint: string) {
    if (kind === "team") {
      const team = (await listLinearTeams(deps.fetch, token)).find((candidate) => candidate.id === linearId);
      if (!team) {
        throw new HttpError(404, "Linear team not found");
      }
      return { name: team.name, teamId: team.id };
    }
    const project = (await listLinearProjects(deps.fetch, token)).find((candidate) => candidate.id === linearId);
    if (!project) {
      throw new HttpError(404, "Linear project not found");
    }
    const team = project.teams.find((candidate) => candidate.id === teamHint) ?? project.teams[0];
    if (!team) {
      throw new HttpError(400, "That Linear project has no team to create issues in");
    }
    return { name: project.name, teamId: team.id };
  }

  function linearKind(value: unknown): LinearKind {
    const kind = str(value) || "project";
    if (kind !== "project" && kind !== "team") {
      throw new HttpError(400, "kind is project or team");
    }
    return kind;
  }

  async function setLinearLink(account: Account, body: Record<string, unknown>, orgParam: string) {
    const owner = resolveOwner(account, str(body.org) || orgParam, "manage");
    const kind = linearKind(body.kind);
    const linearId = str(body.linearId);
    if (!linearId) {
      throw new HttpError(400, "linearId is required");
    }
    const install = requireLinear(owner);
    const workspaceId = str(body.workspace);
    if (!workspaceId) {
      deleteLinearLink.run(owner.kind, owner.id, kind, linearId);
      return { link: null };
    }
    const project = findProject(account.accountId, workspaceId);
    if (!project) {
      throw new HttpError(404, "project not found");
    }
    const token = await linearAccessToken(install);
    await ensureLinearOrg(install, token);
    const target = await resolveLinearTarget(token, kind, linearId, str(body.teamId));
    upsertLinearLink.run({
      ownerKind: owner.kind,
      ownerId: owner.id,
      kind,
      linearId,
      linearName: target.name,
      teamId: target.teamId,
      workspaceId,
      projectName: project.name,
      accountId: account.accountId,
      now: new Date(now()).toISOString(),
    });
    capture(account.accountId, "linear_project_linked", { kind, scope: owner.kind });
    return { link: linkView(findLinearLink.get(owner.kind, owner.id, kind, linearId) as Row, account) };
  }

  // POST /api/app/linear/generate: a goal becomes 1 to 10 issues in a Linear
  // project (or team). They are not labeled `rudder`, so nothing starts until
  // the user labels one. Naming a Rudder project links the two first, so a
  // labeled issue knows where to start and agents there are tracked.
  async function generateLinearTasks(account: Account, body: Record<string, unknown>) {
    const goal = str(body.goal);
    if (!goal) {
      throw new HttpError(400, "goal is required");
    }
    if (goal.length > GOAL_LIMIT) {
      throw new HttpError(400, `goal is longer than ${GOAL_LIMIT} characters`);
    }
    const linearProjectId = str(body.linearProjectId);
    const teamParam = str(body.teamId);
    if (!linearProjectId && !teamParam) {
      throw new HttpError(400, "Choose a Linear project or team");
    }
    const kind: LinearKind = linearProjectId ? "project" : "team";
    const linearId = linearProjectId || teamParam;
    const owner = resolveOwner(account, str(body.org), "read");
    const existing = findLinearLink.get(owner.kind, owner.id, kind, linearId) as Row | undefined;
    const workspaceId = str(body.projectId) || str(body.workspace);
    let rudderProject: IntegrationProject | null = null;
    if (workspaceId) {
      rudderProject = findProject(account.accountId, workspaceId) ?? null;
      if (!rudderProject) {
        throw new HttpError(404, "project not found");
      }
      if (!owner.canManage && String(existing?.workspace_id ?? "") !== workspaceId) {
        throw new HttpError(403, "only an org admin can link a Linear project to a Rudder project");
      }
    } else if (existing) {
      rudderProject = findProject(String(existing.account_id), String(existing.workspace_id)) ?? {
        workspaceId: String(existing.workspace_id),
        name: String(existing.project_name ?? "project"),
      };
    }
    if (!taskgenConfigured(deps.env)) {
      throw new HttpError(503, "Task generation is not set up on this server (OPENAI_API_KEY is missing)");
    }
    const install = requireLinear(owner);
    const token = await linearAccessToken(install);
    await ensureLinearOrg(install, token);
    const target = existing && (!teamParam || teamParam === existing.team_id) && existing.team_id
      ? { name: String(existing.linear_name ?? ""), teamId: String(existing.team_id) }
      : await resolveLinearTarget(token, kind, linearId, teamParam);

    let generated: Awaited<ReturnType<typeof generateTasks>>;
    try {
      generated = await generateTasks({
        env: deps.env,
        fetch: deps.fetch,
        goal,
        project: rudderProject ? { name: rudderProject.name, repo: rudderProject.repo ?? null } : null,
        log,
      });
    } catch (error) {
      if (error instanceof TaskgenError) {
        throw new HttpError(error.status, error.message);
      }
      throw error;
    }
    if (workspaceId && rudderProject && String(existing?.workspace_id ?? "") !== workspaceId) {
      upsertLinearLink.run({
        ownerKind: owner.kind,
        ownerId: owner.id,
        kind,
        linearId,
        linearName: target.name || null,
        teamId: target.teamId,
        workspaceId,
        projectName: rudderProject.name,
        accountId: account.accountId,
        now: new Date(now()).toISOString(),
      });
    }
    const issues: CreatedLinearIssue[] = [];
    let failure: string | null = null;
    for (const task of generated.tasks) {
      try {
        issues.push(
          await createLinearIssue(deps.fetch, token, {
            teamId: target.teamId,
            ...(kind === "project" ? { projectId: linearId } : {}),
            title: task.title,
            description: linearIssueDescription(task),
          }),
        );
      } catch (error) {
        failure = errorText(error);
        log(`integrations: linear issueCreate failed: ${failure}`);
        break;
      }
    }
    if (issues.length === 0) {
      throw new HttpError(502, failure ?? "no issues were created");
    }
    capture(account.accountId, "linear_tasks_generated", { count: issues.length, provider: generated.provider, scope: owner.kind });
    const link = findLinearLink.get(owner.kind, owner.id, kind, linearId) as Row | undefined;
    if (link) {
      // The next tracking decision in this project sees the new issues.
      issueCache.delete(linkKey(link));
    }
    return {
      issues,
      provider: generated.provider,
      model: generated.model,
      link: linkView(link, account),
      ...(failure ? { error: `Created ${issues.length} of ${generated.tasks.length} issues: ${failure}` } : {}),
    };
  }

  // ---- Linear webhook: labeled issues start agents ----

  async function handleLinearWebhook(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const raw = await readBody(req);
    if (!linearConfig.webhookSecret) {
      sendJson(res, 503, { error: "Linear webhooks are not set up on this server" });
      return;
    }
    if (!verifyLinearSignature(linearConfig.webhookSecret, raw, header(req, "linear-signature"))) {
      sendJson(res, 401, { error: "invalid linear signature" });
      return;
    }
    let payload: Row;
    try {
      const parsed = JSON.parse(raw) as unknown;
      payload = parsed && typeof parsed === "object" && !Array.isArray(parsed) ? (parsed as Row) : {};
    } catch {
      sendJson(res, 400, { error: "invalid JSON body" });
      return;
    }
    if (!linearWebhookFresh(payload.webhookTimestamp, now())) {
      sendJson(res, 401, { error: "stale linear webhook" });
      return;
    }
    // Linear wants an answer in seconds; the start happens after it.
    sendJson(res, 200, { ok: true });
    track(onLinearEvent(payload).catch((error: unknown) => log(`integrations: linear webhook failed: ${errorText(error)}`)));
  }

  const labelNames = (value: unknown): string[] | null =>
    Array.isArray(value) ? value.map((label) => str((label as Row | null)?.name)) : null;

  async function onLinearEvent(payload: Row): Promise<void> {
    const action = str(payload.action);
    if (str(payload.type) !== "Issue" || (action !== "create" && action !== "update")) {
      return;
    }
    const data = (payload.data ?? {}) as Row;
    const issueId = str(data.id);
    const teamId = str(data.teamId) || str((data.team as Row | null)?.id);
    const projectId = str(data.projectId) || str((data.project as Row | null)?.id);
    const stateType = str((data.state as Row | null)?.type);
    if (!issueId || stateType === "completed" || stateType === "canceled") {
      return;
    }
    let labels = labelNames(data.labels);
    if (labels && !labels.some((name) => name.toLowerCase() === RUDDER_LABEL)) {
      return;
    }
    dropStaleClaim.run(issueId, now() - LINEAR_CLAIM_STALE_MS);
    if (findIssueRun.get(issueId)) {
      return; // started already
    }
    const orgId = str(payload.organizationId);
    const installs = orgId
      ? (linearInstallsForOrg.all(orgId) as Row[]).map(installFromRow).filter((value): value is Install => Boolean(value))
      : [];
    // The issue's Linear project picks the Rudder project; its team is the fallback.
    let route: { install: Install; link: Row } | null = null;
    for (const install of installs) {
      const link = (projectId ? findLinearLink.get(install.ownerKind, install.ownerId, "project", projectId) : undefined)
        ?? (teamId ? findLinearLink.get(install.ownerKind, install.ownerId, "team", teamId) : undefined);
      if (link && canUseInstall(String((link as Row).account_id), install)) {
        route = { install, link: link as Row };
        break;
      }
    }
    if (!route) {
      return;
    }
    if (!labels) {
      const ids = Array.isArray(data.labelIds) ? data.labelIds : [];
      if (ids.length === 0) {
        return;
      }
      labels = await linearIssueLabelNames(deps.fetch, await linearAccessToken(route.install), issueId);
      if (!labels.some((name) => name.toLowerCase() === RUDDER_LABEL)) {
        return;
      }
    }
    const accountId = String(route.link.account_id);
    const workspaceId = String(route.link.workspace_id);
    const claimed = claimIssueRun.run({
      issueId,
      ownerKind: route.install.ownerKind,
      ownerId: route.install.ownerId,
      accountId,
      workspaceId,
      projectName: route.link.project_name ?? null,
      now: now(),
    });
    if (claimed.changes !== 1) {
      return; // another delivery of the same event got here first
    }
    try {
      const { issue, project } = await deliverLinearIssue({ accountId, install: route.install, issueId, workspaceId });
      markIssueDelivered.run(issue.identifier, project.name, now(), issueId);
      capture(accountId, "linear_issue_started", { scope: route.install.ownerKind, source: "webhook" });
    } catch (error) {
      // Not started: a later update to the issue may try again.
      deleteIssueRun.run(issueId);
      log(`integrations: linear webhook start failed: ${errorText(error)}`);
    }
  }

  // ---- Linear: run status back to the issue ----

  // The latest needs-you view per issue, read when a needs hold runs out.
  const linearLatest = new Map<string, { category: NotifyCategory; agent: IntegrationAgent }>();
  const linearHolds = new Set<string>();

  function matchIssueRun(agents: IntegrationAgent[], identifier: string, since: number, claimed: Set<string>): IntegrationAgent | null {
    const needle = `Linear ${identifier}:`;
    let best: IntegrationAgent | null = null;
    for (const agent of agents) {
      if (!agent?.id || agent.pending === true || claimed.has(agent.id)) {
        continue;
      }
      if (!str(agent.task).includes(needle) && !str(agent.title).includes(needle)) {
        continue;
      }
      const created = typeof agent.createdAt === "number" ? agent.createdAt : null;
      if (created !== null && created < since - LINEAR_RUN_MATCH_SLACK_MS) {
        continue;
      }
      if (!best || (created ?? 0) > (typeof best.createdAt === "number" ? best.createdAt : 0)) {
        best = agent;
      }
    }
    return best;
  }

  async function linearTokenFor(row: Row): Promise<string | null> {
    const install = getInstall({ kind: String(row.owner_kind) as Owner["kind"], id: String(row.owner_id) }, "linear");
    return install ? linearAccessToken(install) : null;
  }

  // Moves an issue forward only: to started from backlog/unstarted/triage, to
  // completed from anything not already done or canceled.
  async function moveLinearIssue(token: string, issueId: string, target: "started" | "completed"): Promise<void> {
    const workflow = await linearIssueWorkflow(deps.fetch, token, issueId);
    if (!workflow) {
      return;
    }
    const from = workflow.stateType ?? "";
    if (target === "started" ? !["backlog", "unstarted", "triage"].includes(from) : ["completed", "canceled"].includes(from)) {
      return;
    }
    const state = firstStateOfType(workflow.states, target);
    if (!state) {
      log(`integrations: linear team has no ${target} state`);
      return;
    }
    await setLinearIssueState(deps.fetch, token, issueId, state.id);
  }

  async function step(what: string, run: () => Promise<void>): Promise<void> {
    try {
      await run();
    } catch (error) {
      log(`integrations: linear ${what} failed: ${errorText(error)}`);
    }
  }

  async function linearRunBegan(row: Row, project: IntegrationProject, runId: string): Promise<void> {
    const issueId = String(row.issue_id);
    const token = await linearTokenFor(row).catch((error: unknown) => {
      log(`integrations: linear token failed: ${errorText(error)}`);
      return null;
    });
    if (!token) {
      return;
    }
    await step("move to started", () => moveLinearIssue(token, issueId, "started"));
    if (claimStartComment.run(now(), issueId).changes === 1) {
      await step("start comment", () =>
        commentOnLinearIssue(deps.fetch, token, issueId, `Rudder started an agent on this in ${project.name}.\n\n${agentUrl(project.workspaceId, runId)}`),
      );
    }
  }

  async function linearRunNeedsYou(row: Row, project: IntegrationProject, runId: string): Promise<void> {
    const issueId = String(row.issue_id);
    const latest = linearLatest.get(issueId);
    if (latest?.category !== "needs" || claimNeedsComment.run(now(), issueId).changes !== 1) {
      return;
    }
    const token = await linearTokenFor(row).catch(() => null);
    if (!token) {
      return;
    }
    const words = NEEDS_WORDS[needsOf(latest.agent).reason ?? ""];
    await step("needs-you comment", () =>
      commentOnLinearIssue(
        deps.fetch,
        token,
        issueId,
        `The Rudder agent on this ${words ? `${words}.` : "needs you."}\n\n${agentUrl(project.workspaceId, runId)}`,
      ),
    );
  }

  async function linearRunMerged(row: Row): Promise<void> {
    const token = await linearTokenFor(row).catch(() => null);
    if (token) {
      await step("move to completed", () => moveLinearIssue(token, String(row.issue_id), "completed"));
    }
  }

  // Called with every home state. The DB row is claimed before any Linear
  // call, so overlapping states never comment or move twice.
  function syncLinearRuns(accountId: string, projects: IntegrationProject[]): void {
    const rows = activeIssueRuns.all(accountId, now() - LINEAR_SYNC_WINDOW_MS) as Row[];
    if (rows.length === 0) {
      return;
    }
    const byWorkspace = new Map((projects ?? []).map((project) => [project.workspaceId, project]));
    const claimed = new Set(rows.map((row) => (row.run_id ? String(row.run_id) : "")).filter(Boolean));
    for (const row of rows) {
      const issueId = String(row.issue_id);
      const project = byWorkspace.get(String(row.workspace_id));
      if (!project) {
        continue;
      }
      const agents = project.agents ?? [];
      const jobs: Array<() => Promise<void>> = [];
      let runId = row.run_id ? String(row.run_id) : "";
      if (!runId) {
        const agent = row.identifier ? matchIssueRun(agents, String(row.identifier), Number(row.created_at), claimed) : null;
        if (!agent || setIssueRunId.run(agent.id, now(), issueId).changes !== 1) {
          continue;
        }
        runId = agent.id;
        claimed.add(runId);
        jobs.push(() => linearRunBegan(row, project, runId));
      }
      const agent = agents.find((candidate) => candidate?.id === runId);
      if (agent) {
        if (normalizedStatus(agent as StatusAgent) === "merged") {
          linearLatest.delete(issueId);
          if (markIssueMerged.run(now(), issueId).changes === 1) {
            jobs.push(() => linearRunMerged(row));
          }
        } else {
          const category = agentCategory(agent);
          linearLatest.set(issueId, { category, agent });
          if (category === "needs" && !Number(row.needs_commented) && !linearHolds.has(issueId)) {
            // Held like Slack's needs-you: an Esc or a prompt answered in the
            // terminal reads as needs-you for a moment, and that is not news.
            if (needsDebounceMs === 0) {
              jobs.push(() => linearRunNeedsYou(row, project, runId));
            } else {
              linearHolds.add(issueId);
              let release: () => void = () => undefined;
              const settled = new Promise<void>((resolve) => { release = resolve; });
              pending.add(settled);
              const timer = setTimeout(() => {
                linearHolds.delete(issueId);
                void linearRunNeedsYou(row, project, runId).finally(() => {
                  pending.delete(settled);
                  release();
                });
              }, needsDebounceMs);
              timer.unref?.();
            }
          }
        }
      }
      if (jobs.length) {
        track((async () => {
          for (const job of jobs) {
            await job();
          }
        })());
      }
    }
  }

  // ---- Linear: agents started in Rudder tracked as Linear issues ----

  type TrackRoute = { install: Install; link: Row };
  type TrackView = { project: IntegrationProject; agent: IntegrationAgent };

  // A linked Linear project's open issues, read at most every TRACK_ISSUE_CACHE_MS.
  const issueCache = new Map<string, { at: number; issues: LinearIssue[] }>();
  const trackInFlight = new Set<string>();
  // The newest view of a run being decided: a merge that lands while the model
  // answers is synced as soon as the decision is.
  const trackLatest = new Map<string, TrackView>();
  const trackQueues = new Map<string, Promise<void>>();
  let trackNoModelLogged = false;

  const runKey = (workspaceId: string, runId: string) => `${workspaceId}\0${runId}`;
  const linkKey = (link: Row) => `${String(link.owner_kind)}:${String(link.owner_id)}:${String(link.linear_kind)}:${String(link.linear_id)}`;

  // Tracking work runs one job at a time per Rudder project: each decision
  // sees the issue the one before it created, and an issue never moves to
  // started after it moved to completed.
  function enqueueTrack(workspaceId: string, job: () => Promise<void>): void {
    const previous = trackQueues.get(workspaceId) ?? Promise.resolve();
    const next = previous.then(job).catch((error: unknown) => log(`integrations: linear tracking failed: ${errorText(error)}`));
    trackQueues.set(workspaceId, next);
    track(next.finally(() => {
      if (trackQueues.get(workspaceId) === next) {
        trackQueues.delete(workspaceId);
      }
    }));
  }

  function trackRoute(accountId: string, links: Row[]): TrackRoute | null {
    for (const link of links) {
      const install = getInstall({ kind: String(link.owner_kind) as Owner["kind"], id: String(link.owner_id) }, "linear");
      if (install && canUseInstall(accountId, install)) {
        return { install, link };
      }
    }
    return null;
  }

  async function openIssuesFor(link: Row, token: string): Promise<LinearIssue[]> {
    const key = linkKey(link);
    const cached = issueCache.get(key);
    if (cached && now() - cached.at < TRACK_ISSUE_CACHE_MS) {
      return cached.issues;
    }
    const scope = link.linear_kind === "project" ? { projectId: String(link.linear_id) } : { teamId: String(link.linear_id) };
    const issues = await listOpenLinearIssues(deps.fetch, token, scope);
    issueCache.set(key, { at: now(), issues });
    return issues;
  }

  function forgetCachedIssue(issueId: string): void {
    for (const entry of issueCache.values()) {
      entry.issues = entry.issues.filter((issue) => issue.id !== issueId);
    }
  }

  // Called with every home state; only database reads and claims happen here.
  // Model and Linear calls run in the background.
  function syncTrackedRuns(accountId: string, projects: IntegrationProject[]): void {
    const links = linksForAccount.all(accountId) as Row[];
    if (links.length === 0) {
      return;
    }
    const linksByWorkspace = new Map<string, Row[]>();
    for (const link of links) {
      const workspaceId = String(link.workspace_id);
      linksByWorkspace.set(workspaceId, [...(linksByWorkspace.get(workspaceId) ?? []), link]);
    }
    const active = new Map(
      (activeTrackedRuns.all(accountId, now() - LINEAR_SYNC_WINDOW_MS) as Row[]).map((row) => [runKey(String(row.workspace_id), String(row.run_id)), row]),
    );
    for (const project of projects ?? []) {
      const projectLinks = linksByWorkspace.get(project.workspaceId);
      if (!projectLinks) {
        continue;
      }
      // Runs created before tracking started, or before this project was linked, are never backfilled.
      const linkedAt = Math.min(...projectLinks.map((link) => Date.parse(String(link.updated_at)) || 0));
      const since = Math.max(trackStartedAt, linkedAt);
      let route: TrackRoute | null | undefined;
      for (const agent of project.agents ?? []) {
        if (!agent?.id || agent.pending === true) {
          continue;
        }
        const key = runKey(project.workspaceId, agent.id);
        const row = active.get(key);
        if (row) {
          syncTrackedRun(row, { project, agent });
          continue;
        }
        if (trackInFlight.has(key)) {
          trackLatest.set(key, { project, agent });
          continue;
        }
        if (typeof agent.createdAt !== "number" || agent.createdAt < since || untrackedReason(agent) || !userTask(agent.task)) {
          continue;
        }
        const decided = findTracked.get(project.workspaceId, agent.id) as Row | undefined;
        if (decided) {
          if (decided.action !== "deciding" || dropStaleTrackClaim.run(project.workspaceId, agent.id, now() - TRACK_CLAIM_STALE_MS).changes !== 1) {
            continue;
          }
        }
        if (issueRunForRun.get(agent.id)) {
          continue; // started from a Linear issue: that issue already tracks it
        }
        if (!taskgenConfigured(deps.env)) {
          if (!trackNoModelLogged) {
            trackNoModelLogged = true;
            log("integrations: linear tracking is off: no OPENAI_API_KEY or ANTHROPIC_API_KEY on this server");
          }
          continue;
        }
        if (route === undefined) {
          route = trackRoute(accountId, projectLinks);
        }
        // A private agent's work stays out of an org's shared Linear workspace.
        if (!route || (agent.visibility === "private" && route.install.ownerKind === "org")) {
          continue;
        }
        const claimed = claimTracked.run({
          workspaceId: project.workspaceId,
          runId: agent.id,
          accountId,
          ownerKind: route.install.ownerKind,
          ownerId: route.install.ownerId,
          title: String(agent.title || userTask(agent.task)).slice(0, TRACK_TITLE_LIMIT),
          now: now(),
        });
        if (claimed.changes !== 1) {
          continue;
        }
        trackInFlight.add(key);
        trackLatest.set(key, { project, agent });
        const chosen = route;
        enqueueTrack(project.workspaceId, () => decideTrackedRun(accountId, project, agent, chosen));
      }
    }
  }

  // One model call, then the decision is stored (a failure is a skip, so a run
  // is never classified twice). An attach or create then syncs like any
  // tracked run.
  async function decideTrackedRun(accountId: string, project: IntegrationProject, agent: IntegrationAgent, route: TrackRoute): Promise<void> {
    const key = runKey(project.workspaceId, agent.id);
    const task = userTask(agent.task);
    let decided: { action: TrackDecision["action"]; issueId: string | null; identifier: string | null; created: boolean; reason: string } = {
      action: "skip",
      issueId: null,
      identifier: null,
      created: false,
      reason: "",
    };
    try {
      const token = await linearAccessToken(route.install);
      const issues = await openIssuesFor(route.link, token);
      const tracked = (recentTracked.all(project.workspaceId, now() - TRACK_RECENT_MS) as Row[]).map((row) => ({
        title: String(row.agent_title ?? ""),
        identifier: String(row.identifier ?? ""),
      }));
      let decision: TrackDecision;
      try {
        decision = await classifyWork({
          env: deps.env,
          fetch: deps.fetch,
          task,
          project: { name: project.name, repo: project.repo ?? null },
          linearName: route.link.linear_name ? String(route.link.linear_name) : null,
          issues,
          tracked,
          log,
        });
      } catch (error) {
        throw new Error(`the model could not decide: ${errorText(error)}`);
      }
      if (decision.action === "attach") {
        decided = { action: "attach", issueId: decision.issue.id, identifier: decision.issue.identifier, created: false, reason: decision.reason };
      } else if (decision.action === "create") {
        const teamId = str(route.link.team_id);
        if (!teamId) {
          throw new Error("the linked Linear project has no team");
        }
        // No labels: only an issue the user labels `rudder` starts an agent.
        const issue = await createLinearIssue(deps.fetch, token, {
          teamId,
          ...(route.link.linear_kind === "project" ? { projectId: String(route.link.linear_id) } : {}),
          title: decision.title,
          description: trackedIssueDescription(task, project.name, agentUrl(project.workspaceId, agent.id)),
        });
        decided = { action: "create", issueId: issue.id, identifier: issue.identifier, created: true, reason: decision.reason };
        issueCache.get(linkKey(route.link))?.issues.unshift({
          id: issue.id,
          identifier: issue.identifier,
          title: issue.title || decision.title,
          url: issue.url,
          state: null,
          stateType: "started",
          priority: null,
          team: null,
        });
      } else {
        decided = { ...decided, reason: decision.reason };
      }
    } catch (error) {
      decided = { action: "skip", issueId: null, identifier: null, created: false, reason: errorText(error).slice(0, 300) };
      log(`integrations: linear tracking skipped ${agent.id}: ${errorText(error)}`);
    } finally {
      trackInFlight.delete(key);
      decideTracked.run({
        workspaceId: project.workspaceId,
        runId: agent.id,
        action: decided.action,
        issueId: decided.issueId,
        identifier: decided.identifier,
        created: decided.created ? 1 : 0,
        reason: decided.reason || null,
        now: now(),
      });
    }
    const latest = trackLatest.get(key) ?? { project, agent };
    trackLatest.delete(key);
    if (decided.action !== "skip") {
      const row = findTracked.get(project.workspaceId, agent.id) as Row | undefined;
      if (row) {
        syncTrackedRun(row, latest);
      }
    }
  }

  // Started when tracking begins (no comment); one "Merged:" comment when the
  // agent merges, and completed only for an issue this run created. Each step
  // is claimed in the row before Linear is called; failures are only logged.
  function syncTrackedRun(row: Row, view: TrackView): void {
    const workspaceId = String(row.workspace_id);
    const runId = String(row.run_id);
    const issueId = str(row.issue_id);
    if (!issueId) {
      return;
    }
    const start = !Number(row.started_synced) && claimTrackStarted.run(now(), workspaceId, runId).changes === 1;
    const merged = normalizedStatus(view.agent as StatusAgent) === "merged" && claimTrackMerged.run(now(), workspaceId, runId).changes === 1;
    if (!start && !merged) {
      return;
    }
    enqueueTrack(workspaceId, async () => {
      const token = await linearTokenFor(row).catch((error: unknown) => {
        log(`integrations: linear token failed: ${errorText(error)}`);
        return null;
      });
      if (!token) {
        return;
      }
      if (start) {
        await step("move to started", () => moveLinearIssue(token, issueId, "started"));
      }
      if (merged) {
        await step("merge comment", () =>
          commentOnLinearIssue(deps.fetch, token, issueId, `Merged: ${mergeLine(view.agent)}\n\n${agentUrl(workspaceId, runId)}`),
        );
        if (Number(row.created_issue)) {
          await step("move to completed", () => moveLinearIssue(token, issueId, "completed"));
          forgetCachedIssue(issueId);
        }
      }
    });
  }

  async function routeApp(req: IncomingMessage, res: ServerResponse, url: URL, account: Account): Promise<boolean> {
    const path = url.pathname;
    const method = req.method ?? "GET";
    const orgParam = url.searchParams.get("org") ?? "";

    if (method === "GET" && path === "/api/app/integrations") {
      sendJson(res, 200, summary(account, orgParam));
      return true;
    }
    const startMatch = /^\/api\/app\/integrations\/(slack|linear)\/start$/.exec(path);
    if (startMatch && method === "POST") {
      const body = await readJson(req);
      sendJson(res, 200, { url: startOAuth(account, str(body.org) || orgParam, startMatch[1] as Provider) });
      return true;
    }
    const providerMatch = /^\/api\/app\/integrations\/(slack|linear)$/.exec(path);
    if (providerMatch && method === "DELETE") {
      await disconnect(account, orgParam, providerMatch[1] as Provider);
      sendJson(res, 200, { ok: true });
      return true;
    }
    if (method === "GET" && path === "/api/app/integrations/slack/channels") {
      const install = requireSlack(resolveOwner(account, orgParam, "manage"));
      const channels = await listSlackChannels(deps.fetch, str(install.token.accessToken)).catch((error: unknown) => {
        throw new HttpError(502, error instanceof Error ? error.message : String(error));
      });
      sendJson(res, 200, { channels });
      return true;
    }
    if (method === "POST" && path === "/api/app/integrations/slack/channel") {
      const body = await readJson(req);
      const owner = resolveOwner(account, str(body.org) || orgParam, "manage");
      const install = requireSlack(owner);
      const wanted = str(body.channel).replace(/^#/, "");
      if (!wanted) {
        throw new HttpError(400, "channel is required");
      }
      const channels = await listSlackChannels(deps.fetch, str(install.token.accessToken)).catch((error: unknown) => {
        throw new HttpError(502, error instanceof Error ? error.message : String(error));
      });
      const channel = channels.find((candidate) => candidate.id === wanted) ?? channels.find((candidate) => candidate.name === wanted);
      if (!channel) {
        throw new HttpError(404, "channel not found; invite the Rudder app to a private channel first");
      }
      updateChannel.run(channel.id, channel.name, new Date(now()).toISOString(), owner.kind, owner.id);
      sendJson(res, 200, { channel });
      return true;
    }
    if (method === "GET" && path === "/api/app/linear/issues") {
      const install = requireLinear(resolveOwner(account, orgParam, "read"));
      const token = await linearAccessToken(install);
      const issues = await listLinearIssues(deps.fetch, token, url.searchParams.get("q") ?? "");
      sendJson(res, 200, {
        issues: issues.map(({ id, identifier, title, url: issueUrl, state, priority, team }) => ({
          id,
          identifier,
          title,
          url: issueUrl,
          state,
          priority,
          team,
        })),
      });
      return true;
    }
    if (method === "POST" && path === "/api/app/linear/start") {
      sendJson(res, 200, await startLinearIssue(account, await readJson(req)));
      return true;
    }
    if (method === "GET" && path === "/api/app/linear/setup") {
      sendJson(res, 200, await linearSetup(account, orgParam));
      return true;
    }
    if (method === "GET" && path === "/api/app/linear/teams") {
      const setup = await linearSetup(account, orgParam);
      sendJson(res, 200, { teams: setup.teams, canManage: setup.canManage });
      return true;
    }
    if (method === "POST" && path === "/api/app/linear/mapping") {
      sendJson(res, 200, await setLinearLink(account, await readJson(req), orgParam));
      return true;
    }
    if (method === "POST" && path === "/api/app/linear/generate") {
      sendJson(res, 200, await generateLinearTasks(account, await readJson(req)));
      return true;
    }
    const linkMatch = /^\/api\/app\/link\/slack\/([A-Za-z0-9_-]{8,128})$/.exec(path);
    if (linkMatch && (method === "GET" || method === "POST")) {
      const codeHash = sha256(linkMatch[1]);
      const row = findLinkCode.get(codeHash) as Row | undefined;
      if (!row) {
        throw new HttpError(404, "link not found; run /rudder link again");
      }
      if (Number(row.expires_at) < now()) {
        deleteLinkCode.run(codeHash);
        throw new HttpError(410, "link expired; run /rudder link again");
      }
      const team = { id: String(row.team_id), name: row.team_name ? String(row.team_name) : "Slack" };
      if (method === "GET") {
        sendJson(res, 200, { team, slackUser: row.user_name ? String(row.user_name) : null, expiresAt: new Date(Number(row.expires_at)).toISOString() });
        return true;
      }
      deleteLinkCode.run(codeHash);
      upsertLink.run(team.id, String(row.user_id), account.accountId, new Date(now()).toISOString());
      sendJson(res, 200, { ok: true, team });
      return true;
    }
    return false;
  }

  async function handleAppApi(req: IncomingMessage, res: ServerResponse, url: URL, account: Account): Promise<boolean> {
    const path = url.pathname;
    if (!path.startsWith("/api/app/integrations") && !path.startsWith("/api/app/linear/") && !path.startsWith("/api/app/link/slack/")) {
      return false;
    }
    try {
      if (await routeApp(req, res, url, account)) {
        return true;
      }
      sendJson(res, 404, { error: "not found" });
    } catch (error) {
      const status = error instanceof HttpError ? error.status : Number((error as { status?: number })?.status) || 500;
      sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
    }
    return true;
  }

  // ---- Slack slash command ----

  function slackAccountFor(teamId: string, userId: string): { accountId: string | null; installs: Install[] } {
    const installs = (slackInstallsForTeam.all(teamId) as Row[]).map(installFromRow).filter((value): value is Install => Boolean(value));
    const link = findLink.get(teamId, userId) as Row | undefined;
    return { accountId: link ? String(link.account_id) : null, installs };
  }

  function canUseInstall(accountId: string, install: Install): boolean {
    return install.ownerKind === "account"
      ? install.ownerId === accountId
      : deps.orgRole(accountId, install.ownerId) !== null;
  }

  function agentUrl(workspaceId: string, runId: string): string {
    return `${baseURL}/app/p/${encodeURIComponent(workspaceId)}/a/${encodeURIComponent(runId)}`;
  }

  function projectList(projects: IntegrationProject[]): string {
    if (projects.length === 0) {
      return "No projects on your cloud machine yet. Open one in Rudder first.";
    }
    return ["Your projects:", ...projects.slice(0, 30).map((project) => `• \`${slackEscape(project.name)}\`${project.repo ? ` (${slackEscape(project.repo)})` : ""}`)].join("\n");
  }

  function statusText(accountId: string): string {
    const lines: string[] = [];
    for (const project of deps.listProjects(accountId)) {
      for (const agent of project.agents ?? []) {
        const category = agentCategory(agent);
        if (category === "needs" || category === "failed" || agent.status === "review") {
          lines.push(`• *${slackEscape(project.name)}* · ${slackLink(agentUrl(project.workspaceId, agent.id), agent.title || agent.id)} · ${statusLine(agent, category)}`);
        }
      }
    }
    if (lines.length === 0) {
      return "Nothing needs you right now.";
    }
    const shown = lines.slice(0, 15);
    if (lines.length > shown.length) {
      shown.push(`…and ${lines.length - shown.length} more`);
    }
    return ["These agents need you:", ...shown].join("\n");
  }

  async function respondLater(responseUrl: string, text: string): Promise<void> {
    let target: URL;
    try {
      target = new URL(responseUrl);
    } catch {
      return;
    }
    if (target.protocol !== "https:" || !(target.hostname === "slack.com" || target.hostname.endsWith(".slack.com"))) {
      return;
    }
    await deps.fetch(target.toString(), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ response_type: "ephemeral", replace_original: false, text }),
    }).catch((error: unknown) => log(`integrations: slack response_url failed: ${String(error)}`));
  }

  async function slashCommand(form: URLSearchParams): Promise<string> {
    const teamId = form.get("team_id") ?? "";
    const userId = form.get("user_id") ?? "";
    const command = parseSlashCommand(form.get("text") ?? "");
    if (command.action === "help") {
      return SLASH_HELP_TEXT;
    }
    const { accountId, installs } = slackAccountFor(teamId, userId);
    if (installs.length === 0) {
      return "Rudder is not connected to this Slack workspace. Connect it from Rudder's settings.";
    }
    if (command.action === "link") {
      pruneLinkCodes.run(now());
      const code = randomBytes(24).toString("base64url");
      insertLinkCode.run(sha256(code), teamId, form.get("team_domain") ?? installs[0].teamName, userId, form.get("user_name") ?? null, now() + LINK_CODE_TTL_MS);
      return `Open this link to connect your Slack user to Rudder (works once, for 10 minutes):\n${baseURL}/app/link/slack/${code}`;
    }
    if (!accountId) {
      return "Your Slack user is not linked to Rudder yet. Run `/rudder link` first.";
    }
    if (!installs.some((install) => canUseInstall(accountId, install))) {
      return "Your linked Rudder account is not in the org that connected this Slack workspace. Ask an admin for an invite, or `/rudder link` another account.";
    }
    const projects = deps.listProjects(accountId);
    if (command.action === "status") {
      return statusText(accountId);
    }
    if (command.action === "projects") {
      return projectList(projects);
    }
    const found = matchProject(projects, command.project);
    if (!found.match) {
      const head = found.ambiguous
        ? `\`${slackEscape(command.project)}\` matches more than one project.`
        : `No project called \`${slackEscape(command.project)}\`.`;
      return `${head}\n${projectList(found.ambiguous ?? projects)}`;
    }
    if (command.action === "usage") {
      return `Say what to do: \`/rudder ${slackEscape(found.match.name)} <task>\``;
    }
    const project = found.match;
    const task = command.task.slice(0, 16_000);
    const responseUrl = form.get("response_url") ?? "";
    track(
      deps.deliverTask(accountId, project.workspaceId, task).then(
        () => {
          capture(accountId, "task_started", { source: "slack" });
          return respondLater(responseUrl, `Started an agent on *${slackEscape(project.name)}*.`);
        },
        (error: unknown) =>
          respondLater(responseUrl, `Could not start the task on *${slackEscape(project.name)}*: ${slackEscape(error instanceof Error ? error.message : String(error))}`),
      ),
    );
    return `Starting on *${slackEscape(project.name)}*: ${slackEscape(task.length > 200 ? `${task.slice(0, 200)}…` : task)}`;
  }

  async function handleSlackCommand(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const raw = await readBody(req);
    if (!slackConfig.signingSecret) {
      sendJson(res, 503, { error: "Slack is not set up on this server" });
      return;
    }
    const verified = verifySlackSignature({
      signingSecret: slackConfig.signingSecret,
      timestamp: header(req, "x-slack-request-timestamp"),
      signature: header(req, "x-slack-signature"),
      rawBody: raw,
      now: now(),
    });
    if (!verified) {
      sendJson(res, 401, { error: "invalid slack signature" });
      return;
    }
    const form = new URLSearchParams(raw);
    if (form.get("ssl_check") === "1") {
      res.writeHead(200);
      res.end();
      return;
    }
    let text: string;
    try {
      text = await slashCommand(form);
    } catch (error) {
      log(`integrations: slash command failed: ${error instanceof Error ? error.message : String(error)}`);
      text = "Something went wrong on Rudder's side. Try again in a moment.";
    }
    sendJson(res, 200, { response_type: "ephemeral", text });
  }

  async function handlePublicApi(req: IncomingMessage, res: ServerResponse, url: URL): Promise<boolean> {
    const callback = /^\/api\/integrations\/(slack|linear)\/callback$/.exec(url.pathname);
    if (callback && req.method === "GET") {
      redirect(res, await finishOAuth(callback[1] as Provider, url));
      return true;
    }
    const handler = req.method === "POST"
      ? url.pathname === "/api/integrations/slack/commands"
        ? handleSlackCommand
        : url.pathname === "/api/integrations/linear/webhook"
          ? handleLinearWebhook
          : null
      : null;
    if (handler) {
      try {
        await handler(req, res);
      } catch (error) {
        const status = error instanceof HttpError ? error.status : 500;
        sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
      }
      return true;
    }
    return false;
  }

  // ---- Notifications ----

  function notifyChannels(accountId: string): Install[] {
    const owners: Array<Pick<Owner, "kind" | "id">> = [{ kind: "account", id: accountId }];
    let orgIds: string[] = [];
    try {
      orgIds = deps.orgIdsForAccount(accountId);
    } catch {
      orgIds = [];
    }
    for (const orgId of orgIds) {
      owners.push({ kind: "org", id: orgId });
    }
    return owners
      .map((owner) => getInstall(owner, "slack"))
      .filter((install): install is Install => Boolean(install?.channelId));
  }

  function notificationText(project: IntegrationProject, agent: IntegrationAgent, category: NotifyCategory, redact = false): string {
    const icon = category === "needs" ? ":raising_hand:" : category === "failed" ? ":x:" : ":white_check_mark:";
    const title = redact ? "Private agent" : agent.title || agent.id;
    return `${icon} *${slackEscape(project.name)}* · ${slackEscape(title)} ${statusLine(agent, category)}\n${slackLink(agentUrl(project.workspaceId, agent.id), "Open in Rudder")}`;
  }

  async function onHomeState(accountId: string, projects: IntegrationProject[]): Promise<void> {
    try {
      syncLinearRuns(accountId, projects);
    } catch (error) {
      log(`integrations: linear sync failed: ${errorText(error)}`);
    }
    try {
      syncTrackedRuns(accountId, projects);
    } catch (error) {
      log(`integrations: linear tracking failed: ${errorText(error)}`);
    }
    try {
      const first = !memory.has(accountId);
      const known = memory.get(accountId) ?? new Map<string, AgentMemory>();
      memory.set(accountId, known);
      const seen = new Set<string>();
      // A private agent posts its title only to the person's own channel; an
      // org channel (teammates read it) gets "Private agent".
      const posts: Array<{ full: string; org: string }> = [];
      const at = now();
      const held: Array<{ entry: AgentMemory; key: string; post: { full: string; org: string } }> = [];
      for (const project of projects ?? []) {
        for (const agent of project.agents ?? []) {
          if (!agent?.id) {
            continue;
          }
          const key = `${project.workspaceId}\0${agent.id}`;
          seen.add(key);
          const category = agentCategory(agent);
          const previous = known.get(key);
          const entry: AgentMemory = previous ?? { category, posted: new Map(), turnAt: 0 };
          if (category === "working") {
            // A new turn: the next needs-you or done is news again.
            entry.turnAt = at;
          }
          const notifiable = category === "needs" || category === "ready" || category === "failed";
          const changed = !previous || previous.category !== category;
          if (changed && entry.held) {
            // It left needs-you before the hold ran out: a blip, not news.
            entry.held.cancel();
          }
          const lastPost = entry.posted.get(category);
          const fresh = lastPost === undefined || (entry.turnAt > lastPost && at - lastPost >= NOTIFY_REPEAT_MS);
          if (!first && notifiable && changed && fresh) {
            const full = notificationText(project, agent, category);
            const post = { full, org: agent.visibility === "private" ? notificationText(project, agent, category, true) : full };
            if (category === "needs" && needsDebounceMs > 0) {
              held.push({ entry, key, post });
            } else {
              posts.push(post);
              entry.posted.set(category, at);
            }
          }
          entry.category = category;
          known.set(key, entry);
        }
      }
      for (const [key, entry] of known) {
        if (!seen.has(key)) {
          entry.held?.cancel();
          known.delete(key);
        }
      }
      for (const { entry, key, post } of held) {
        entry.held?.cancel();
        let release: () => void = () => undefined;
        const settled = new Promise<void>((resolve) => { release = resolve; });
        pending.add(settled);
        const done = () => {
          pending.delete(settled);
          release();
        };
        const hold = {
          cancel: () => {
            clearTimeout(timer);
            if (entry.held === hold) {
              entry.held = undefined;
            }
            done();
          },
        };
        // Posted only if the agent still needs you when the hold runs out; a
        // later state that moves it on cancels the hold (above).
        const timer = setTimeout(() => {
          entry.held = undefined;
          if (known.get(key) === entry && entry.category === "needs") {
            entry.posted.set("needs", now());
            void sendPosts(accountId, [post]).finally(done);
          } else {
            done();
          }
        }, needsDebounceMs);
        timer.unref?.();
        entry.held = hold;
      }
      await sendPosts(accountId, posts);
    } catch (error) {
      log(`integrations: onHomeState failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function sendPosts(accountId: string, posts: Array<{ full: string; org: string }>): Promise<void> {
    if (posts.length === 0) {
      return;
    }
    const channels = notifyChannels(accountId);
    await Promise.all(
      channels.flatMap((install) =>
        posts.map((post) =>
          postSlack(deps.fetch, str(install.token.accessToken), install.channelId!, install.ownerKind === "org" ? post.org : post.full).catch((error: unknown) =>
            log(`integrations: slack notify failed for ${install.ownerKind}:${install.ownerId}: ${error instanceof Error ? error.message : String(error)}`),
          ),
        ),
      ),
    );
  }

  return {
    handleAppApi,
    handlePublicApi,
    onHomeState,
    configured,
    // Resolves once background work (slash-command task delivery) settles.
    idle: async () => {
      while (pending.size > 0) {
        await Promise.allSettled(Array.from(pending));
      }
    },
  };
}
