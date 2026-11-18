import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import type { RunRecord } from "./types.js";
import { runCommand, runCommandSync } from "./util.js";
import { listRuns } from "./state.js";
import { jjStatus } from "./jj.js";

// ---------------------------------------------------------------------------
// Externally-git contract + plumbing.
//
// Rudder's run isolation, merge and sync run on jj (see src/jj.ts). git.ts only
// owns the surface where Rudder still talks plain git: discovering the repo
// root and reporting branch/commit.
// ---------------------------------------------------------------------------

export function findRepoRoot(cwd = process.cwd()): string {
  // A jj workspace can share the main checkout's Git directory. Git then
  // reports that checkout, which would send local writes outside this worker.
  // Read the workspace marker instead of invoking jj: even discovery commands
  // can snapshot a working copy, and completion/read-only paths must not do so.
  let directory = path.resolve(cwd);
  while (true) {
    try {
      if (fs.statSync(path.join(directory, ".jj")).isDirectory()) return directory;
    } catch {
      // No readable local jj marker; continue ordinary repository discovery.
    }
    // A nested Git checkout belongs to its own project, not an enclosing jj
    // workspace. Let Git resolve its marker or worktree file below.
    if (fs.existsSync(path.join(directory, ".git"))) break;
    const parent = path.dirname(directory);
    if (parent === directory) break;
    directory = parent;
  }
  const result = runCommandSync("git", ["rev-parse", "--show-toplevel"], {
    cwd,
    allowFailure: true,
  });
  if (result.code === 0 && result.stdout.trim()) {
    return result.stdout.trim();
  }
  return path.resolve(cwd);
}

export async function currentBranch(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["branch", "--show-current"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim() || "HEAD";
}

export async function currentCommit(repoRoot: string): Promise<string> {
  const result = await runCommand("git", ["rev-parse", "HEAD"], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.stdout.trim() || "";
}
// A run's workspace status, from jj.
export async function workspaceStatus(run: RunRecord): Promise<string[]> {
  return await jjStatus(run.workspace.path);
}

export async function runHasChanges(run: RunRecord): Promise<boolean> {
  return (await workspaceStatus(run)).length > 0;
}

export function processAlive(pid: number | undefined): boolean {
  if (!pid) {
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export function runProcessAlive(run: RunRecord): boolean {
  return [run.process?.controllerPid, run.process?.backendPid, run.process?.pid]
    .some((pid) => processAlive(pid));
}

export async function activeRunsForCheckout(repoRoot: string, checkoutPath: string): Promise<RunRecord[]> {
  const runs = await listRuns(repoRoot);
  const active: RunRecord[] = [];
  for (const run of runs) {
    if (!["created", "running", "verifying"].includes(run.status)) {
      continue;
    }
    if (!(await sameFilesystemPath(run.workspace.path, checkoutPath))) {
      continue;
    }
    if (runProcessAlive(run)) {
      active.push(run);
    }
  }
  return active;
}

async function sameFilesystemPath(left: string, right: string): Promise<boolean> {
  const normalize = async (value: string) => {
    const resolved = path.resolve(value);
    return await fsp.realpath(resolved).catch(() => resolved);
  };
  return (await normalize(left)) === (await normalize(right));
}
