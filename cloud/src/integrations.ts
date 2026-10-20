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
// handlePublicApi (OAuth callbacks and the Slack slash command) and
// onHomeState (agent status changes, posted to Slack). Everything it needs
// from the rest of the control plane is injected.

import { createCipheriv, createDecipheriv, createHash, createHmac, hkdfSync, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Database } from "better-sqlite3";
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
  exchangeLinearCode,
  getLinearIssue,
  linearAuthorizeUrl,
  linearTaskText,
  linearWorkspace,
  listLinearIssues,
  refreshLinearToken,
  revokeLinearToken,
  type LinearToken,
} from "./linear.js";

export type Provider = "slack" | "linear";
export type OrgRole = "admin" | "member";

export type IntegrationAgent = {
  id: string;
  title?: string | null;
  status?: string | null;
  asks?: unknown[] | null;
  /** "private" (team.ts): an org channel gets no title for it. */
  visibility?: string | null;
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
};

export type Account = { accountId: string; email?: string };

const STATE_TTL_MS = 10 * 60 * 1000;
const LINK_CODE_TTL_MS = 10 * 60 * 1000;
// The same agent reaching the same status again inside this window (a flap
// between waiting and working, say) is not news.
const NOTIFY_REPEAT_MS = 60 * 1000;
const MAX_BODY_BYTES = 256 * 1024;

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
type AgentMemory = { category: NotifyCategory; posted: Map<NotifyCategory, number>; turnAt: number };

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

function agentCategory(agent: IntegrationAgent): NotifyCategory {
  const status = String(agent.status ?? "");
  if ((Array.isArray(agent.asks) && agent.asks.length > 0) || status === "waiting") {
    return "needs";
  }
  if (status === "review" || status === "done") {
    return "ready";
  }
  if (status === "failed") {
    return "failed";
  }
  if (status === "working" || status === "starting") {
    return "working";
  }
  return "other";
}

function statusLine(agent: IntegrationAgent, category: NotifyCategory): string {
  if (category === "needs") {
    return "needs you";
  }
  if (category === "failed") {
    return "failed";
  }
  return agent.status === "review" ? "ready for review" : "done";
}

export type Integrations = ReturnType<typeof createIntegrations>;

export function createIntegrations(deps: IntegrationsDeps) {
  const { database } = deps;
  const now = deps.now ?? Date.now;
  const log = deps.log ?? ((message: string) => console.warn(message));
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
  `);

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
        const workspace = await linearWorkspace(deps.fetch, token.accessToken).catch(() => ({ name: "Linear", urlKey: null }));
        storeInstall({ owner, provider, token, workspaceName: workspace.name, installedBy: state.a });
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

  async function startLinearIssue(account: Account, body: Record<string, unknown>) {
    const owner = resolveOwner(account, str(body.org), "read");
    const issueId = str(body.issueId);
    const workspaceId = str(body.workspace);
    if (!issueId || !workspaceId) {
      throw new HttpError(400, "issueId and workspace are required");
    }
    const project = deps.listProjects(account.accountId).find((candidate) => candidate.workspaceId === workspaceId);
    if (!project) {
      throw new HttpError(404, "project not found");
    }
    const install = requireLinear(owner);
    const token = await linearAccessToken(install);
    const issue = await getLinearIssue(deps.fetch, token, issueId);
    if (!issue) {
      throw new HttpError(404, "Linear issue not found");
    }
    const text = linearTaskText(issue);
    await deps.deliverTask(account.accountId, workspaceId, text);
    try {
      await commentOnLinearIssue(deps.fetch, token, issue.id, `Rudder started an agent on this in ${project.name}.`);
    } catch (error) {
      log(`integrations: linear comment failed: ${error instanceof Error ? error.message : String(error)}`);
    }
    capture(account.accountId, "linear_issue_started", { scope: owner.kind });
    return { ok: true, text };
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
    if (url.pathname === "/api/integrations/slack/commands" && req.method === "POST") {
      try {
        await handleSlackCommand(req, res);
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
      const first = !memory.has(accountId);
      const known = memory.get(accountId) ?? new Map<string, AgentMemory>();
      memory.set(accountId, known);
      const seen = new Set<string>();
      // A private agent posts its title only to the person's own channel; an
      // org channel (teammates read it) gets "Private agent".
      const posts: Array<{ full: string; org: string }> = [];
      const at = now();
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
          const lastPost = entry.posted.get(category);
          const fresh = lastPost === undefined || (entry.turnAt > lastPost && at - lastPost >= NOTIFY_REPEAT_MS);
          if (!first && notifiable && changed && fresh) {
            const full = notificationText(project, agent, category);
            posts.push({ full, org: agent.visibility === "private" ? notificationText(project, agent, category, true) : full });
            entry.posted.set(category, at);
          }
          entry.category = category;
          known.set(key, entry);
        }
      }
      for (const key of known.keys()) {
        if (!seen.has(key)) {
          known.delete(key);
        }
      }
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
    } catch (error) {
      log(`integrations: onHomeState failed: ${error instanceof Error ? error.message : String(error)}`);
    }
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
