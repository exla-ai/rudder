/**
 * Warm snapshots and overlay forks (Linux).
 *
 * A warm snapshot is one jj workspace per base commit (`rudderwarm-<key>`)
 * where the trusted builder installed dependencies and ran the build once. It
 * is never written again: agents get an OverlayFS fork of it instead of a full
 * checkout.
 *
 * A fork is a normal jj workspace added with empty sparse patterns, so its
 * directory on disk holds only `.jj` and jj outside the fork never sees (or
 * deletes) files. Next to it, `<workspace>.fork/` holds:
 * - `upper/`, `work/`: the agent's private OverlayFS layers;
 * - `wc/`: the working-copy state used inside the fork (the snapshot's
 *   tree_state, whose file stats match the shared lower, so the first `jj
 *   status` does not rehash the tree);
 * - `lower`: path of the warm snapshot;
 * - `enter`: runs a command in a private user+mount namespace with the overlay
 *   mounted at the workspace path and `.jj` bound back, as the real user.
 * The native PTY layer runs every agent command whose cwd has a sibling
 * `.fork/enter` through it.
 */
import fs from "node:fs";
import fsp from "node:fs/promises";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { runCommand } from "./util.js";
import { workspacePath } from "./state.js";
import { toolCacheEnv } from "./tool-cache.js";

export const WARM_WORKSPACE_PREFIX = "rudderwarm-";
const KEEP_READY = 2;

export type WarmSnapshot = { key: string; commit: string; dir: string; builtAt: string };

/** `.rudder-workspaces/<repo>/` (where agent workspaces live). */
function workspacesDir(repoRoot: string): string {
  return path.dirname(workspacePath(repoRoot, "warm-probe"));
}

export function warmDir(repoRoot: string): string {
  return path.join(workspacesDir(repoRoot), ".warm");
}

export function forkStateDir(workspace: string): string {
  return `${workspace}.fork`;
}

/** The single parent of the main checkout's @: what `jj workspace add` bases on. */
export async function currentBaseCommit(repoRoot: string): Promise<string | null> {
  const result = await runCommand("jj", ["log", "--no-graph", "--ignore-working-copy", "-r", "@-", "-T", 'commit_id ++ "\\n"'], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const ids = result.stdout.split("\n").map((line) => line.trim()).filter(Boolean);
  return result.code === 0 && ids.length === 1 ? ids[0]! : null;
}

export function readySnapshots(repoRoot: string): WarmSnapshot[] {
  const dir = warmDir(repoRoot);
  let names: string[] = [];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return [];
  }
  const out: WarmSnapshot[] = [];
  for (const name of names.filter((n) => n.endsWith(".ready"))) {
    try {
      const snap = JSON.parse(fs.readFileSync(path.join(dir, name), "utf8")) as WarmSnapshot;
      if (fs.existsSync(path.join(snap.dir, ".jj"))) out.push(snap);
    } catch {
      /* torn marker: ignore */
    }
  }
  return out.sort((a, b) => b.builtAt.localeCompare(a.builtAt));
}

/** Commands from rudder.toml `[warm] commands = [...]`, else detected from lockfiles. */
export function warmCommands(dir: string): string[] {
  const configured = readConfiguredCommands(path.join(dir, "rudder.toml"));
  if (configured) return configured;
  const has = (file: string) => fs.existsSync(path.join(dir, file));
  const commands: string[] = [];
  if (has("pnpm-lock.yaml")) commands.push("pnpm install --frozen-lockfile");
  else if (has("yarn.lock")) commands.push("yarn install --frozen-lockfile");
  else if (has("package-lock.json")) commands.push("npm ci");
  else if (has("package.json")) commands.push("npm install");
  if (has("package.json")) {
    try {
      const pkg = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8")) as { scripts?: Record<string, string> };
      if (pkg.scripts?.build) commands.push(has("pnpm-lock.yaml") ? "pnpm run build" : has("yarn.lock") ? "yarn build" : "npm run build");
    } catch {
      /* unreadable package.json: install only */
    }
  }
  if (has("Cargo.lock") || has("Cargo.toml")) commands.push("cargo build");
  if (has("uv.lock")) commands.push("uv sync");
  else if (has("requirements.txt")) commands.push("python3 -m venv .venv && .venv/bin/pip install -r requirements.txt");
  return commands;
}

function readConfiguredCommands(file: string): string[] | null {
  let text: string;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch {
    return null;
  }
  const section = text.split(/^\s*\[warm\]\s*$/m)[1]?.split(/^\s*\[/m)[0];
  const match = section?.match(/^\s*commands\s*=\s*\[([\s\S]*?)\]/m);
  if (!match) return null;
  return [...match[1]!.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((m) => JSON.parse(`"${m[1]}"`) as string);
}

/**
 * Builds the warm snapshot for `commit` (blocking). Returns it, or null when
 * another builder holds the key or the build failed (a failed key is not
 * retried; a new base commit gets a new key).
 */
export async function buildWarmSnapshot(repoRoot: string, commit: string): Promise<WarmSnapshot | null> {
  const key = commit.slice(0, 12);
  const root = warmDir(repoRoot);
  await fsp.mkdir(root, { recursive: true });
  const marker = (suffix: string) => path.join(root, `${key}.${suffix}`);
  if (fs.existsSync(marker("ready")) || fs.existsSync(marker("failed"))) {
    return readySnapshots(repoRoot).find((snap) => snap.key === key) ?? null;
  }
  let lock: fs.promises.FileHandle;
  try {
    lock = await fsp.open(marker("building"), "wx");
  } catch {
    return null;
  }
  const dir = path.join(root, key);
  const name = `${WARM_WORKSPACE_PREFIX}${key}`;
  try {
    await lock.write(String(process.pid));
    await runCommand("jj", ["workspace", "forget", name], { cwd: repoRoot, allowFailure: true });
    await fsp.rm(dir, { recursive: true, force: true });
    const added = await runCommand("jj", ["workspace", "add", dir, "--name", name, "-r", commit], { cwd: repoRoot, allowFailure: true });
    if (added.code !== 0) throw new Error(added.stderr.trim() || "jj workspace add failed");
    const env = { ...process.env, ...toolCacheEnv("builder") };
    for (const command of warmCommands(dir)) {
      process.stderr.write(`[rudder] warm snapshot ${key}: ${command}\n`);
      const ran = spawnSync("sh", ["-c", command], { cwd: dir, env, stdio: ["ignore", "inherit", "inherit"] });
      if (ran.status !== 0) throw new Error(`${command} exited ${ran.status ?? ran.signal}`);
    }
    // Record file stats for the whole tree once; forks reuse this state.
    await runCommand("jj", ["status"], { cwd: dir, allowFailure: true });
    const snap: WarmSnapshot = { key, commit, dir, builtAt: new Date().toISOString() };
    await fsp.writeFile(marker("ready"), JSON.stringify(snap));
    await pruneWarmSnapshots(repoRoot);
    return snap;
  } catch (error) {
    process.stderr.write(`[rudder] warm snapshot ${key} failed: ${error instanceof Error ? error.message : String(error)}\n`);
    await fsp.writeFile(marker("failed"), String(error));
    return null;
  } finally {
    await lock.close();
    await fsp.rm(marker("building"), { force: true });
  }
}

/** Starts a detached builder for `commit` unless one is ready, failed or running. */
export function startWarmBuild(repoRoot: string, commit: string): void {
  const key = commit.slice(0, 12);
  const root = warmDir(repoRoot);
  if (["ready", "failed", "building"].some((s) => fs.existsSync(path.join(root, `${key}.${s}`)))) return;
  fs.mkdirSync(root, { recursive: true });
  const log = fs.openSync(path.join(root, `${key}.log`), "a");
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), repoRoot, commit], {
    detached: true,
    stdio: ["ignore", log, log],
  });
  child.unref();
  fs.closeSync(log);
}

/** Deletes ready snapshots beyond the newest few that no fork still uses. */
export async function pruneWarmSnapshots(repoRoot: string): Promise<void> {
  const used = new Set<string>();
  const parent = workspacesDir(repoRoot);
  for (const name of fs.readdirSync(parent).filter((n) => n.endsWith(".fork"))) {
    try {
      used.add(fs.readFileSync(path.join(parent, name, "lower"), "utf8").trim());
    } catch {
      /* not a fork */
    }
  }
  for (const snap of readySnapshots(repoRoot).slice(KEEP_READY)) {
    if (used.has(snap.dir)) continue;
    await fsp.rm(path.join(warmDir(repoRoot), `${snap.key}.ready`), { force: true });
    await runCommand("jj", ["workspace", "forget", `${WARM_WORKSPACE_PREFIX}${snap.key}`], { cwd: repoRoot, allowFailure: true });
    await fsp.rm(snap.dir, { recursive: true, force: true });
  }
}

const ENTER_SCRIPT = `#!/bin/sh
# Runs "$@" inside this Rudder fork: private user+mount namespace, overlay of
# the warm snapshot mounted at the workspace path, .jj bound back, real uid.
F="$(dirname "$(readlink -f "$0")")"; M="\${F%.fork}"; U="$(id -u)"; G="$(id -g)"
exec unshare -rm --propagation private sh -ec '
F="$1"; M="$2"; U="$3"; G="$4"; shift 4
mount --bind "$M/.jj" "$F/jjmnt"
mount -t overlay overlay -o "lowerdir=$(cat "$F/lower"),upperdir=$F/upper,workdir=$F/work,userxattr" "$M"
mount --bind "$F/jjmnt" "$M/.jj"
mount --bind "$F/wc" "$M/.jj/working_copy"
cd "$M"
exec unshare -U --map-user="$U" --map-group="$G" -- "$@"' sh "$F" "$M" "$U" "$G" "$@"
`;

/**
 * Creates `targetPath` as an overlay fork of the newest warm snapshot. Returns
 * null (after cleaning up) when there is no snapshot or the overlay cannot be
 * mounted here; the caller then makes a full jj checkout.
 */
export async function createForkWorkspace(params: {
  repoRoot: string;
  targetPath: string;
  workspaceName: string;
  description?: string;
}): Promise<{ snapshot: WarmSnapshot } | { reason: string }> {
  if (process.platform !== "linux") return { reason: "not Linux" };
  const base = await currentBaseCommit(params.repoRoot);
  const snaps = readySnapshots(params.repoRoot);
  // Projects with nothing to install or build keep the plain checkout.
  if (base && !snaps.some((snap) => snap.commit === base) && warmCommands(params.repoRoot).length > 0) {
    startWarmBuild(params.repoRoot, base);
  }
  const snapshot = snaps.find((snap) => snap.commit === base) ?? snaps[0];
  if (!snapshot) return { reason: "no warm snapshot yet (building)" };
  const added = await runCommand(
    "jj",
    ["workspace", "add", params.targetPath, "--name", params.workspaceName, "--sparse-patterns", "empty", "-r", snapshot.commit],
    { cwd: params.repoRoot, allowFailure: true },
  );
  if (added.code !== 0) return { reason: added.stderr.trim() || "jj workspace add failed" };
  const state = forkStateDir(params.targetPath);
  try {
    if (params.description) {
      await runCommand("jj", ["describe", "-m", params.description], { cwd: params.targetPath, allowFailure: true });
    }
    for (const sub of ["upper", "work", "wc", "jjmnt"]) await fsp.mkdir(path.join(state, sub), { recursive: true });
    const wc = path.join(params.targetPath, ".jj", "working_copy");
    await fsp.copyFile(path.join(wc, "checkout"), path.join(state, "wc", "checkout"));
    await fsp.copyFile(path.join(wc, "type"), path.join(state, "wc", "type"));
    await fsp.copyFile(path.join(snapshot.dir, ".jj", "working_copy", "tree_state"), path.join(state, "wc", "tree_state"));
    await fsp.writeFile(path.join(state, "lower"), snapshot.dir);
    await fsp.writeFile(path.join(state, "enter"), ENTER_SCRIPT, { mode: 0o755 });
    const probe = spawnSync(path.join(state, "enter"), ["true"], { cwd: params.targetPath, stdio: ["ignore", "ignore", "pipe"] });
    if (probe.status !== 0) throw new Error(`overlay mount unavailable: ${probe.stderr?.toString().trim()}`);
    return { snapshot };
  } catch (error) {
    await runCommand("jj", ["workspace", "forget", params.workspaceName], { cwd: params.repoRoot, allowFailure: true });
    await removeForkState(params.targetPath);
    await fsp.rm(params.targetPath, { recursive: true, force: true });
    return { reason: error instanceof Error ? error.message : String(error) };
  }
}

/** Removes `<workspace>.fork/` (OverlayFS leaves a mode-000 `work/work`). */
export async function removeForkState(workspace: string): Promise<void> {
  const state = forkStateDir(workspace);
  await fsp.chmod(path.join(state, "work", "work"), 0o700).catch(() => undefined);
  await fsp.rm(state, { recursive: true, force: true });
}

// Detached builder entry: node warm-snapshot.js <repoRoot> <commit>
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const [repoRoot, commit] = process.argv.slice(2);
  if (repoRoot && commit) await buildWarmSnapshot(repoRoot, commit);
}
