import type { IncomingMessage } from "node:http";
import { CloudProjectFiles, CloudProjectFileError, type CloudFileScope } from "./relay.js";

/** The Files page's agent setup: what every agent on the owner's home reads
 * (instructions, skills, Claude's subagents and commands) and each project's
 * instruction files. Only the home's own account reaches it; teammates and
 * organization members never do, even for projects shared with them.
 * Mirrors src/project-files/agent-setup.ts on the home. */
export type AgentSetupDependencies = {
  /** The account's own connected home, or null. */
  ownHome(accountId: string): string | null;
  workspace(accountId: string, workspaceId: string): { homeId: string };
  rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};

const SETUP_PROJECT = "agent-setup";
const AGENTS = ["claude", "codex"] as const;
type Agent = typeof AGENTS[number];
const SKILL_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;
const REVISION = /^[a-f0-9]{64}$/;
const ROOT = /^(?:instructions:(?:claude|codex)|skill:(?:claude|codex|shared):[A-Za-z0-9][A-Za-z0-9._-]{0,127}|folder:(?:claude-rules|claude-agents|claude-commands))$/;

const text = (value: unknown, max: number): string => {
  if (typeof value !== "string" || value.length > max || /[\x00-\x08\x0b-\x1f\x7f]/.test(value)) throw new CloudProjectFileError(502, "Invalid agent setup reply");
  return value;
};
const nullable = <T>(value: unknown, read: (v: unknown) => T): T | null => (value === null ? null : read(value));
const root = (value: unknown) => nullable(value, (v) => { const id = text(v, 300); if (!ROOT.test(id)) throw new CloudProjectFileError(502, "Invalid agent setup reply"); return id; });
const agents = (value: unknown): Agent[] => {
  if (!Array.isArray(value) || value.length > 2 || !value.every((a) => AGENTS.includes(a))) throw new CloudProjectFileError(502, "Invalid agent setup reply");
  return [...new Set(value as Agent[])];
};
const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new CloudProjectFileError(502, "Invalid agent setup reply");
  return value as Record<string, unknown>;
};

/** Only known fields with bounded values reach the browser. */
export function overviewFrom(value: unknown) {
  const reply = record(value);
  const agentInfo = record(reply.agents);
  const instructions = record(reply.instructions);
  const instruction = (agent: Agent) => {
    const item = record(instructions[agent]);
    if (item.agent !== agent || !["file", "link", "missing"].includes(String(item.state))) throw new CloudProjectFileError(502, "Invalid agent setup reply");
    return { agent, path: text(item.path, 4096), state: item.state as "file" | "link" | "missing", linksTo: nullable(item.linksTo, (v) => text(v, 4096)), root: root(item.root) };
  };
  if (!Array.isArray(reply.skills) || reply.skills.length > 500 || !Array.isArray(reply.folders) || reply.folders.length > 3) throw new CloudProjectFileError(502, "Invalid agent setup reply");
  return {
    agents: Object.fromEntries(AGENTS.map((agent) => {
      const item = record(agentInfo[agent]);
      return [agent, { dir: text(item.dir, 4096), present: item.present === true }];
    })) as Record<Agent, { dir: string; present: boolean }>,
    instructions: { claude: instruction("claude"), codex: instruction("codex"), shared: instructions.shared === true },
    skills: reply.skills.map((entry: unknown) => {
      const item = record(entry);
      return { name: text(item.name, 256), description: text(item.description, 1024), agents: agents(item.agents), root: root(item.root),
        location: text(item.location, 4096), note: text(item.note, 512) };
    }),
    folders: reply.folders.map((entry: unknown) => {
      const item = record(entry);
      return { id: text(item.id, 64), label: text(item.label, 64), root: root(item.root) };
    }),
  };
}

export function guideFrom(value: unknown) {
  const reply = record(value);
  if (!Array.isArray(reply.files) || reply.files.length > 16) throw new CloudProjectFileError(502, "Invalid project guide reply");
  const relative = (v: unknown) => { const p = text(v, 1024); if (p.startsWith("/") || p.split("/").includes("..")) throw new CloudProjectFileError(502, "Invalid project guide reply"); return p; };
  return {
    files: reply.files.map((entry: unknown) => {
      const item = record(entry);
      if (!["file", "link"].includes(String(item.kind)) || !Array.isArray(item.imports) || item.imports.length > 32) throw new CloudProjectFileError(502, "Invalid project guide reply");
      return { path: relative(item.path), kind: item.kind as "file" | "link", linksTo: nullable(item.linksTo, relative), imports: item.imports.map(relative), readBy: agents(item.readBy) };
    }),
    shared: reply.shared === true,
  };
}

function actionFrom(body: unknown): Record<string, unknown> {
  const value = body as Record<string, unknown> | null;
  const keys = value && typeof value === "object" && !Array.isArray(value) ? Object.keys(value) : null;
  const bad = () => new CloudProjectFileError(400, "Invalid agent setup action");
  if (!keys || value!.version !== 1) throw bad();
  const only = (allowed: string[]) => { if (keys.some((k) => !allowed.includes(k))) throw bad(); };
  switch (value!.operation) {
    case "link-instructions":
      only(["version", "operation", "keep", "replacedRevision"]);
      if (!AGENTS.includes(value!.keep as Agent) || (value!.replacedRevision !== undefined && !(typeof value!.replacedRevision === "string" && REVISION.test(value!.replacedRevision)))) throw bad();
      return { version: 1, operation: "link-instructions", keep: value!.keep, ...(value!.replacedRevision ? { replacedRevision: value!.replacedRevision } : {}) };
    case "share-skill":
      only(["version", "operation", "name", "from"]);
      if (typeof value!.name !== "string" || !SKILL_NAME.test(value!.name) || !AGENTS.includes(value!.from as Agent)) throw bad();
      return { version: 1, operation: "share-skill", name: value!.name, from: value!.from };
    case "new-skill":
      only(["version", "operation", "name", "description"]);
      if (typeof value!.name !== "string" || !SKILL_NAME.test(value!.name) || typeof value!.description !== "string" || value!.description.length > 1024) throw bad();
      return { version: 1, operation: "new-skill", name: value!.name, description: value!.description };
    default: throw bad();
  }
}

async function jsonBody(req: IncomingMessage, limit: number, signal: AbortSignal): Promise<unknown> {
  if (!/^application\/json(?:;|$)/i.test(req.headers["content-type"] ?? "")) throw new CloudProjectFileError(400, "Send JSON");
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    signal.throwIfAborted();
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    if ((total += bytes.length) > limit) { req.destroy(); throw new CloudProjectFileError(413, "The request is too large"); }
    chunks.push(bytes);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); } catch { throw new CloudProjectFileError(400, "Send JSON"); }
}

export const AGENT_SETUP_PATHS = ["/api/app/agent-setup", "/api/app/agent-setup/files", "/api/app/agent-setup/actions", "/api/app/project-guide"];

export class AgentSetupRoutes {
  #files: CloudProjectFiles<string>;
  constructor(private readonly dependencies: AgentSetupDependencies) {
    this.#files = new CloudProjectFiles({
      authorize: async (accountId, scope) => ({ scope: this.#scope(scope), homeId: this.#home(accountId) }),
      rpc: (homeId, _method, request, signal) => this.dependencies.rpc(homeId, "agentSetup", { request }, signal),
    });
  }
  #home(accountId: string): string {
    const homeId = this.dependencies.ownHome(accountId);
    if (!homeId) throw new CloudProjectFileError(404, "Your cloud machine is not connected");
    return homeId;
  }
  #scope(scope: CloudFileScope): CloudFileScope {
    if (scope.projectId !== SETUP_PROJECT || scope.attemptId !== scope.workspaceId || scope.ownerEpoch !== 0 || !ROOT.test(scope.workspaceId)) {
      throw new CloudProjectFileError(403, "That agent setup file is not available");
    }
    return scope;
  }
  /** One request for a path in AGENT_SETUP_PATHS, already behind account auth. */
  async handle(req: IncomingMessage, url: URL, accountId: string, signal: AbortSignal, saveBody: () => Promise<unknown>): Promise<unknown> {
    const { pathname } = url;
    if (pathname === "/api/app/agent-setup/files") {
      if (req.method === "GET") return this.#files.query(accountId, url, signal);
      if (req.method === "POST" && !url.search) return this.#files.save(accountId, await saveBody(), signal);
      throw new CloudProjectFileError(405, "Use GET to read agent setup files and POST to save one");
    }
    if (pathname === "/api/app/agent-setup/actions") {
      if (req.method !== "POST" || url.search) throw new CloudProjectFileError(405, "Use POST for agent setup actions");
      const action = actionFrom(await jsonBody(req, 8 * 1024, signal));
      const homeId = this.#home(accountId);
      const reply = await this.dependencies.rpc(homeId, "agentSetup", { request: action }, signal);
      if (this.#home(accountId) !== homeId) throw new CloudProjectFileError(403, "Your cloud machine changed");
      return overviewFrom(reply);
    }
    if (req.method !== "GET") throw new CloudProjectFileError(405, "Use GET");
    if (pathname === "/api/app/agent-setup") {
      if (url.search) throw new CloudProjectFileError(400, "Invalid agent setup request");
      const homeId = this.#home(accountId);
      return overviewFrom(await this.dependencies.rpc(homeId, "agentSetup", { request: { version: 1, operation: "overview" } }, signal));
    }
    // A project's instruction files: only for projects on the account's own home.
    const workspaceId = url.searchParams.get("workspaceId") ?? "";
    if (!workspaceId || workspaceId.length > 256 || [...url.searchParams.keys()].some((k) => k !== "workspaceId") || url.searchParams.getAll("workspaceId").length !== 1) {
      throw new CloudProjectFileError(400, "Select a project");
    }
    const homeId = this.#home(accountId);
    if (this.dependencies.workspace(accountId, workspaceId).homeId !== homeId) throw new CloudProjectFileError(403, "Only projects on your own cloud machine are shown here");
    return guideFrom(await this.dependencies.rpc(homeId, "projectGuide", { workspaceId }, signal));
  }
}
