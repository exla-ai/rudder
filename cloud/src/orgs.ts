// Orgs: a named group of accounts that share integrations (Slack, Linear).
// Two roles: admins add and remove people and change roles, members just
// belong. People join through invite links (no email is ever sent): a link is
// single use, lasts 7 days, and may be locked to one email address. The token
// is only ever returned when the invite is created; the table keeps its sha256.
//
// Invariant: an org always has at least one admin. Removing, demoting or
// leaving as the last admin is refused with 409.
//
// The store is pure over a better-sqlite3 handle; handleOrgApi routes the
// /api/app/orgs* and /api/app/invites* paths with injected helpers so this
// module never imports server.ts.

import { createHash, randomBytes } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import type Database from "better-sqlite3";

export type OrgRole = "admin" | "member";

export const ORG_ROLES: readonly OrgRole[] = ["admin", "member"];
export const ORG_NAME_MAX = 60;
export const INVITE_TTL_MS = 7 * 24 * 3600_000;
export const LAST_ADMIN_MESSAGE = "an org needs at least one admin";

export class OrgError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "OrgError";
    this.status = status;
  }
}

export type OrgSummary = { id: string; name: string; role: OrgRole; memberCount: number; createdAt: string };

export type OrgMemberView = {
  accountId: string;
  email: string | null;
  name: string | null;
  role: OrgRole;
  joinedAt: string;
  you: boolean;
};

export type OrgInviteView = { id: string; email: string | null; role: OrgRole; createdAt: string; expiresAt: string };

export type OrgView = {
  org: { id: string; name: string; createdAt: string };
  role: OrgRole;
  members: OrgMemberView[];
  invites: OrgInviteView[];
};

export type CreatedInvite = OrgInviteView & { token: string };

export type InviteInfo = {
  org: { id: string; name: string };
  role: OrgRole;
  email: string | null;
  expiresAt: string;
  alreadyMember: boolean;
};

export type OrgActor = { accountId: string; email?: string | null };

type OrgRow = { id: string; name: string; created_at: number };
type MemberRow = { org_id: string; account_id: string; role: OrgRole; email: string | null; joined_at: number };
type InviteRow = {
  id: string;
  org_id: string;
  token_hash: string;
  email: string | null;
  role: OrgRole;
  created_by: string;
  created_at: number;
  expires_at: number;
  accepted_at: number | null;
  accepted_by: string | null;
  revoked_at: number | null;
};

export function hashInviteToken(token: string): string {
  return createHash("sha256").update(token).digest("hex");
}

/** A trimmed org name of 1 to 60 characters, or an OrgError(400). */
export function validOrgName(value: unknown): string {
  const name = typeof value === "string" ? value.trim().replace(/\s+/g, " ") : "";
  if (!name) {
    throw new OrgError(400, "an org needs a name");
  }
  if (name.length > ORG_NAME_MAX) {
    throw new OrgError(400, `an org name is at most ${ORG_NAME_MAX} characters`);
  }
  return name;
}

export function validRole(value: unknown): OrgRole {
  if (value === "admin" || value === "member") {
    return value;
  }
  throw new OrgError(400, "role must be admin or member");
}

function normalizeEmail(value: unknown): string | null {
  if (typeof value !== "string") {
    return null;
  }
  const email = value.trim().toLowerCase();
  return email || null;
}

function validInviteEmail(value: unknown): string | null {
  if (value === undefined || value === null || value === "") {
    return null;
  }
  const email = normalizeEmail(value);
  if (!email || email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
    throw new OrgError(400, "that email does not look right");
  }
  return email;
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

function newId(prefix: string): string {
  return `${prefix}_${randomBytes(8).toString("hex")}`;
}

export type OrgStore = ReturnType<typeof createOrgStore>;

export function createOrgStore(database: Database.Database, options: { now?: () => number } = {}) {
  const now = options.now ?? (() => Date.now());

  database.exec(`
    create table if not exists rudder_orgs (
      id text primary key,
      name text not null,
      created_by text not null,
      created_at integer not null,
      updated_at integer not null
    );
    create table if not exists rudder_org_members (
      org_id text not null,
      account_id text not null,
      role text not null,
      email text,
      joined_at integer not null,
      primary key (org_id, account_id)
    );
    create index if not exists rudder_org_members_account on rudder_org_members (account_id);
    create table if not exists rudder_org_invites (
      id text primary key,
      org_id text not null,
      token_hash text not null unique,
      email text,
      role text not null,
      created_by text not null,
      created_at integer not null,
      expires_at integer not null,
      accepted_at integer,
      accepted_by text,
      revoked_at integer
    );
    create index if not exists rudder_org_invites_org on rudder_org_invites (org_id);
  `);

  const hasUserTable = (): boolean =>
    Boolean(database.prepare("select 1 from sqlite_master where type = 'table' and name = 'user'").get());
  let userLookup: Database.Statement | null | undefined;
  // The better-auth `user` table may not exist (tests, or before better-auth
  // migrates); look it up lazily and fall back to the email stored at join.
  function authUser(accountId: string): { email: string | null; name: string | null } | null {
    if (userLookup === undefined || userLookup === null) {
      if (!hasUserTable()) {
        return null;
      }
      try {
        userLookup = database.prepare('select email, name from "user" where id = ?');
      } catch {
        userLookup = null;
        return null;
      }
    }
    const id = accountId.startsWith("better-auth:") ? accountId.slice("better-auth:".length) : accountId;
    const row = userLookup.get(id) as { email?: unknown; name?: unknown } | undefined;
    if (!row) {
      return null;
    }
    return {
      email: typeof row.email === "string" && row.email ? row.email : null,
      name: typeof row.name === "string" && row.name ? row.name : null,
    };
  }

  const q = {
    org: database.prepare("select id, name, created_at from rudder_orgs where id = ?"),
    insertOrg: database.prepare(
      "insert into rudder_orgs (id, name, created_by, created_at, updated_at) values (?, ?, ?, ?, ?)",
    ),
    renameOrg: database.prepare("update rudder_orgs set name = ?, updated_at = ? where id = ?"),
    deleteOrg: database.prepare("delete from rudder_orgs where id = ?"),
    deleteMembers: database.prepare("delete from rudder_org_members where org_id = ?"),
    deleteInvites: database.prepare("delete from rudder_org_invites where org_id = ?"),
    member: database.prepare(
      "select org_id, account_id, role, email, joined_at from rudder_org_members where org_id = ? and account_id = ?",
    ),
    members: database.prepare(
      "select org_id, account_id, role, email, joined_at from rudder_org_members where org_id = ? order by joined_at, account_id",
    ),
    insertMember: database.prepare(
      "insert into rudder_org_members (org_id, account_id, role, email, joined_at) values (?, ?, ?, ?, ?)",
    ),
    setRole: database.prepare("update rudder_org_members set role = ? where org_id = ? and account_id = ?"),
    removeMember: database.prepare("delete from rudder_org_members where org_id = ? and account_id = ?"),
    adminCount: database.prepare("select count(*) as n from rudder_org_members where org_id = ? and role = 'admin'"),
    memberCount: database.prepare("select count(*) as n from rudder_org_members where org_id = ?"),
    orgsForAccount: database.prepare(`
      select o.id, o.name, o.created_at, m.role,
        (select count(*) from rudder_org_members c where c.org_id = o.id) as member_count
      from rudder_org_members m join rudder_orgs o on o.id = m.org_id
      where m.account_id = ?
      order by o.created_at, o.id
    `),
    orgIds: database.prepare("select org_id from rudder_org_members where account_id = ? order by joined_at"),
    insertInvite: database.prepare(`
      insert into rudder_org_invites (id, org_id, token_hash, email, role, created_by, created_at, expires_at)
      values (?, ?, ?, ?, ?, ?, ?, ?)
    `),
    invite: database.prepare("select * from rudder_org_invites where id = ? and org_id = ?"),
    inviteByHash: database.prepare("select * from rudder_org_invites where token_hash = ?"),
    openInvites: database.prepare(`
      select * from rudder_org_invites
      where org_id = ? and accepted_at is null and revoked_at is null and expires_at > ?
      order by created_at
    `),
    revokeInvite: database.prepare("update rudder_org_invites set revoked_at = ? where id = ?"),
    acceptInvite: database.prepare("update rudder_org_invites set accepted_at = ?, accepted_by = ? where id = ?"),
  };

  function orgRow(orgId: string): OrgRow {
    const row = q.org.get(orgId) as OrgRow | undefined;
    if (!row) {
      throw new OrgError(404, "org not found");
    }
    return row;
  }

  function roleOf(accountId: string, orgId: string): OrgRole | null {
    const row = q.member.get(orgId, accountId) as MemberRow | undefined;
    return row ? row.role : null;
  }

  /** The caller's role, or 404 when they are not in the org (an org you are
   * not in does not exist as far as you can tell). */
  function requireMember(actor: OrgActor, orgId: string): OrgRole {
    orgRow(orgId);
    const role = roleOf(actor.accountId, orgId);
    if (!role) {
      throw new OrgError(404, "org not found");
    }
    return role;
  }

  function requireAdmin(actor: OrgActor, orgId: string): void {
    if (requireMember(actor, orgId) !== "admin") {
      throw new OrgError(403, "only an admin can do that");
    }
  }

  function adminCount(orgId: string): number {
    return (q.adminCount.get(orgId) as { n: number }).n;
  }

  function inviteView(row: InviteRow): OrgInviteView {
    return { id: row.id, email: row.email, role: row.role, createdAt: iso(row.created_at), expiresAt: iso(row.expires_at) };
  }

  function actorEmail(actor: OrgActor): string | null {
    return normalizeEmail(actor.email) ?? normalizeEmail(authUser(actor.accountId)?.email);
  }

  function liveInvite(token: string): InviteRow {
    const row = typeof token === "string" && token ? (q.inviteByHash.get(hashInviteToken(token)) as InviteRow | undefined) : undefined;
    if (!row || row.revoked_at !== null || !q.org.get(row.org_id)) {
      throw new OrgError(404, "invite not found");
    }
    return row;
  }

  function inviteUsable(row: InviteRow): void {
    if (row.accepted_at !== null) {
      throw new OrgError(410, "this invite has already been used");
    }
    if (row.expires_at <= now()) {
      throw new OrgError(410, "this invite has expired");
    }
  }

  return {
    roleOf,

    orgIdsForAccount(accountId: string): string[] {
      return (q.orgIds.all(accountId) as Array<{ org_id: string }>).map((row) => row.org_id);
    },

    listOrgsForAccount(accountId: string): OrgSummary[] {
      const rows = q.orgsForAccount.all(accountId) as Array<OrgRow & { role: OrgRole; member_count: number }>;
      return rows.map((row) => ({
        id: row.id,
        name: row.name,
        role: row.role,
        memberCount: row.member_count,
        createdAt: iso(row.created_at),
      }));
    },

    createOrg(actor: OrgActor, rawName: unknown): OrgSummary {
      const name = validOrgName(rawName);
      const id = newId("org");
      const at = now();
      database.transaction(() => {
        q.insertOrg.run(id, name, actor.accountId, at, at);
        q.insertMember.run(id, actor.accountId, "admin", actorEmail(actor), at);
      })();
      return { id, name, role: "admin", memberCount: 1, createdAt: iso(at) };
    },

    getOrgView(actor: OrgActor, orgId: string): OrgView {
      const role = requireMember(actor, orgId);
      const org = orgRow(orgId);
      const members = (q.members.all(orgId) as MemberRow[]).map((row) => {
        const user = authUser(row.account_id);
        return {
          accountId: row.account_id,
          email: user?.email ?? row.email ?? null,
          name: user?.name ?? null,
          role: row.role,
          joinedAt: iso(row.joined_at),
          you: row.account_id === actor.accountId,
        };
      });
      const invites = role === "admin" ? (q.openInvites.all(orgId, now()) as InviteRow[]).map(inviteView) : [];
      return { org: { id: org.id, name: org.name, createdAt: iso(org.created_at) }, role, members, invites };
    },

    renameOrg(actor: OrgActor, orgId: string, rawName: unknown): { id: string; name: string; createdAt: string } {
      requireAdmin(actor, orgId);
      const name = validOrgName(rawName);
      q.renameOrg.run(name, now(), orgId);
      const org = orgRow(orgId);
      return { id: org.id, name: org.name, createdAt: iso(org.created_at) };
    },

    deleteOrg(actor: OrgActor, orgId: string): void {
      requireAdmin(actor, orgId);
      database.transaction(() => {
        q.deleteInvites.run(orgId);
        q.deleteMembers.run(orgId);
        q.deleteOrg.run(orgId);
      })();
    },

    createInvite(actor: OrgActor, orgId: string, input: { email?: unknown; role?: unknown }): CreatedInvite {
      requireAdmin(actor, orgId);
      const role = input.role === undefined ? "member" : validRole(input.role);
      const email = validInviteEmail(input.email);
      const token = randomBytes(32).toString("base64url");
      const id = newId("inv");
      const at = now();
      q.insertInvite.run(id, orgId, hashInviteToken(token), email, role, actor.accountId, at, at + INVITE_TTL_MS);
      return { id, email, role, createdAt: iso(at), expiresAt: iso(at + INVITE_TTL_MS), token };
    },

    revokeInvite(actor: OrgActor, orgId: string, inviteId: string): void {
      requireAdmin(actor, orgId);
      const row = q.invite.get(inviteId, orgId) as InviteRow | undefined;
      if (!row) {
        throw new OrgError(404, "invite not found");
      }
      if (row.revoked_at === null) {
        q.revokeInvite.run(now(), inviteId);
      }
    },

    inviteInfo(actor: OrgActor, token: string): InviteInfo {
      const row = liveInvite(token);
      inviteUsable(row);
      const org = orgRow(row.org_id);
      return {
        org: { id: org.id, name: org.name },
        role: row.role,
        email: row.email,
        expiresAt: iso(row.expires_at),
        alreadyMember: roleOf(actor.accountId, row.org_id) !== null,
      };
    },

    /** Join through an invite. Accepting as someone already in the org is a
     * no-op that leaves the invite unused (the link still works for whoever it
     * was meant for). */
    acceptInvite(actor: OrgActor, token: string): { id: string; name: string; role: OrgRole } {
      const row = liveInvite(token);
      const org = orgRow(row.org_id);
      const existing = roleOf(actor.accountId, row.org_id);
      if (existing) {
        return { id: org.id, name: org.name, role: existing };
      }
      inviteUsable(row);
      const email = actorEmail(actor);
      if (row.email && row.email !== email) {
        throw new OrgError(403, `this invite is for ${row.email}`);
      }
      const at = now();
      database.transaction(() => {
        const claimed = q.acceptInvite.run(at, actor.accountId, row.id);
        if (claimed.changes !== 1) {
          throw new OrgError(410, "this invite has already been used");
        }
        q.insertMember.run(row.org_id, actor.accountId, row.role, email, at);
      })();
      return { id: org.id, name: org.name, role: row.role };
    },

    setRole(actor: OrgActor, orgId: string, accountId: string, rawRole: unknown): OrgRole {
      requireAdmin(actor, orgId);
      const role = validRole(rawRole);
      const current = roleOf(accountId, orgId);
      if (!current) {
        throw new OrgError(404, "member not found");
      }
      if (current === role) {
        return role;
      }
      database.transaction(() => {
        if (current === "admin" && adminCount(orgId) <= 1) {
          throw new OrgError(409, LAST_ADMIN_MESSAGE);
        }
        q.setRole.run(role, orgId, accountId);
      })();
      return role;
    },

    /** An admin removes anyone; anyone removes themself (leaving). */
    removeMember(actor: OrgActor, orgId: string, accountId: string): void {
      const role = requireMember(actor, orgId);
      if (accountId !== actor.accountId && role !== "admin") {
        throw new OrgError(403, "only an admin can do that");
      }
      const target = roleOf(accountId, orgId);
      if (!target) {
        throw new OrgError(404, "member not found");
      }
      database.transaction(() => {
        if (target === "admin" && adminCount(orgId) <= 1) {
          throw new OrgError(409, LAST_ADMIN_MESSAGE);
        }
        q.removeMember.run(orgId, accountId);
      })();
    },
  };
}

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type OrgApiDeps = {
  store: OrgStore;
  sendJson: (res: ServerResponse, status: number, body: any) => void;
  readJsonBody: (req: IncomingMessage) => Promise<unknown>;
  baseURL: string;
  capture?: (accountId: string, name: string, props?: Record<string, unknown>) => void;
};

function bodyObject(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

function segment(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    throw new OrgError(400, "bad path");
  }
}

/**
 * Route every /api/app/orgs* and /api/app/invites* request. Returns false for
 * any other path so the caller can keep routing. OrgErrors become
 * `{ error }` responses with their status; anything else is rethrown.
 */
export async function handleOrgApi(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  account: OrgActor,
  deps: OrgApiDeps,
): Promise<boolean> {
  const parts = url.pathname.replace(/\/+$/, "").split("/").slice(1); // ["api","app",...]
  if (parts[0] !== "api" || parts[1] !== "app" || (parts[2] !== "orgs" && parts[2] !== "invites")) {
    return false;
  }
  const method = (req.method || "GET").toUpperCase();
  const rest = parts.slice(3).map(segment);
  const { store, sendJson } = deps;
  const capture = (name: string, props: Record<string, unknown> = {}) => {
    try {
      deps.capture?.(account.accountId, name, props);
    } catch {
      // analytics never breaks a request
    }
  };
  const readBody = async () => {
    try {
      return bodyObject(await deps.readJsonBody(req));
    } catch (error) {
      if ((error as { status?: number }).status) {
        throw error;
      }
      throw new OrgError(400, "invalid JSON body");
    }
  };
  const notAllowed = () => sendJson(res, 405, { error: "method not allowed" });

  try {
    if (parts[2] === "invites") {
      const [token, action] = rest;
      if (!token || rest.length > 2 || (action !== undefined && action !== "accept")) {
        sendJson(res, 404, { error: "not found" });
        return true;
      }
      if (!action) {
        if (method !== "GET") return notAllowed(), true;
        sendJson(res, 200, store.inviteInfo(account, token) as unknown as Json);
        return true;
      }
      if (method !== "POST") return notAllowed(), true;
      const org = store.acceptInvite(account, token);
      capture("org_invite_accepted", { role: org.role });
      sendJson(res, 200, { org });
      return true;
    }

    // /api/app/orgs
    if (rest.length === 0) {
      if (method === "GET") {
        sendJson(res, 200, { orgs: store.listOrgsForAccount(account.accountId) });
        return true;
      }
      if (method === "POST") {
        const body = await readBody();
        const org = store.createOrg(account, body.name);
        capture("org_created");
        sendJson(res, 200, { org });
        return true;
      }
      return notAllowed(), true;
    }

    const [orgId, sub, subId] = rest;
    if (rest.length === 1) {
      if (method === "GET") {
        sendJson(res, 200, store.getOrgView(account, orgId) as unknown as Json);
        return true;
      }
      if (method === "PATCH") {
        const body = await readBody();
        sendJson(res, 200, { org: store.renameOrg(account, orgId, body.name) });
        return true;
      }
      if (method === "DELETE") {
        store.deleteOrg(account, orgId);
        sendJson(res, 200, { ok: true });
        return true;
      }
      return notAllowed(), true;
    }

    if (sub === "invites" && rest.length === 2) {
      if (method !== "POST") return notAllowed(), true;
      const body = await readBody();
      const { token, ...invite } = store.createInvite(account, orgId, { email: body.email, role: body.role });
      const url = `${deps.baseURL.replace(/\/+$/, "")}/app/join/${token}`;
      capture("org_invite_created", { role: invite.role, email_locked: Boolean(invite.email) });
      sendJson(res, 200, { invite: { ...invite, url } });
      return true;
    }
    if (sub === "invites" && rest.length === 3) {
      if (method !== "DELETE") return notAllowed(), true;
      store.revokeInvite(account, orgId, subId);
      sendJson(res, 200, { ok: true });
      return true;
    }
    if (sub === "members" && rest.length === 3) {
      if (method === "POST") {
        const body = await readBody();
        const role = store.setRole(account, orgId, subId, body.role);
        capture("org_member_role_changed", { role });
        sendJson(res, 200, { ok: true, role });
        return true;
      }
      if (method === "DELETE") {
        store.removeMember(account, orgId, subId);
        capture("org_member_removed", { self: subId === account.accountId });
        sendJson(res, 200, { ok: true });
        return true;
      }
      return notAllowed(), true;
    }
    sendJson(res, 404, { error: "not found" });
    return true;
  } catch (error) {
    if (error instanceof OrgError) {
      sendJson(res, error.status, { error: error.message });
      return true;
    }
    throw error;
  }
}
