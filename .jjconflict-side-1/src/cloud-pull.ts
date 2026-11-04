// Pulling one agent from the home machine back to this computer: the reverse
// of `rudder cloud move` (src/move-home.ts), and built from the same parts.
//
//   home    exportRunForPull (`rudder __pull-export`, run by the supervisor):
//           a move bundle holding the run's jj `@` commit (the working copy is
//           snapshotted first, so uncommitted edits come along; what origin's
//           default branch has stays behind), its run.json and its Claude
//           transcript or Codex rollout, tarred. The supervisor PUTs it to the
//           control plane as a one-shot sync blob.
//   laptop  pullFromHome (`rudder cloud pull <run>`): ask for it, download the
//           blob, and adopt it like the home adopts a move: a jj workspace under
//           .rudder-workspaces whose `@` IS the pulled commit, the conversation
//           placed where `--resume` looks for it at the new cwd, and a `paused`
//           run.json the dashboard lists and resumes on Enter.
//
// It is a copy, not a move: the agent keeps running on the home machine.

import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { findRepoRoot } from "./git.js";
import { currentJjCommitId } from "./jj.js";
import {
  adoptMovedAgents,
  buildMoveBundle,
  findCodexMainThreadForCwd,
  type MoveCandidate,
  type MoveManifest,
} from "./move-home.js";
import { adoptLegacyWorkspaceKey, AGENT_WORKSPACES_DIR, ensureProjectRuntimeIgnored, workspacePath } from "./state.js";
import type { JsonValue } from "./types.js";
import { nowIso, pathExists, readJson, runCommand, shortHash, writeJson } from "./util.js";

const RUN_ID_RE = /^[A-Za-z0-9._-]{1,120}$/;

export type PullExport = {
  head: string;
  base: string | null;
  title: string;
  task: string;
  backend: string;
  model: string | null;
  sessionId: string | null;
};

export type PullImport = {
  path: string;
  runId: string;
  title: string;
  resumable: boolean;
};

function validRunId(runId: string): boolean {
  return RUN_ID_RE.test(runId) && !runId.startsWith(".");
}

async function originDefaultCommit(repoRoot: string): Promise<string | null> {
  const head = await runCommand("git", ["symbolic-ref", "-q", "refs/remotes/origin/HEAD"], { cwd: repoRoot, allowFailure: true });
  for (const ref of [head.stdout.trim(), "refs/remotes/origin/main", "refs/remotes/origin/master"].filter(Boolean)) {
    const sha = await runCommand("git", ["rev-parse", "--verify", "-q", ref], { cwd: repoRoot, allowFailure: true });
    if (sha.code === 0) {
      return sha.stdout.trim();
    }
  }
  return null;
}

/**
 * Home side: bundle run `runId` of the clone at `repoRoot` into `outFile` (a
 * .tgz move bundle with one agent). Any status pulls, the main row included:
 * its `@` is the clone's own working copy.
 */
export async function exportRunForPull(repoRoot: string, runId: string, outFile: string): Promise<PullExport> {
  if (!validRunId(runId)) {
    throw new Error("bad run id");
  }
  const record = await readJson<Record<string, unknown>>(path.join(repoRoot, ".rudder", "runs", runId, "run.json"));
  if (!record || record.id !== runId) {
    throw new Error(`no agent ${runId} on the home machine`);
  }
  adoptLegacyWorkspaceKey(record);
  const workspace = (record.workspace ?? {}) as Record<string, unknown>;
  const ownWorkspace = workspace.enabled === true && typeof workspace.path === "string" && workspace.path
    && (await pathExists(workspace.path)) ? workspace.path : null;
  const workspaceDir = ownWorkspace ?? repoRoot;
  const backend = String(record.backend ?? "claude");
  const session = (record.session ?? {}) as Record<string, unknown>;
  let sessionId = typeof session.nativeSessionId === "string" && session.nativeSessionId.trim() ? session.nativeSessionId.trim() : undefined;
  if (!sessionId && backend === "codex") {
    sessionId = (await findCodexMainThreadForCwd(workspaceDir)) ?? undefined;
  }
  const candidate: MoveCandidate = {
    runId,
    task: String(record.task ?? ""),
    taskSummary: typeof record.taskSummary === "string" ? record.taskSummary : undefined,
    backend,
    status: String(record.status ?? ""),
    workspacePath: workspaceDir,
    workspaceName: ownWorkspace && typeof workspace.workspaceName === "string" ? workspace.workspaceName : undefined,
    sessionId,
  };
  const stage = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-pull-stage-"));
  try {
    const manifest = await buildMoveBundle(repoRoot, [candidate], stage);
    await runCommand("tar", ["-czf", outFile, "-C", stage, "."], { cwd: stage });
    const agent = manifest.agents[0]!;
    return {
      head: agent.commit,
      base: await originDefaultCommit(repoRoot),
      title: (candidate.taskSummary || candidate.task || runId).slice(0, 200),
      task: candidate.task,
      backend,
      model: typeof record.model === "string" ? record.model : null,
      sessionId: agent.sessionId || null,
    };
  } finally {
    await fsp.rm(stage, { recursive: true, force: true });
  }
}

/**
 * Laptop side: adopt a bundle made by exportRunForPull into the checkout at
 * `repoRoot`. A run with the same id already here is replaced only when it is
 * the row that moved to the home machine, or an earlier pull nothing runs.
 */
export async function importPulledRun(repoRoot: string, archive: string, homeDir = os.homedir()): Promise<PullImport> {
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-pull-in-"));
  try {
    await runCommand("tar", ["-xzf", archive, "-C", dir]);
    const manifest = await readJson<MoveManifest>(path.join(dir, "manifest.json"));
    const agent = manifest?.agents?.[0];
    if (!manifest || manifest.kind !== "rudder-move" || manifest.agents.length !== 1 || !agent || !validRunId(agent.runId)) {
      throw new Error("not a Rudder pull bundle");
    }
    const runId = agent.runId;
    const recordPath = path.join(repoRoot, ".rudder", "runs", runId, "run.json");
    const existing = await readJson<Record<string, unknown>>(recordPath);
    if (existing) {
      const migration = (existing.migration ?? {}) as Record<string, unknown>;
      const status = String(existing.status ?? "");
      const replaceable = status === "migrated" || (migration.pulledFrom === "home" && status !== "running");
      if (!replaceable && migration.fromCommit !== agent.commit) {
        throw new Error(`an agent ${runId} already exists in this checkout (${status || "unknown status"}); delete it first`);
      }
    }

    // Same place under .rudder-workspaces as on the home machine, unless that
    // directory is taken here (the workspace this agent left when it moved,
    // or an earlier pull): then a sibling, so nothing local is overwritten.
    const parts = (agent.relativeWorkspacePath ?? "").split("/");
    let relative = parts.length >= 2 && parts[0] === AGENT_WORKSPACES_DIR
      ? agent.relativeWorkspacePath!
      : path.relative(repoRoot, workspacePath(repoRoot, runId, agent.taskSummary || agent.task)).split(path.sep).join("/");
    const sameCommit = existing && ((existing.migration ?? {}) as Record<string, unknown>).fromCommit === agent.commit;
    if (!sameCommit && (await pathExists(path.join(repoRoot, relative)))) {
      relative = `${relative}-${shortHash(`${runId}:${agent.commit}`).slice(0, 4)}`;
      if (await pathExists(path.join(repoRoot, relative))) {
        throw new Error(`${relative} already exists; delete it to pull this agent again`);
      }
    }
    agent.relativeWorkspacePath = relative;
    // adoptMovedAgents reads the manifest from disk.
    await writeJson(path.join(dir, "manifest.json"), manifest as unknown as JsonValue);
    // A record that moved there and back is adopted fresh, not "already adopted".
    if (existing && !sameCommit) {
      await fsp.rm(recordPath, { force: true });
    }

    await ensureProjectRuntimeIgnored(repoRoot);
    const result = await adoptMovedAgents(repoRoot, dir, homeDir);
    const adopted = result.adopted[0];
    if (!adopted) {
      if (existing && !sameCommit) {
        await writeJson(recordPath, existing as unknown as JsonValue);
      }
      throw new Error(result.failed[0]?.error ?? "could not stage the agent here");
    }

    const record = (await readJson<Record<string, unknown>>(recordPath)) ?? {};
    const migration = (record.migration ?? {}) as Record<string, unknown>;
    record.migration = { ...migration, origin: "home", pulledFrom: "home", at: nowIso(), pendingResume: false, pendingFresh: false };
    delete record.movedTo;
    // A main row on the home machine is an isolated worker here, and it belongs
    // to no plan in this checkout.
    if (record.mode === "main" || record.mode === "one-off") {
      record.mode = "execute";
    }
    for (const key of ["nodeId", "planId", "gam", "resolverFor", "resolverRunId"]) {
      delete record[key];
    }
    record.updatedAt = String(Date.now());
    await writeJson(recordPath, record as unknown as JsonValue);
    return {
      path: adopted.workspacePath,
      runId,
      title: String(agent.taskSummary || agent.task || runId).slice(0, 200),
      resumable: adopted.resumable,
    };
  } finally {
    await fsp.rm(dir, { recursive: true, force: true });
  }
}

/** The slice of the cloud client a pull needs (src/cloud.ts builds it). */
export type PullClient = {
  request<T>(pathOrUrl: string, init: { method: string; body?: JsonValue }): Promise<T>;
  download(pathname: string): Promise<Buffer>;
};

type PullResponse = PullExport & { download?: string; repo?: string };

function retryable(message: string): boolean {
  return /\b503\b|waking|asleep|not connected|starting|did not answer|fetch failed/i.test(message);
}

/**
 * `rudder cloud pull <runId> [--json]`, in a local clone of a repo open on the
 * home machine.
 */
export async function pullFromHome(
  args: string[],
  options: { json?: boolean },
  deps: { client: PullClient; githubSlug: (root: string) => Promise<string | null> },
): Promise<PullImport> {
  const runId = args.find((arg) => !arg.startsWith("-")) ?? "";
  if (!validRunId(runId)) {
    throw new Error("Usage: rudder cloud pull <runId> [--json]  (the run id is in the app's agent menu and `rudder cloud home`)");
  }
  const root = findRepoRoot();
  const slug = await deps.githubSlug(root);
  if (!slug) {
    throw new Error("Run `rudder cloud pull` inside a local clone of the GitHub repo the agent works in.");
  }
  const home = await deps.client.request<{ projects?: { repo?: string }[] }>("/api/rudder/home", { method: "GET" });
  if (!(home.projects ?? []).some((project) => project.repo?.toLowerCase() === slug.toLowerCase())) {
    throw new Error(`${slug} is not open on your cloud machine, so it has no agent to pull here.`);
  }
  if (!options.json) {
    process.stderr.write("Asking the cloud machine for the agent's work (waking it if it is asleep)...\n");
  }
  const ask = () => deps.client.request<PullResponse>("/api/rudder/home/pull", { method: "POST", body: { repo: slug, runId } });
  let response: PullResponse;
  try {
    response = await ask();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (!retryable(message)) {
      throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, 5000));
    response = await ask();
  }
  if (!response.download || !/^\/api\/rudder\/home\/syncs\/[^/]+$/.test(response.download)) {
    throw new Error("the cloud machine did not send the agent's work");
  }
  const scratch = await fsp.mkdtemp(path.join(os.tmpdir(), "rudder-pull-"));
  try {
    const archive = path.join(scratch, "pull.tgz");
    await fsp.writeFile(archive, await deps.client.download(response.download));
    const imported = await importPulledRun(root, archive);
    // Not every jj state survives a bundle; read back what the workspace holds.
    const here = await currentJjCommitId(imported.path).catch(() => "");
    if (here && response.head && here !== response.head) {
      process.stderr.write(`note: the workspace is at ${here.slice(0, 12)}, the cloud sent ${response.head.slice(0, 12)}\n`);
    }
    return { ...imported, title: response.title || imported.title };
  } finally {
    await fsp.rm(scratch, { recursive: true, force: true });
  }
}
