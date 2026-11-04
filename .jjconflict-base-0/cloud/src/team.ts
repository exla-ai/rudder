// The team view and private agents.
//
// Members of an org see each other and what each other's agents are doing:
// per person, their projects and agents (title, status, backend and model,
// last update). Everything comes from the home state the control plane
// already holds in memory for each machine; nothing here talks to a
// teammate's machine except reading one transcript, and only while that
// machine is already connected (a teammate looking never wakes it).
//
// Each agent has a visibility, "team" (the default) or "private". A private
// agent shows to teammates as a muted row with its status only (no title,
// task, model or transcript), and its transcript is 404 to anyone but the
// owner. A project can also make NEW agents private: turning that on pins
// every agent already known as "team", and each agent first seen afterwards
// is pinned "private" (pinNew), so the lock on each agent always says what
// it is.
//
// Visibility lives in rudder_agent_visibility, keyed by the home-repo
// workspace id (stable across machine restarts and image updates) and the
// run id; run_id "*" is the project's default for new agents. Not in run.json:
// the dashboard owns that file and the control plane must answer for a
// machine that is asleep.
//
// Writes stay owner-only everywhere: this module's only write is the owner's
// own toggle, and every agent route in server.ts keeps resolving the
// workspace through the caller's own account.

import { agentNeeds, type StatusAgent } from "./agent-status.js";
import type { IncomingMessage, ServerResponse } from "node:http";
import type Database from "better-sqlite3";

export type Visibility = "team" | "private";

export const PROJECT_DEFAULT_RUN = "*";

export class TeamError extends Error {
  status: number;
  extra: Record<string, unknown>;
  constructor(status: number, message: string, extra: Record<string, unknown> = {}) {
    super(message);
    this.name = "TeamError";
    this.status = status;
    this.extra = extra;
  }
}

export function validVisibility(value: unknown): Visibility {
  if (value === "team" || value === "private") {
    return value;
  }
  throw new TeamError(400, "visibility must be team or private");
}

const RUN_ID_RE = /^[A-Za-z0-9._-]{1,120}$/;

export type VisibilityStore = ReturnType<typeof createVisibilityStore>;

export function createVisibilityStore(database: Database.Database, options: { now?: () => number } = {}) {
  const now = options.now ?? (() => Date.now());
  database.exec(`
    create table if not exists rudder_agent_visibility (
      workspace_id text not null,
      run_id text not null,
      visibility text not null,
      updated_at integer not null,
      primary key (workspace_id, run_id)
    );
  `);
  const q = {
    all: database.prepare("select run_id, visibility from rudder_agent_visibility where workspace_id = ?"),
    upsert: database.prepare(`
      insert into rudder_agent_visibility (workspace_id, run_id, visibility, updated_at) values (?, ?, ?, ?)
      on conflict (workspace_id, run_id) do update set visibility = excluded.visibility, updated_at = excluded.updated_at
    `),
    insertIgnore: database.prepare(
      "insert or ignore into rudder_agent_visibility (workspace_id, run_id, visibility, updated_at) values (?, ?, ?, ?)",
    ),
    remove: database.prepare("delete from rudder_agent_visibility where workspace_id = ? and run_id = ?"),
  };

  /** One project's settings: the default for new agents and each pinned agent. */
  function forProject(workspaceId: string): { newAgentsPrivate: boolean; runs: Map<string, Visibility> } {
    const runs = new Map<string, Visibility>();
    let newAgentsPrivate = false;
    for (const row of q.all.all(workspaceId) as Array<{ run_id: string; visibility: string }>) {
      const visibility: Visibility = row.visibility === "private" ? "private" : "team";
      if (row.run_id === PROJECT_DEFAULT_RUN) {
        newAgentsPrivate = visibility === "private";
      } else {
        runs.set(row.run_id, visibility);
      }
    }
    return { newAgentsPrivate, runs };
  }

  function effective(settings: ReturnType<typeof forProject>, runId: string): Visibility {
    return settings.runs.get(runId) ?? (settings.newAgentsPrivate ? "private" : "team");
  }

  return {
    forProject,
    effective,

    visibilityOf(workspaceId: string, runId: string): Visibility {
      return effective(forProject(workspaceId), runId);
    },

    setAgent(workspaceId: string, runId: string, visibility: Visibility): void {
      if (!RUN_ID_RE.test(runId)) {
        throw new TeamError(400, "run is required");
      }
      q.upsert.run(workspaceId, runId, visibility, now());
    },

    /** "New agents here are private". Turning it on keeps every agent already
     * known where it is (pinned "team" unless it has its own setting). */
    setNewAgentsPrivate(workspaceId: string, on: boolean, knownRunIds: string[]): void {
      const at = now();
      database.transaction(() => {
        if (on) {
          for (const runId of knownRunIds) {
            if (RUN_ID_RE.test(runId)) {
              q.insertIgnore.run(workspaceId, runId, "team", at);
            }
          }
          q.upsert.run(workspaceId, PROJECT_DEFAULT_RUN, "private", at);
        } else {
          q.remove.run(workspaceId, PROJECT_DEFAULT_RUN);
        }
      })();
    },

    /** Called on every home state: under a private default, an agent seen for
     * the first time is pinned private. Returns how many were pinned. */
    pinNew(workspaceId: string, runIds: string[]): number {
      const settings = forProject(workspaceId);
      if (!settings.newAgentsPrivate) {
        return 0;
      }
      let pinned = 0;
      const at = now();
      for (const runId of runIds) {
        if (!settings.runs.has(runId) && RUN_ID_RE.test(runId)) {
          pinned += q.insertIgnore.run(workspaceId, runId, "private", at).changes;
        }
      }
      return pinned;
    },
  };
}

// ---------------------------------------------------------------------------
// What a teammate sees of an agent.
// ---------------------------------------------------------------------------

export type TeamSourceAgent = {
  id?: unknown;
  title?: unknown;
  status?: unknown;
  phase?: unknown;
  backend?: unknown;
  model?: unknown;
  createdAt?: unknown;
  updatedAt?: unknown;
  pending?: unknown;
  asks?: unknown;
  visibility?: unknown;
  attention?: unknown;
  conflict?: unknown;
  notInTrunk?: unknown;
  pr?: unknown;
  needsYou?: unknown;
  reason?: unknown;
};

export type TeamSourceProject = {
  workspaceId: string;
  name?: string | null;
  repo?: string | null;
  agents?: TeamSourceAgent[] | unknown;
};

export type TeamAgent =
  | { id: string; private: true; status: string; asking: string | null; needsYou: boolean; reason: string | null; updatedAt: number | null }
  | {
      id: string;
      private: false;
      title: string;
      status: string;
      phase: string | null;
      backend: string | null;
      model: string | null;
      asking: string | null;
      needsYou: boolean;
      reason: string | null;
      pending: boolean;
      updatedAt: number | null;
    };

const str = (value: unknown): string | null => (typeof value === "string" && value ? value : null);
const num = (value: unknown): number | null => (typeof value === "number" && Number.isFinite(value) ? value : null);

/** The kind of the first prompt the agent is blocked on, never its content. */
function askingKind(asks: unknown): string | null {
  if (!Array.isArray(asks) || asks.length === 0) {
    return null;
  }
  const kind = (asks[0] as { kind?: unknown } | null)?.kind;
  return kind === "permission" || kind === "question" || kind === "plan" ? kind : "permission";
}

/** A whitelist, not a blacklist: a field the supervisor adds later stays home. */
export function teamAgentView(agent: TeamSourceAgent, visibility: Visibility): TeamAgent | null {
  const id = str(agent.id);
  if (!id) {
    return null;
  }
  const status = str(agent.status) ?? "unknown";
  const asking = askingKind(agent.asks);
  const updatedAt = num(agent.updatedAt);
  // The control plane's one rule (agent-status.ts); a category, never content.
  const needs = typeof agent.needsYou === "boolean"
    ? { needsYou: agent.needsYou, reason: str(agent.reason) }
    : agentNeeds(agent as StatusAgent);
  const needsYou = needs.needsYou;
  const reason = needs.reason ?? null;
  if (visibility === "private") {
    return { id, private: true, status, asking, needsYou, reason, updatedAt };
  }
  return {
    id,
    private: false,
    title: (str(agent.title) ?? "Untitled agent").slice(0, 200),
    status,
    phase: str(agent.phase),
    backend: str(agent.backend),
    model: str(agent.model),
    asking,
    needsYou,
    reason,
    pending: agent.pending === true,
    updatedAt,
  };
}

const LIVE = new Set(["working", "starting"]);
function rank(agent: TeamAgent): number {
  // Blocked on a person mid-turn first, then working, then finished work
  // that wants a look (review, failed, conflict), then the rest.
  if (agent.needsYou && agent.reason !== "review" && agent.reason !== "failed") return 0;
  if (LIVE.has(agent.status)) return 1;
  if (agent.needsYou) return 2;
  return 3;
}

export type TeamMember = {
  accountId: string;
  name: string | null;
  email: string | null;
  role: string;
  machine: "awake" | "asleep" | "none";
  projects: Array<{ workspaceId: string; name: string; repo: string | null; agents: TeamAgent[] }>;
};

export type TeamDeps = {
  /** The orgs the viewer is in. */
  orgsFor(accountId: string): Array<{ id: string; name: string }>;
  /** An org's members as the viewer may see them (throws a 404 for outsiders). */
  membersOf(viewer: { accountId: string; email?: string | null }, orgId: string): Array<{
    accountId: string;
    email: string | null;
    name: string | null;
    role: string;
    you: boolean;
  }>;
  /** A member's projects from the in-memory home state (never wakes anything);
   * each agent carries its `visibility`. */
  projectsOf(accountId: string): TeamSourceProject[];
  /** "awake" while the supervisor holds its control socket. */
  machineOf(accountId: string): "awake" | "asleep" | "none";
};

/** Teammates in one org (or every org the viewer is in), not the viewer. */
export function buildTeamView(
  viewer: { accountId: string; email?: string | null },
  orgId: string | null,
  deps: TeamDeps,
): { orgs: Array<{ id: string; name: string }>; org: { id: string; name: string } | null; members: TeamMember[] } {
  const orgs = deps.orgsFor(viewer.accountId);
  let scope = orgs;
  if (orgId) {
    const org = orgs.find((candidate) => candidate.id === orgId);
    if (!org) {
      throw new TeamError(404, "org not found");
    }
    scope = [org];
  }
  const seen = new Map<string, TeamMember>();
  for (const org of scope) {
    for (const member of deps.membersOf(viewer, org.id)) {
      if (member.you || member.accountId === viewer.accountId || seen.has(member.accountId)) {
        continue;
      }
      const projects = deps.projectsOf(member.accountId)
        .map((project) => {
          const agents = (Array.isArray(project.agents) ? (project.agents as TeamSourceAgent[]) : [])
            .map((agent) => teamAgentView(agent, agent.visibility === "private" ? "private" : "team"))
            .filter((agent): agent is TeamAgent => Boolean(agent))
            .sort((a, b) => rank(a) - rank(b) || (b.updatedAt ?? 0) - (a.updatedAt ?? 0));
          return { workspaceId: project.workspaceId, name: project.name || project.repo || "repo", repo: project.repo ?? null, agents };
        })
        .filter((project) => project.agents.length > 0);
      seen.set(member.accountId, {
        accountId: member.accountId,
        name: member.name,
        email: member.email,
        role: member.role,
        machine: deps.machineOf(member.accountId),
        projects,
      });
    }
  }
  const members = Array.from(seen.values()).sort((a, b) => {
    const live = (m: TeamMember) => m.projects.reduce((n, p) => n + p.agents.filter((x) => LIVE.has(x.status) || x.asking).length, 0);
    return live(b) - live(a) || (a.name || a.email || a.accountId).localeCompare(b.name || b.email || b.accountId);
  });
  return { orgs, org: orgId ? scope[0] : null, members };
}

/** Do the two accounts share an org? (An account always "shares" with itself.) */
export function sharesOrg(deps: Pick<TeamDeps, "orgsFor">, viewerId: string, ownerId: string): boolean {
  if (viewerId === ownerId) {
    return true;
  }
  const mine = new Set(deps.orgsFor(viewerId).map((org) => org.id));
  return deps.orgsFor(ownerId).some((org) => mine.has(org.id));
}

// ---------------------------------------------------------------------------
// HTTP: GET /api/app/team, GET /api/app/team/transcript, POST /api/app/visibility
// ---------------------------------------------------------------------------

export type TeamApiDeps = TeamDeps & {
  visibility: VisibilityStore;
  sendJson: (res: ServerResponse, status: number, body: any) => void;
  readJsonBody: (req: IncomingMessage) => Promise<unknown>;
  /** Resolves the caller's OWN project (throws 404 otherwise). */
  requireOwnProject(accountId: string, workspaceId: string): { workspaceId: string };
  /** Run ids the owner's machine last reported for a project. */
  knownRunIds(workspaceId: string): string[];
  /** The owner's machine, only when it is connected right now (null otherwise). */
  connectedHomeOf(accountId: string): string | null;
  rpc(homeId: string, method: string, params: Record<string, unknown>): Promise<unknown>;
  persist?(): void;
  displayName(accountId: string): string;
};

function body(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
}

export async function handleTeamApi(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  account: { accountId: string; email?: string | null },
  deps: TeamApiDeps,
): Promise<boolean> {
  const path = url.pathname.replace(/\/+$/, "");
  const method = (req.method || "GET").toUpperCase();
  if (path !== "/api/app/team" && path !== "/api/app/team/transcript" && path !== "/api/app/visibility") {
    return false;
  }
  try {
    if (path === "/api/app/team") {
      if (method !== "GET") {
        deps.sendJson(res, 405, { error: "method not allowed" });
        return true;
      }
      deps.sendJson(res, 200, buildTeamView(account, url.searchParams.get("org") || null, deps));
      return true;
    }

    if (path === "/api/app/team/transcript") {
      if (method !== "GET") {
        deps.sendJson(res, 405, { error: "method not allowed" });
        return true;
      }
      const ownerId = url.searchParams.get("member") ?? "";
      const workspaceId = url.searchParams.get("workspace") ?? "";
      const runId = url.searchParams.get("run") ?? "";
      // One answer for "not in your org", "no such project", "no such agent"
      // and "private": nothing tells an outsider which of them it was.
      const notFound = new TeamError(404, "not found");
      if (!ownerId || !workspaceId || !RUN_ID_RE.test(runId) || !sharesOrg(deps, account.accountId, ownerId)) {
        throw notFound;
      }
      const project = deps.projectsOf(ownerId).find((candidate) => candidate.workspaceId === workspaceId);
      const agent = (Array.isArray(project?.agents) ? (project!.agents as TeamSourceAgent[]) : []).find((candidate) => candidate.id === runId);
      if (!project || !agent) {
        throw notFound;
      }
      if (ownerId !== account.accountId && deps.visibility.visibilityOf(workspaceId, runId) === "private") {
        throw notFound;
      }
      const homeId = deps.connectedHomeOf(ownerId);
      if (!homeId) {
        throw new TeamError(503, `${deps.displayName(ownerId)}'s cloud machine is asleep. Their agents show here again when it wakes.`, { asleep: true });
      }
      const limit = Math.min(800, Math.max(20, Number(url.searchParams.get("limit") || 300) || 300));
      const result = await deps.rpc(homeId, "transcript", { workspaceId, runId, limit });
      deps.sendJson(res, 200, { ...body(result), readOnly: ownerId !== account.accountId, owner: { accountId: ownerId, name: deps.displayName(ownerId) } });
      return true;
    }

    // POST /api/app/visibility {workspace, run, visibility} or {workspace, newAgentsPrivate}
    if (method !== "POST") {
      deps.sendJson(res, 405, { error: "method not allowed" });
      return true;
    }
    const input = body(await deps.readJsonBody(req));
    const { workspaceId } = deps.requireOwnProject(account.accountId, typeof input.workspace === "string" ? input.workspace : "");
    if (typeof input.newAgentsPrivate === "boolean") {
      deps.visibility.setNewAgentsPrivate(workspaceId, input.newAgentsPrivate, deps.knownRunIds(workspaceId));
      deps.persist?.();
      deps.sendJson(res, 200, { workspaceId, newAgentsPrivate: input.newAgentsPrivate });
      return true;
    }
    const runId = typeof input.run === "string" ? input.run : "";
    const visibility = validVisibility(input.visibility);
    deps.visibility.setAgent(workspaceId, runId, visibility);
    deps.persist?.();
    deps.sendJson(res, 200, { workspaceId, run: runId, visibility });
    return true;
  } catch (error) {
    const status = (error as { status?: unknown }).status;
    if (error instanceof TeamError) {
      deps.sendJson(res, error.status, { error: error.message, ...error.extra });
      return true;
    }
    if (typeof status === "number" && status >= 400 && status < 500) {
      deps.sendJson(res, status, { error: error instanceof Error ? error.message : String(error) });
      return true;
    }
    throw error;
  }
}
