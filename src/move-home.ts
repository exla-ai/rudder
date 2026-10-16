// Moving live agents from this laptop to the home machine.
//
// The home machine clones every repo from GitHub, so it already has whatever
// is pushed. What it does not have is an agent's IN-PROGRESS work: the jj
// change in its isolated workspace (often on top of local-only integration
// changes) and its conversation. A move carries exactly those, and nothing is
// pushed to GitHub to do it:
//
//   laptop  buildMoveBundle: one git bundle holding every moving agent's @
//           commit (minus what origin's default branch already has), each
//           agent's run.json, and its Claude transcript or Codex rollout.
//   home    adoptMovedAgents: fetch the bundle into the project's clone, give
//           each agent a jj workspace whose @ IS the moved commit (so `jj diff`
//           there shows the same work), place the session where `claude
//           --resume` / `codex resume` look for it, and write a paused run.json
//           the dashboard resumes when it is told to adopt it.
//
// The dashboard on the laptop decides nothing about success: a row is moved
// when the CLI has written `migrated` into its run.json, which happens only
// after the home machine reported the agent staged.

import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  currentJjChangeId,
  currentJjCommitId,
  ensureColocated,
  jjWriteWithStaleRecovery,
  workspaceNameFor,
} from "./jj.js";
import { buildFreshHandoffPrompt, encodeClaudeProjectsCwd, type MigrationCandidate } from "./migration.js";
import { adoptLegacyWorkspaceKey, AGENT_WORKSPACES_DIR, LEGACY_AGENT_WORKSPACES_DIR, workspacePath } from "./state.js";
import type { JsonValue } from "./types.js";
import { ensureDir, nowIso, pathExists, readJson, runCommand, shortHash, writeJson } from "./util.js";

export const MOVE_BUNDLE_VERSION = 1;
const MOVE_REF_PREFIX = "refs/rudder-move/";
/** Statuses a run can be in and still be worth moving: work not yet finished. */
const MOVABLE_STATUSES = new Set(["running", "created", "steering", "verifying", "paused", "orphaned"]);
const NAMED_MOVABLE_STATUSES = new Set(["completed", "failed", "cancelled"]);

export type MoveAgent = {
  runId: string;
  /** Ref inside repo.bundle that points at the agent's @ commit. */
  ref: string;
  commit: string;
  backend: string;
  task: string;
  taskSummary?: string;
  sessionId: string;
  /** Bundle-relative transcript, when the conversation can be resumed. */
  sessionFile?: string;
  /** Codex rollouts keep their date path under ~/.codex/sessions; this is it. */
  codexRolloutPath?: string;
  /** Where the workspace sat below the laptop's repo root, reused on the home. */
  relativeWorkspacePath?: string;
  workspaceName?: string;
  /** Context-rich restart prompt when there is no conversation to resume. */
  freshPrompt?: string;
};

export type MoveManifest = {
  version: number;
  kind: "rudder-move";
  createdAt: string;
  agents: MoveAgent[];
};

export type MoveCandidate = {
  runId: string;
  task: string;
  taskSummary?: string;
  backend: string;
  status: string;
  workspacePath: string;
  workspaceName?: string;
  sessionId?: string;
};

export type AdoptedAgent = {
  runId: string;
  workspacePath: string;
  workspaceName: string;
  jjChangeId: string;
  sessionId: string;
  resumable: boolean;
  alreadyAdopted?: boolean;
};

export type AdoptResult = {
  adopted: AdoptedAgent[];
  failed: Array<{ runId: string; error: string }>;
};

/**
 * Local agents that can move: unfinished runs with an isolated workspace that
 * still exists. Main-checkout rows, planners and cloud-command rows point at
 * the repo root rather than a workspace of their own, so they never qualify.
 * With `runIds`, only those (a named run that does not qualify is an error the
 * caller reports, not something to drop silently).
 */
export async function findMoveCandidates(repoRoot: string, runIds?: string[]): Promise<MoveCandidate[]> {
  const runsDir = path.join(repoRoot, ".rudder", "runs");
  const names = runIds && runIds.length > 0
    ? runIds
    : (await fsp.readdir(runsDir, { withFileTypes: true }).catch(() => []))
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name);
  const out: MoveCandidate[] = [];
  for (const name of names) {
    if (!/^[A-Za-z0-9._-]+$/.test(name)) {
      continue;
    }
    const record = await readJson<Record<string, unknown>>(path.join(runsDir, name, "run.json"));
    if (!record || record.id !== name) {
      continue;
    }
    adoptLegacyWorkspaceKey(record);
    const status = String(record.status ?? "");
    // Named explicitly, a finished agent (an interrupted turn reads as done)
    // can move too: the person asked for that one.
    const named = Boolean(runIds && runIds.length > 0);
    const workspace = (record.workspace ?? {}) as Record<string, unknown>;
    const workspaceDir = typeof workspace.path === "string" ? workspace.path : "";
    if (!(MOVABLE_STATUSES.has(status) || (named && NAMED_MOVABLE_STATUSES.has(status))) || workspace.enabled !== true || !workspaceDir) {
      continue;
    }
    if (path.resolve(workspaceDir) === path.resolve(repoRoot) || !(await pathExists(workspaceDir))) {
      continue;
    }
    const session = (record.session ?? {}) as Record<string, unknown>;
    let sessionId = typeof session.nativeSessionId === "string" && session.nativeSessionId.trim()
      ? session.nativeSessionId.trim()
      : undefined;
    // A Codex row whose id was never pinned: its conversation is the newest
    // MAIN thread (not a subagent's) that ran in its workspace.
    if (!sessionId && String(record.backend ?? "") === "codex") {
      sessionId = (await findCodexMainThreadForCwd(workspaceDir)) ?? undefined;
    }
    out.push({
      runId: name,
      task: String(record.task ?? ""),
      taskSummary: typeof record.taskSummary === "string" ? record.taskSummary : undefined,
      backend: String(record.backend ?? "claude"),
      status,
      workspacePath: workspaceDir,
      workspaceName: typeof workspace.workspaceName === "string" ? workspace.workspaceName : undefined,
      sessionId,
    });
  }
  return out;
}

/** Newest Codex main-thread session whose rollout ran in `cwd` (subagent threads skipped). */
export async function findCodexMainThreadForCwd(cwd: string, root = codexSessionsDir()): Promise<string | null> {
  const target = path.resolve(cwd);
  const files: Array<{ file: string; mtime: number }> = [];
  async function walk(dir: string, depth: number): Promise<void> {
    for (const entry of await fsp.readdir(dir, { withFileTypes: true }).catch(() => [])) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory() && depth < 4) {
        await walk(full, depth + 1);
      } else if (entry.isFile() && entry.name.startsWith("rollout-") && entry.name.endsWith(".jsonl")) {
        const stat = await fsp.stat(full).catch(() => null);
        if (stat && Date.now() - stat.mtimeMs < 14 * 24 * 3600 * 1000) {
          files.push({ file: full, mtime: stat.mtimeMs });
        }
      }
    }
  }
  await walk(root, 0);
  files.sort((a, b) => b.mtime - a.mtime);
  for (const { file } of files.slice(0, 400)) {
    const head = await fsp.open(file, "r").then(async (handle) => {
      try {
        const buffer = Buffer.alloc(16384);
        const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
        return buffer.subarray(0, bytesRead).toString("utf8");
      } finally {
        await handle.close();
      }
    }).catch(() => "");
    // The session_meta line carries Codex's whole instructions, so it can be
    // longer than what was read: pick the fields out instead of parsing it.
    if (!head.includes('"type":"session_meta"') || !head.includes('"source":"cli"')) {
      continue;
    }
    const id = /[{,]"id":"([A-Za-z0-9-]+)"/.exec(head)?.[1];
    const rolloutCwd = /"cwd":"((?:[^"\\]|\\.)*)"/.exec(head)?.[1];
    if (id && rolloutCwd && path.resolve(rolloutCwd) === target) {
      return id;
    }
  }
  return null;
}

/** Where `codex resume` finds a session: ~/.codex/sessions/<y>/<m>/<d>/rollout-…-<id>.jsonl. */
export function codexSessionsDir(): string {
  const codexHome = process.env.CODEX_HOME?.trim() || path.join(os.homedir(), ".codex");
  return path.join(codexHome, "sessions");
}

export async function findCodexRollout(sessionId: string, root = codexSessionsDir()): Promise<string | null> {
  if (!/^[A-Za-z0-9-]+$/.test(sessionId)) {
    return null;
  }
  // Newest directories first: a live agent's rollout is recent.
  async function walk(dir: string, depth: number): Promise<string | null> {
    const entries = await fsp.readdir(dir, { withFileTypes: true }).catch(() => []);
    const sorted = entries.sort((a, b) => b.name.localeCompare(a.name));
    for (const entry of sorted) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory() && depth < 4) {
        const found = await walk(full, depth + 1);
        if (found) {
          return found;
        }
      } else if (entry.isFile() && entry.name.startsWith("rollout-") && entry.name.endsWith(`${sessionId}.jsonl`)) {
        return full;
      }
    }
    return null;
  }
  return await walk(root, 0);
}

/** The ref that should not travel in the bundle: what origin's default branch already has. */
async function originDefaultRef(repoRoot: string): Promise<string | null> {
  const head = await runCommand("git", ["symbolic-ref", "-q", "refs/remotes/origin/HEAD"], { cwd: repoRoot, allowFailure: true });
  const candidates = [head.stdout.trim(), "refs/remotes/origin/main", "refs/remotes/origin/master"].filter(Boolean);
  for (const ref of candidates) {
    const exists = await runCommand("git", ["rev-parse", "--verify", "-q", ref], { cwd: repoRoot, allowFailure: true });
    if (exists.code === 0) {
      return ref;
    }
  }
  return null;
}

function isInside(parent: string, child: string): boolean {
  const relative = path.relative(path.resolve(parent), path.resolve(child));
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

function moveRefFor(runId: string): string {
  return `${MOVE_REF_PREFIX}${runId.replace(/[^A-Za-z0-9._-]/g, "-")}`;
}

/**
 * Stage everything the home machine needs to continue `candidates` into
 * `stageDir` (the caller tars it). The agents must already be quiesced: the
 * commit captured here is what moves, and edits made after it stay behind.
 */
export async function buildMoveBundle(
  repoRoot: string,
  candidates: MoveCandidate[],
  stageDir: string,
): Promise<MoveManifest> {
  if (candidates.length === 0) {
    throw new Error("no agents to move");
  }
  await ensureDir(path.join(stageDir, "runs"));
  await ensureDir(path.join(stageDir, "sessions"));
  const agents: MoveAgent[] = [];
  const refs: string[] = [];
  try {
    for (const candidate of candidates) {
      // Reading @ snapshots the workspace, so the commit includes every edit
      // the agent made before it was paused.
      const commit = await currentJjCommitId(candidate.workspacePath);
      if (!/^[0-9a-f]{40}$/.test(commit)) {
        throw new Error(`could not read the jj change in ${candidate.workspacePath}`);
      }
      const ref = moveRefFor(candidate.runId);
      await runCommand("git", ["update-ref", ref, commit], { cwd: repoRoot });
      refs.push(ref);

      const runJsonSource = path.join(repoRoot, ".rudder", "runs", candidate.runId, "run.json");
      await fsp.cp(runJsonSource, path.join(stageDir, "runs", `${candidate.runId}.json`));

      let sessionFile: string | undefined;
      let codexRolloutPath: string | undefined;
      if (candidate.sessionId && candidate.backend === "claude") {
        const jsonl = path.join(
          os.homedir(),
          ".claude",
          "projects",
          encodeClaudeProjectsCwd(path.resolve(candidate.workspacePath)),
          `${candidate.sessionId}.jsonl`,
        );
        if (await pathExists(jsonl)) {
          sessionFile = path.posix.join("sessions", `${candidate.runId}.jsonl`);
          await fsp.cp(jsonl, path.join(stageDir, sessionFile));
        }
      } else if (candidate.sessionId && candidate.backend === "codex") {
        const rollout = await findCodexRollout(candidate.sessionId);
        if (rollout) {
          sessionFile = path.posix.join("sessions", `${candidate.runId}.jsonl`);
          codexRolloutPath = path.relative(codexSessionsDir(), rollout).split(path.sep).join("/");
          await fsp.cp(rollout, path.join(stageDir, sessionFile));
        }
      }

      let freshPrompt: string | undefined;
      if (!sessionFile) {
        const record = await readJson<{ turns?: Array<{ prompt: string; source: string }> }>(runJsonSource);
        const asCandidate = {
          runId: candidate.runId,
          task: candidate.task,
          taskSummary: candidate.taskSummary,
          backend: candidate.backend,
        } as MigrationCandidate;
        freshPrompt = buildFreshHandoffPrompt(asCandidate, Array.isArray(record?.turns) ? record!.turns : [])
          .replace(
            "The earlier conversation transcript is not available. Re-establish context from the repo state and continue.",
            "The earlier conversation transcript is not available, but your work so far is in this workspace: run `jj diff` to see it, then continue.",
          );
      }

      const relative = path.relative(repoRoot, candidate.workspacePath);
      agents.push({
        runId: candidate.runId,
        ref,
        commit,
        backend: candidate.backend,
        task: candidate.task,
        taskSummary: candidate.taskSummary,
        sessionId: sessionFile ? candidate.sessionId ?? "" : "",
        sessionFile,
        codexRolloutPath,
        relativeWorkspacePath: relative && !relative.startsWith("..") && !path.isAbsolute(relative)
          ? relative.split(path.sep).join("/")
          : undefined,
        workspaceName: candidate.workspaceName,
        freshPrompt,
      });
    }

    const exclude = await originDefaultRef(repoRoot);
    const bundleArgs = ["bundle", "create", path.join(stageDir, "repo.bundle"), ...refs];
    if (exclude) {
      bundleArgs.push("--not", exclude);
    }
    const bundled = await runCommand("git", bundleArgs, { cwd: repoRoot, allowFailure: true });
    if (bundled.code !== 0) {
      throw new Error(`git bundle failed: ${bundled.stderr.trim() || bundled.stdout.trim()}`);
    }
  } finally {
    for (const ref of refs) {
      await runCommand("git", ["update-ref", "-d", ref], { cwd: repoRoot, allowFailure: true });
    }
  }

  const manifest: MoveManifest = {
    version: MOVE_BUNDLE_VERSION,
    kind: "rudder-move",
    createdAt: nowIso(),
    agents,
  };
  await writeJson(path.join(stageDir, "manifest.json"), manifest as unknown as JsonValue);
  return manifest;
}

/** Where an adopted agent's workspace goes in the home machine's clone. */
export function adoptedWorkspacePath(repoRoot: string, agent: MoveAgent): string {
  const relative = agent.relativeWorkspacePath ?? "";
  const parts = relative.split("/");
  const underWorkspaces = parts.length >= 2
    && (parts[0] === AGENT_WORKSPACES_DIR || parts[0] === LEGACY_AGENT_WORKSPACES_DIR)
    && parts.every((part) => part && part !== "." && part !== "..");
  if (underWorkspaces) {
    // Same place below the root as on the laptop, under the current spelling
    // of the directory; the dashboard's path healing keys off that marker.
    const target = path.join(repoRoot, AGENT_WORKSPACES_DIR, ...parts.slice(1));
    if (isInside(repoRoot, target)) {
      return target;
    }
  }
  return workspacePath(repoRoot, agent.runId, agent.task);
}

async function jjWorkspaceNames(repoRoot: string): Promise<Set<string>> {
  const result = await runCommand("jj", ["workspace", "list", "-T", "name ++ \"\\n\""], { cwd: repoRoot, allowFailure: true });
  return new Set(result.stdout.split("\n").map((line) => line.trim()).filter(Boolean));
}

/**
 * Home side: continue the agents in an extracted move bundle inside the clone
 * at `repoRoot`. Idempotent per run: an agent this clone already adopted from
 * the same commit is reported again rather than staged twice, so a retried
 * move never forks a conversation.
 */
export async function adoptMovedAgents(repoRoot: string, bundleDir: string, homeDir = os.homedir()): Promise<AdoptResult> {
  const manifest = await readJson<MoveManifest>(path.join(bundleDir, "manifest.json"));
  if (!manifest || manifest.kind !== "rudder-move" || !Array.isArray(manifest.agents)) {
    throw new Error("not a Rudder move bundle");
  }
  if (manifest.version > MOVE_BUNDLE_VERSION) {
    throw new Error(`move bundle version ${manifest.version} is newer than this machine's Rudder; update the worker image`);
  }
  const result: AdoptResult = { adopted: [], failed: [] };
  await ensureColocated(repoRoot);

  // The bundle's prerequisites are on origin; a clone that has not fetched
  // since they were pushed needs them first.
  const bundlePath = path.join(bundleDir, "repo.bundle");
  const fetchArgs = ["fetch", "-q", bundlePath, `${MOVE_REF_PREFIX}*:refs/heads/rudder-move/*`];
  let fetched = await runCommand("git", fetchArgs, { cwd: repoRoot, allowFailure: true });
  if (fetched.code !== 0) {
    await runCommand("git", ["fetch", "-q", "origin"], { cwd: repoRoot, allowFailure: true });
    fetched = await runCommand("git", fetchArgs, { cwd: repoRoot, allowFailure: true });
  }
  if (fetched.code !== 0) {
    const error = `could not read the moved work: ${(fetched.stderr || fetched.stdout).trim().split("\n")[0]}`;
    return { adopted: [], failed: manifest.agents.map((agent) => ({ runId: agent.runId, error })) };
  }
  // Make jj see the fetched heads before workspaces are pointed at them.
  await runCommand("jj", ["git", "import"], { cwd: repoRoot, allowFailure: true });

  const workspaceNames = await jjWorkspaceNames(repoRoot);
  for (const agent of manifest.agents) {
    if (!/^[A-Za-z0-9._-]+$/.test(agent.runId) || !/^[0-9a-f]{40}$/.test(agent.commit)) {
      result.failed.push({ runId: String(agent.runId), error: "malformed entry" });
      continue;
    }
    const bookmark = `rudder-move/${agent.ref.slice(MOVE_REF_PREFIX.length)}`;
    try {
      const adopted = await adoptOne(repoRoot, bundleDir, agent, workspaceNames, homeDir);
      result.adopted.push(adopted);
    } catch (error) {
      result.failed.push({ runId: agent.runId, error: error instanceof Error ? error.message : String(error) });
    } finally {
      // The commit stays visible as the workspace's @; the bookmark was only
      // the way in, and a leftover would show up in `jj log` and on push.
      await runCommand("jj", ["bookmark", "delete", bookmark], { cwd: repoRoot, allowFailure: true });
    }
  }
  return result;
}

async function adoptOne(
  repoRoot: string,
  bundleDir: string,
  agent: MoveAgent,
  workspaceNames: Set<string>,
  homeDir: string,
): Promise<AdoptedAgent> {
  const runDir = path.join(repoRoot, ".rudder", "runs", agent.runId);
  const runJsonPath = path.join(runDir, "run.json");
  const existing = await readJson<Record<string, unknown>>(runJsonPath);
  const existingMigration = (existing?.migration ?? {}) as Record<string, unknown>;
  const existingWorkspace = (existing?.workspace ?? {}) as Record<string, unknown>;
  if (existing && existingMigration.fromCommit === agent.commit && typeof existingWorkspace.path === "string"
    && (await pathExists(existingWorkspace.path))) {
    return {
      runId: agent.runId,
      workspacePath: existingWorkspace.path,
      workspaceName: String(existingWorkspace.workspaceName ?? ""),
      jjChangeId: String(existingWorkspace.jjChangeId ?? ""),
      sessionId: String(existingMigration.sessionId ?? ""),
      resumable: Boolean(existingMigration.sessionId),
      alreadyAdopted: true,
    };
  }

  const record = await readJson<Record<string, unknown>>(path.join(bundleDir, "runs", `${agent.runId}.json`));
  if (!record) {
    throw new Error("run record missing from the bundle");
  }
  adoptLegacyWorkspaceKey(record);

  const target = adoptedWorkspacePath(repoRoot, agent);
  if (await pathExists(target)) {
    throw new Error(`${path.relative(repoRoot, target)} already exists on the home machine`);
  }
  let name = agent.workspaceName && /^[A-Za-z0-9._-]+$/.test(agent.workspaceName)
    ? agent.workspaceName
    : workspaceNameFor(agent.runId);
  if (workspaceNames.has(name)) {
    name = `${name}-${shortHash(`${agent.runId}:${agent.commit}`).slice(0, 4)}`;
  }
  await ensureDir(path.dirname(target));
  // By commit id: the imported bookmark keeps the commit visible, and a name
  // like rudder-move/<id> is not a safe revset symbol (`-` is an operator).
  const added = await jjWriteWithStaleRecovery(repoRoot, ["workspace", "add", target, "--name", name, "-r", agent.commit]);
  if (added.code !== 0) {
    throw new Error(`jj workspace add failed: ${(added.stderr || added.stdout).trim().split("\n")[0]}`);
  }
  workspaceNames.add(name);
  // `workspace add` puts a fresh empty change on top; the agent's work is the
  // moved commit itself, so make THAT the working copy (jj drops the empty one).
  const edited = await runCommand("jj", ["edit", agent.commit], { cwd: target, allowFailure: true });
  if (edited.code !== 0) {
    throw new Error(`jj edit failed: ${(edited.stderr || edited.stdout).trim().split("\n")[0]}`);
  }
  const jjChangeId = await currentJjChangeId(target);

  let sessionId = "";
  if (agent.sessionFile && agent.sessionId && /^[A-Za-z0-9-]+$/.test(agent.sessionId)) {
    const staged = path.join(bundleDir, agent.sessionFile);
    if (isInside(bundleDir, staged) && (await pathExists(staged))) {
      let dest: string | null = null;
      if (agent.backend === "claude") {
        dest = path.join(homeDir, ".claude", "projects", encodeClaudeProjectsCwd(target), `${agent.sessionId}.jsonl`);
      } else if (agent.backend === "codex" && agent.codexRolloutPath) {
        const sessionsRoot = process.env.CODEX_HOME?.trim()
          ? path.join(process.env.CODEX_HOME.trim(), "sessions")
          : path.join(homeDir, ".codex", "sessions");
        const candidate = path.join(sessionsRoot, ...agent.codexRolloutPath.split("/"));
        dest = isInside(sessionsRoot, candidate) ? candidate : null;
      }
      if (dest) {
        await ensureDir(path.dirname(dest));
        await fsp.cp(staged, dest, { force: true });
        sessionId = agent.sessionId;
      }
    }
  }

  const workspace = (record.workspace ?? {}) as Record<string, unknown>;
  record.repoRoot = repoRoot;
  record.workspace = {
    ...workspace,
    enabled: true,
    path: target,
    workspaceName: name,
    ...(jjChangeId ? { jjChangeId } : {}),
  };
  record.vcs = "jj";
  // Nothing runs it yet: the dashboard resumes it when told to adopt it, and a
  // record claiming "running" with no process would read as orphaned first.
  record.status = "paused";
  record.lifecyclePhase = "working";
  delete record.process;
  delete record.attention;
  record.session = { ...((record.session ?? {}) as Record<string, unknown>), nativeSessionId: sessionId || null };
  record.migration = {
    origin: "laptop",
    fromCommit: agent.commit,
    sessionId: sessionId || null,
    backend: agent.backend,
    pendingResume: Boolean(sessionId),
    pendingFresh: !sessionId,
    freshPrompt: sessionId ? null : agent.freshPrompt ?? null,
    movedAt: nowIso(),
  };
  record.updatedAt = String(Date.now());
  await ensureDir(runDir);
  await writeJson(runJsonPath, record as unknown as JsonValue);
  return { runId: agent.runId, workspacePath: target, workspaceName: name, jjChangeId, sessionId, resumable: Boolean(sessionId) };
}

/** After the home machine staged them: the laptop's records hand ownership over. */
export async function markRunsMovedHome(repoRoot: string, runIds: string[], repoSlug: string): Promise<void> {
  for (const runId of runIds) {
    const recordPath = path.join(repoRoot, ".rudder", "runs", runId, "run.json");
    const record = await readJson<Record<string, JsonValue>>(recordPath);
    if (!record || record.id !== runId) {
      continue;
    }
    record.status = "migrated";
    record.lifecyclePhase = "cloud-owned";
    record.movedTo = { kind: "home", repo: repoSlug, at: nowIso() };
    delete record.process;
    record.updatedAt = String(Date.now());
    await writeJson(recordPath, record);
  }
}
