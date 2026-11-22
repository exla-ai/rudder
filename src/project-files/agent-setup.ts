import { constants } from "node:fs";
import { lstat, mkdir, open, readdir, readlink, realpath, rename, stat, symlink } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { ProjectFileError, ProjectFileService, validateFileScope, type FileGrant } from "./service.js";
import type { FileScope, ProjectFileRequest, ProjectFileResult } from "./types.js";

/** Setup that applies to every agent on this machine: the instructions Claude
 * Code and Codex read everywhere, their skills, and Claude's rules, subagents
 * and commands. Only the owner's own home serves it (see the Cloud routes).
 *
 * What each CLI reads (Claude Code 2.1.327, codex-cli 0.169.0, from their
 * binaries): Claude reads $CLAUDE_CONFIG_DIR (default ~/.claude) CLAUDE.md,
 * rules/, skills/<name>/SKILL.md (symlinked folders too; `synced` is its own),
 * agents/ and commands/. Codex reads $CODEX_HOME (default ~/.codex)
 * AGENTS.override.md when it has text, else AGENTS.md, and skills from
 * $CODEX_HOME/skills and ~/.agents/skills (CODEX_HOME does not move that one).
 *
 * Files are reached through ProjectFileService with narrow roots: the real
 * instructions file alone, each skill's folder, and a few named folders, never
 * an agent's whole config folder, where sign-in tokens live. Credential names
 * are refused here even inside those roots.
 */
export type Agent = "claude" | "codex";
export type SetupPaths = {
  claude: string;          // CLAUDE_CONFIG_DIR or ~/.claude
  codex: string;           // CODEX_HOME or ~/.codex
  shared: string;          // new skills live here and are linked to both agents
  /** Skill folders each agent reads, in its own precedence order. */
  skills: Record<Agent, string[]>;
};

export function setupPaths(env: NodeJS.ProcessEnv = process.env, home = homedir()): SetupPaths {
  const claude = env.CLAUDE_CONFIG_DIR ? path.resolve(env.CLAUDE_CONFIG_DIR) : path.join(home, ".claude");
  const codex = env.CODEX_HOME ? path.resolve(env.CODEX_HOME) : path.join(home, ".codex");
  const shared = path.join(home, ".agents", "skills");
  return { claude, codex, shared, skills: { claude: [path.join(claude, "skills")], codex: [path.join(codex, "skills"), shared] } };
}

const INSTRUCTIONS: Record<Agent, string> = { claude: "CLAUDE.md", codex: "AGENTS.md" };
const CODEX_OVERRIDE = "AGENTS.override.md";
const AGENT_NAME: Record<Agent, string> = { claude: "Claude", codex: "Codex" };
const other = (agent: Agent): Agent => (agent === "claude" ? "codex" : "claude");
const SETUP_PROJECT = "agent-setup";
const SKILL_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;
/** Claude keeps the skills it syncs from the account in this folder itself. */
const CLAUDE_RESERVED = "synced";
const REVISION = /^[a-f0-9]{64}$/;
/** Where the CLIs keep sign-ins and tokens. Refused by name in every root. */
const SECRET = /^(?:auth\.json|\.credentials\.json|credentials\.json|\.device-keys\.json|\.claude.*\.json|\.config\.json|gateway_oauth\.json|token\.json|.*\.token|.*\.age)$/i;

export type InstructionsState = {
  agent: Agent;
  path: string;                       // as the agent reads it, with ~ for home
  state: "file" | "link" | "missing";
  linksTo: string | null;             // display path of a link's target
  root: string | null;                // file scope workspaceId when readable
};
export type SkillRow = {
  name: string;
  description: string;
  agents: Agent[];                    // who reads this exact folder
  root: string | null;                // file scope workspaceId when browsable
  location: string;                   // display path of the real folder
  note: string;
};
export type SetupOverview = {
  agents: Record<Agent, { dir: string; present: boolean }>;
  instructions: { claude: InstructionsState; codex: InstructionsState; shared: boolean };
  skills: SkillRow[];
  folders: { id: string; label: string; root: string }[];
};
export type ProjectGuideFile = { path: string; kind: "file" | "link"; linksTo: string | null; imports: string[]; readBy: Agent[] };
export type ProjectGuide = { files: ProjectGuideFile[]; shared: boolean };

export type AgentSetupRequest =
  | { version: 1; operation: "overview" }
  | ProjectFileRequest
  | { version: 1; operation: "link-instructions"; keep: Agent; replacedRevision?: string }
  | { version: 1; operation: "share-skill"; name: string; from: Agent }
  | { version: 1; operation: "new-skill"; name: string; description: string };

const isInside = (parent: string, child: string) => child === parent || child.startsWith(parent.endsWith("/") ? parent : `${parent}/`);
const revisionOf = (bytes: Buffer) => createHash("sha256").update(bytes).digest("hex");
const scopeFor = (id: string): FileScope => ({ projectId: SETUP_PROJECT, workspaceId: id, attemptId: id, ownerEpoch: 0 });
const missing = (error: unknown) => ["ENOENT", "ENOTDIR"].includes((error as NodeJS.ErrnoException)?.code ?? "");
async function exists(at: string): Promise<boolean> { try { await lstat(at); return true; } catch (error) { if (missing(error)) return false; throw error; } }
async function realOrNull(at: string): Promise<string | null> { try { return await realpath(at); } catch (error) { if (missing(error)) return null; throw error; } }

/** At most `limit` bytes from the start of a regular file. */
async function head(at: string, limit: number): Promise<Buffer> {
  const file = await open(at, constants.O_RDONLY | constants.O_NONBLOCK);
  try {
    if (!(await file.stat()).isFile()) throw new ProjectFileError("unsupported", "Not a regular file");
    const buffer = Buffer.alloc(limit);
    const { bytesRead } = await file.read(buffer, 0, limit, 0);
    return buffer.subarray(0, bytesRead);
  } finally { await file.close(); }
}

/** name and description from SKILL.md frontmatter. A long description is
 * often YAML block text (`description: |` or `>`, then indented lines) or a
 * plain value continued on indented lines: those lines are its value. */
export function skillFrontmatter(text: string): { name: string; description: string } {
  const match = /^---\r?\n([\s\S]*?)\r?\n---/.exec(text);
  const fields: Record<string, string> = {};
  let key = "";
  let literal = false;
  let block: string[] = [];
  const finish = () => {
    if (key && block.length) fields[key] = [fields[key], ...block].filter(Boolean).join(literal ? "\n" : " ");
    block = [];
  };
  for (const line of (match?.[1] ?? "").split(/\r?\n/)) {
    if (key && /^\s+\S/.test(line)) {
      block.push(line.trim());
      continue;
    }
    finish();
    key = "";
    const field = /^([A-Za-z_-]+):\s*(.*)$/.exec(line);
    if (!field) continue;
    key = field[1].toLowerCase();
    const value = field[2].trim();
    literal = /^\|[-+]?$/.test(value);
    fields[key] = /^[|>][-+]?$/.test(value) ? "" : value.replace(/^(["'])(.*)\1$/, "$2");
  }
  finish();
  return { name: fields.name ?? "", description: fields.description ?? "" };
}

export class AgentSetup {
  readonly paths: SetupPaths;
  readonly #home: string;
  readonly #projectDirs: () => string[];
  readonly #files: ProjectFileService;

  constructor(options: { projectDirs: () => string[]; paths?: SetupPaths; home?: string }) {
    this.#home = options.home ?? homedir();
    this.paths = options.paths ?? setupPaths(process.env, this.#home);
    this.#projectDirs = options.projectDirs;
    this.#files = new ProjectFileService(async (scope) => this.#grant(scope));
  }

  #show(at: string): string { return isInside(this.#home, at) ? `~${at.slice(this.#home.length)}` : at; }

  /** The file an agent reads everywhere: Codex's override wins while it has text. */
  async #instructionsPath(agent: Agent): Promise<string> {
    if (agent === "codex") {
      const override = path.join(this.paths.codex, CODEX_OVERRIDE);
      try { if ((await head(override, 64 * 1024)).toString("utf8").trim()) return override; }
      catch (error) { if (!missing(error) && !(error instanceof ProjectFileError)) throw error; }
    }
    return path.join(this.paths[agent], INSTRUCTIONS[agent]);
  }

  /** The one entry point of the home RPC. */
  async handle(request: AgentSetupRequest, signal?: AbortSignal): Promise<SetupOverview | ProjectFileResult> {
    if (request?.version !== 1) throw new ProjectFileError("invalid", "Unsupported agent setup request");
    switch (request.operation) {
      case "overview": return this.overview();
      case "list": case "read": case "write": return this.#dispatch(request, signal);
      case "link-instructions": await this.linkInstructions(request.keep, request.replacedRevision); return this.overview();
      case "share-skill": await this.shareSkill(request.name, request.from); return this.overview();
      case "new-skill": await this.newSkill(request.name, request.description); return this.overview();
      default: throw new ProjectFileError("invalid", "Unsupported agent setup request");
    }
  }

  async #dispatch(request: ProjectFileRequest, signal?: AbortSignal): Promise<ProjectFileResult> {
    if (typeof request.path === "string" && request.path.split("/").some((part) => SECRET.test(part))) {
      throw new ProjectFileError("denied", "Sign-in and token files are never shown");
    }
    const result = await this.#files.dispatch(request, signal);
    if ("entries" in result) return { ...result, entries: result.entries.filter((entry) => !SECRET.test(entry.name)) };
    return result;
  }

  // ---------- Roots ----------

  /** Real folders a skill may live in: the agents' skill folders, the shared
   * one, and the home's projects (a skill linked from a repository). */
  async #skillAreas(): Promise<string[]> {
    const areas = [...new Set([...this.paths.skills.claude, ...this.paths.skills.codex, this.paths.shared])];
    const real = await Promise.all([...areas, ...this.#projectDirs()].map(realOrNull));
    return real.filter((at): at is string => !!at);
  }

  /** A real folder is never an agent config folder itself, nor holds one. */
  #safeRoot(real: string): boolean {
    return ![this.paths.claude, this.paths.codex, this.#home].some((dir) => isInside(real, dir));
  }

  async #grant(scope: FileScope): Promise<FileGrant> {
    const id = validateFileScope(scope).workspaceId;
    if (scope.projectId !== SETUP_PROJECT || scope.attemptId !== id || scope.ownerEpoch !== 0) throw new ProjectFileError("denied", "Agent setup access is no longer current");
    const denied = () => new ProjectFileError("denied", "That agent setup file is not available");
    const instructions = /^instructions:(claude|codex)$/.exec(id);
    if (instructions) {
      const agent = instructions[1] as Agent;
      const real = await realOrNull(await this.#instructionsPath(agent));
      if (!real || !(await stat(real)).isFile() || SECRET.test(path.basename(real))) throw denied();
      return { scope: scopeFor(id), root: path.dirname(real), access: { kind: "host" }, only: path.basename(real) };
    }
    const skill = /^skill:(claude|codex|shared):(.+)$/.exec(id);
    if (skill) {
      const [, where, name] = skill;
      if (!SKILL_NAME.test(name)) throw denied();
      const dirs = where === "shared" ? [this.paths.shared] : this.paths.skills[where as Agent];
      for (const dir of dirs) {
        const real = await realOrNull(path.join(dir, name));
        if (!real) continue;
        const areas = await this.#skillAreas();
        if (!(await stat(real)).isDirectory() || !this.#safeRoot(real) || !areas.some((area) => isInside(area, real))) throw denied();
        return { scope: scopeFor(id), root: real, access: { kind: "host" } };
      }
      throw denied();
    }
    const folder = this.#folders().find((item) => item.id === id);
    if (folder) {
      const real = await realOrNull(folder.dir);
      if (!real || !(await stat(real)).isDirectory() || !this.#safeRoot(real)) throw denied();
      return { scope: scopeFor(id), root: real, access: { kind: "host" } };
    }
    throw denied();
  }

  #folders() {
    return [
      { id: "folder:claude-rules", label: "Claude rules", dir: path.join(this.paths.claude, "rules") },
      { id: "folder:claude-agents", label: "Claude subagents", dir: path.join(this.paths.claude, "agents") },
      { id: "folder:claude-commands", label: "Claude commands", dir: path.join(this.paths.claude, "commands") },
    ];
  }

  // ---------- Overview ----------

  async overview(): Promise<SetupOverview> {
    const agents = {
      claude: { dir: this.#show(this.paths.claude), present: await exists(this.paths.claude) },
      codex: { dir: this.#show(this.paths.codex), present: await exists(this.paths.codex) },
    };
    const claude = await this.#instructions("claude");
    const codex = await this.#instructions("codex");
    const realClaude = await realOrNull(await this.#instructionsPath("claude"));
    const realCodex = await realOrNull(await this.#instructionsPath("codex"));
    const folders = [];
    for (const folder of this.#folders()) if (await realOrNull(folder.dir)) folders.push({ id: folder.id, label: folder.label, root: folder.id });
    return { agents, instructions: { claude, codex, shared: !!realClaude && realClaude === realCodex }, skills: await this.#skills(), folders };
  }

  async #instructions(agent: Agent): Promise<InstructionsState> {
    const at = await this.#instructionsPath(agent);
    const base = { agent, path: this.#show(at), linksTo: null, root: null };
    let info;
    try { info = await lstat(at); } catch (error) { if (missing(error)) return { ...base, state: "missing" }; throw error; }
    const real = await realOrNull(at);
    const readable = !!real && (await stat(real)).isFile();
    if (info.isSymbolicLink()) return { ...base, state: "link", linksTo: real ? this.#show(real) : await readlink(at), root: readable ? `instructions:${agent}` : null };
    return { ...base, state: "file", root: readable ? `instructions:${agent}` : null };
  }

  async #skills(): Promise<SkillRow[]> {
    const areas = await this.#skillAreas();
    // One row per real folder; an agent reads it when any of its skill folders holds it.
    const rows = new Map<string, SkillRow & { where: "claude" | "codex" | "shared" }>();
    const byName = new Map<string, string[]>();
    const places: { where: "claude" | "codex" | "shared"; dir: string; readers: Agent[] }[] = [];
    for (const agent of ["claude", "codex"] as Agent[]) {
      for (const dir of this.paths.skills[agent]) {
        const where = dir === this.paths.shared ? "shared" : agent;
        const known = places.find((place) => place.dir === dir);
        if (known) known.readers.push(agent);
        else places.push({ where, dir, readers: [agent] });
      }
    }
    for (const place of places) {
      let names: string[];
      try { names = (await readdir(place.dir)).filter((name) => SKILL_NAME.test(name) && !(place.where === "claude" && name === CLAUDE_RESERVED)).sort(); }
      catch (error) { if (missing(error)) continue; throw error; }
      for (const name of names) {
        const real = await realOrNull(path.join(place.dir, name));
        if (!real || !(await stat(real)).isDirectory()) continue;
        const existing = rows.get(real);
        if (existing) { for (const reader of place.readers) if (!existing.agents.includes(reader)) existing.agents.push(reader); continue; }
        let meta = { name, description: "" };
        let note = "";
        try { meta = { ...skillFrontmatter((await head(path.join(real, "SKILL.md"), 16 * 1024)).toString("utf8")), }; if (!meta.name) meta.name = name; }
        catch (error) { if (!missing(error)) throw error; note = "This folder has no SKILL.md, so agents do not load it."; }
        const browsable = this.#safeRoot(real) && areas.some((area) => isInside(area, real));
        if (!browsable) note = "Its files live outside the agent and project folders, so they are not shown here.";
        rows.set(real, { where: place.where, name: meta.name, description: meta.description.slice(0, 1024), agents: [...place.readers],
          root: browsable ? `skill:${place.where}:${name}` : null, location: this.#show(real), note });
        byName.set(meta.name, [...(byName.get(meta.name) ?? []), real]);
      }
    }
    for (const [name, reals] of byName) {
      if (reals.length < 2) continue;
      for (const real of reals) {
        const row = rows.get(real)!;
        row.note ||= `Each agent has its own copy of ${name}. Edits to one do not reach the other.`;
      }
    }
    return [...rows.values()].map(({ where: _where, ...row }) => row).sort((a, b) => a.name.localeCompare(b.name));
  }

  // ---------- Links ----------

  /** One instructions file for both agents: `keep`'s file stays, the other
   * agent's path becomes a link to it. A separate file there is kept beside
   * it as <name>.replaced-<time>, and only replaced while it still holds the
   * revision the user was shown. */
  async linkInstructions(keep: Agent, replacedRevision?: string): Promise<void> {
    if (keep !== "claude" && keep !== "codex") throw new ProjectFileError("invalid", "Choose whose instructions to keep");
    const target = await this.#instructionsPath(keep);
    const real = await realOrNull(target);
    if (!real || !(await stat(real)).isFile()) throw new ProjectFileError("missing", `${AGENT_NAME[keep]}'s ${INSTRUCTIONS[keep]} does not exist`);
    const linked = other(keep);
    if (!(await exists(this.paths[linked]))) throw new ProjectFileError("missing", `${AGENT_NAME[linked]} is not set up on this machine`);
    const at = await this.#instructionsPath(linked);
    let info;
    try { info = await lstat(at); } catch (error) { if (!missing(error)) throw error; }
    if (info?.isSymbolicLink()) {
      if (await realOrNull(at) === real) return;
      throw new ProjectFileError("invalid", `${this.#show(at)} already links to another file`);
    }
    if (info) {
      if (!info.isFile()) throw new ProjectFileError("unsupported", `${this.#show(at)} is not a regular file`);
      if (!replacedRevision || !REVISION.test(replacedRevision) || revisionOf(await head(at, 4 * 1024 * 1024 + 1)) !== replacedRevision) {
        throw new ProjectFileError("changed", `${this.#show(at)} changed since you looked at it. Nothing was linked.`);
      }
      const stamp = new Date().toISOString().replace(/[-:]/g, "").replace(/\..*$/, "");
      await rename(at, `${at}.replaced-${stamp}`);
    }
    await symlink(target, at);
  }

  /** `from`'s skill also reaches the other agent: a link in its skill folder. */
  async shareSkill(name: string, from: Agent): Promise<void> {
    if (!SKILL_NAME.test(name) || (from !== "claude" && from !== "codex")) throw new ProjectFileError("invalid", "Choose a skill");
    let real: string | null = null;
    for (const dir of this.paths.skills[from]) if ((real = await realOrNull(path.join(dir, name)))) break;
    if (!real || !(await stat(real)).isDirectory()) throw new ProjectFileError("missing", `${AGENT_NAME[from]} has no skill named ${name}`);
    const to = other(from);
    for (const dir of this.paths.skills[to]) {
      if (await realOrNull(path.join(dir, name)) === real) return;
      if (await exists(path.join(dir, name))) throw new ProjectFileError("invalid", `${AGENT_NAME[to]} already has a different skill named ${name}`);
    }
    if (!(await exists(this.paths[to]))) throw new ProjectFileError("missing", `${AGENT_NAME[to]} is not set up on this machine`);
    const folder = this.paths.skills[to][0];
    await mkdir(folder, { recursive: true });
    await symlink(real, path.join(folder, name));
  }

  /** A new skill in the shared folder, read by both agents. */
  async newSkill(name: string, description: string): Promise<void> {
    // Codex drops skills whose name is over 64 characters.
    if (!SKILL_NAME.test(name) || name.includes(".") || name.length > 64 || name === CLAUDE_RESERVED) throw new ProjectFileError("invalid", "Use up to 64 letters, numbers and dashes for the skill name");
    if (typeof description !== "string" || !description.trim() || description.length > 1024 || /[\r\n\x00-\x1f]/.test(description)) {
      throw new ProjectFileError("invalid", "Describe in one line when agents should use the skill");
    }
    for (const agent of ["claude", "codex"] as Agent[]) {
      for (const dir of this.paths.skills[agent]) if (await exists(path.join(dir, name))) throw new ProjectFileError("invalid", `A skill named ${name} already exists`);
    }
    const folder = path.join(this.paths.shared, name);
    await mkdir(this.paths.shared, { recursive: true });
    await mkdir(folder);
    const title = name.split(/[-_]/).filter(Boolean).map((word) => word[0].toUpperCase() + word.slice(1)).join(" ");
    const file = await open(path.join(folder, "SKILL.md"), constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o644);
    try { await file.writeFile(`---\nname: ${name}\ndescription: ${description.trim()}\n---\n\n# ${title}\n\nWhat the agent should do, step by step.\n`); }
    finally { await file.close(); }
    for (const agent of ["claude", "codex"] as Agent[]) {
      if (this.paths.skills[agent].includes(this.paths.shared) || !(await exists(this.paths[agent]))) continue;
      const own = this.paths.skills[agent][0];
      await mkdir(own, { recursive: true });
      await symlink(folder, path.join(own, name));
    }
  }

  // ---------- A project's instructions ----------

  /** The files agents read in a project checkout, and whether one leads to
   * the other (a link, or Claude's `@AGENTS.md` import). */
  async projectGuide(dir: string): Promise<ProjectGuide> {
    const root = await realpath(dir);
    const names = ["AGENTS.md", "AGENTS.override.md", "CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md", "RUDDER.md", "README.md"];
    const files: ProjectGuideFile[] = [];
    for (const name of names) {
      const at = path.join(root, name);
      let info;
      try { info = await lstat(at); } catch (error) { if (missing(error)) continue; throw error; }
      let linksTo: string | null = null;
      if (info.isSymbolicLink()) {
        const real = await realOrNull(at);
        if (!real || !isInside(root, real) || !(await stat(real)).isFile()) continue;
        linksTo = path.relative(root, real);
      } else if (!info.isFile()) continue;
      const imports = name.includes("CLAUDE") && !linksTo
        ? [...(await head(at, 64 * 1024)).toString("utf8").matchAll(/(?:^|\s)@([\w./-]+\.md)\b/g)].map((m) => path.normalize(path.join(path.dirname(name), m[1]))).filter((p) => !p.startsWith(".."))
        : [];
      files.push({ path: name, kind: linksTo ? "link" : "file", linksTo, imports, readBy: [] });
    }
    const target = (file: ProjectGuideFile) => file.linksTo ?? file.path;
    const claudeReads = new Set<string>();
    const codexReads = new Set<string>();
    for (const file of files) {
      if (["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"].includes(file.path)) { claudeReads.add(target(file)); for (const p of file.imports) claudeReads.add(p); }
    }
    const override = files.find((file) => file.path === "AGENTS.override.md");
    const agents = override ?? files.find((file) => file.path === "AGENTS.md");
    if (agents) codexReads.add(target(agents));
    for (const file of files) {
      file.readBy = (["claude", "codex"] as Agent[]).filter((agent) => (agent === "claude" ? claudeReads : codexReads).has(target(file)));
    }
    return { files, shared: [...codexReads].some((p) => claudeReads.has(p)) };
  }
}
