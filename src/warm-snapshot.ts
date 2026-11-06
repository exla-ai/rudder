/**
 * Warm snapshots, overlay forks (Linux) and clone forks (macOS).
 *
 * A warm snapshot is one jj workspace per base commit (`rudderwarm-<key>`)
 * where the trusted builder installed dependencies and ran the build once
 * (with the shared tool cache, src/tool-cache.ts). It is never written again:
 * agents get a fork of it instead of a full checkout. On macOS a fork is an
 * APFS copy-on-write clone of the snapshot (see createCloneWorkspace). On
 * Linux it is an overlay fork:
 *
 * A fork is a jj workspace on the `rudder-overlay` working copy, added by
 * `rudder-jj rudder-fork-add`: its directory on disk holds only `.jj`, the
 * agent's private OverlayFS layers live in `.jj/working_copy/{upper,work}`,
 * and the warm snapshot directory is the lower layer (linked into the shared
 * `.warm/lowers/<tree>` cache). Snapshot reads only the upper layer (no tree
 * scan); check-out repoints the lower (no copy). Only rudder-jj can load such
 * a workspace. Next to it, `<workspace>.fork/` holds:
 * - `lower`: path of the warm snapshot (for pruning);
 * - `bin/jj`: rudder-jj, first on PATH inside the fork;
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
import { runCommand, rudderJjPath } from "./util.js";
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

/**
 * A fork's files exist only inside its mount namespace: run `command` there
 * when `cwd` is an overlay fork, otherwise unchanged.
 */
export function inFork(cwd: string, command: string, args: string[]): { command: string; args: string[] } {
  const enter = path.join(forkStateDir(cwd), "enter");
  if (process.platform !== "linux" || !fs.existsSync(enter)) return { command, args };
  return { command: enter, args: [command, ...args] };
}

/** The single parent of the main checkout's @: what `jj workspace add` bases on. */
async function commitOf(repoRoot: string, change: string): Promise<string | null> {
  const result = await runCommand("jj", ["log", "--no-graph", "--ignore-working-copy", "-r", change, "-T", 'commit_id ++ "\\n"'], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const ids = result.stdout.split("\n").map((line) => line.trim()).filter(Boolean);
  return result.code === 0 && ids.length === 1 ? ids[0]! : null;
}

export async function currentBaseCommit(repoRoot: string): Promise<string | null> {
  const result = await runCommand("jj", ["log", "--no-graph", "--ignore-working-copy", "-r", "@-", "-T", 'commit_id ++ "\\n"'], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const ids = result.stdout.split("\n").map((line) => line.trim()).filter(Boolean);
  if (result.code === 0 && ids.length === 1) return ids[0]!;
  // After automatic merges @ is itself the merge commit (several parents):
  // its content is current main, so new work starts from it.
  if (result.code !== 0 || ids.length < 2) return null;
  const merge = await runCommand("jj", ["log", "--no-graph", "--ignore-working-copy", "-r", "@", "-T", 'commit_id ++ "\\n"'], {
    cwd: repoRoot,
    allowFailure: true,
  });
  const id = merge.stdout.trim();
  return merge.code === 0 && id ? id : null;
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
    const previous = readySnapshots(repoRoot).find((snap) => snap.key !== key);
    if (previous) await seedFromPrevious(previous.dir, dir);
    const env = { ...process.env, ...toolCacheEnv("builder", { workspace: dir }) };
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

/**
 * Incremental warm build: copies the previous snapshot's untracked top-level
 * entries (dependencies and build outputs such as node_modules, target, .venv)
 * into the new checkout, so install/build only redo what changed.
 */
export async function seedFromPrevious(previousDir: string, dir: string): Promise<void> {
  let names: string[];
  try {
    names = await fsp.readdir(previousDir);
  } catch {
    return;
  }
  for (const name of names) {
    if (name === ".jj" || name === ".git" || fs.existsSync(path.join(dir, name))) continue;
    const from = path.join(previousDir, name);
    const to = path.join(dir, name);
    // GNU cp reflinks where it can; macOS cp -c clones on APFS, else copies.
    const attempts = process.platform === "linux" ? [["-a", "--reflink=auto"]] : [["-cRp"], ["-Rp"]];
    let copied = false;
    for (const flags of attempts) {
      if (spawnSync("cp", [...flags, from, to], { stdio: "ignore" }).status === 0) { copied = true; break; }
      await fsp.rm(to, { recursive: true, force: true });
    }
    if (!copied) await fsp.rm(to, { recursive: true, force: true });
  }
}

const MIN_FREE_FOR_WARM_BUILD = 10 * 1024 ** 3;

/**
 * Why a warm build should not start on this disk, or null. A Mac's disk is
 * shared with everything else on it, and a warm build (install, build, a Rust
 * target dir) can take gigabytes, so it waits for room. Cloud homes size their
 * own disks and always build.
 */
export function warmBuildSkipReason(dir: string, minFree = MIN_FREE_FOR_WARM_BUILD): string | null {
  if (process.platform !== "darwin") return null;
  let free: number;
  try {
    const stats = fs.statfsSync(dir);
    free = stats.bavail * stats.bsize;
  } catch {
    return null;
  }
  if (free >= minFree) return null;
  return `no warm snapshot: ${Math.floor(free / 1024 ** 3)} GB free, a warm build waits for ${Math.ceil(minFree / 1024 ** 3)} GB`;
}

/**
 * Starts a detached builder for `commit` unless one is ready, failed or
 * running. Returns why it did not start one when the disk lacks room.
 */
export function startWarmBuild(repoRoot: string, commit: string): string | null {
  const key = commit.slice(0, 12);
  const root = warmDir(repoRoot);
  if (["ready", "failed", "building"].some((s) => fs.existsSync(path.join(root, `${key}.${s}`)))) return null;
  fs.mkdirSync(root, { recursive: true });
  const skipped = warmBuildSkipReason(root);
  if (skipped) return skipped;
  const log = fs.openSync(path.join(root, `${key}.log`), "a");
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), repoRoot, commit], {
    detached: true,
    stdio: ["ignore", log, log],
  });
  child.unref();
  fs.closeSync(log);
  return null;
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
    await removeLowerLinks(repoRoot, snap.dir);
    await runCommand("jj", ["workspace", "forget", `${WARM_WORKSPACE_PREFIX}${snap.key}`], { cwd: repoRoot, allowFailure: true });
    await fsp.rm(snap.dir, { recursive: true, force: true });
  }
}

/** Drops `.warm/lowers/<tree>` links into a snapshot that is being deleted. */
async function removeLowerLinks(repoRoot: string, target: string): Promise<void> {
  const root = path.join(warmDir(repoRoot), "lowers");
  for (const name of await fsp.readdir(root).catch(() => [] as string[])) {
    const link = path.join(root, name);
    if ((await fsp.readlink(link).catch(() => null)) === target) await fsp.rm(link, { force: true });
  }
}

const ENTER_SCRIPT = `#!/bin/sh
# Runs "$@" inside this Rudder fork: private user+mount namespace, overlay of
# the warm snapshot mounted at the workspace path, .jj bound back, real uid.
F="$(dirname "$(readlink -f "$0")")"; M="\${F%.fork}"; U="$(id -u)"; G="$(id -g)"
exec unshare -rm --propagation private sh -ec '
F="$1"; M="$2"; U="$3"; G="$4"; shift 4
mount --bind "$M/.jj" "$F/jjmnt"
W="$F/jjmnt/working_copy"
mount -t overlay overlay -o "lowerdir=$(readlink -f "$(cat "$W/lower")"),upperdir=$W/upper,workdir=$W/work,userxattr" "$M"
mount --bind "$F/jjmnt" "$M/.jj"
# Login shells rebuild PATH; make every jj on it this fork-aware rudder-jj.
J="$(readlink -f "$F/bin/jj")"
for d in $(printf %s "$PATH" | tr : " ") /usr/local/bin /usr/bin "$HOME/.local/bin"; do
  [ -f "$d/jj" ] && [ "$(readlink -f "$d/jj")" != "$J" ] && mount --bind "$J" "$d/jj" 2>/dev/null || true
done
cd "$M"
PATH="$F/bin:$PATH"; export PATH
exec unshare -U --map-user="$U" --map-group="$G" -- "$@"' sh "$F" "$M" "$U" "$G" "$@"
`;

/** Git's empty tree. As the attribute source it turns off in-tree .gitattributes. */
const EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// Runs inside the mounted fork. $1 = git dir, $2 = throwaway index file,
// $3 = snapshot commit, $4 = base. jj commits are Git commits in the colocated
// repo, so Git's two-tree checkout (what `git checkout` does to switch
// branches) writes exactly the paths that differ, in one process: removals
// (whiteouts in the upper layer), executable bits, symlinks and file/directory
// swaps. It uses its own index file; the repo's index and refs are never
// written. The overrides keep line-ending, filter and attribute settings,
// hooks, sparse checkout and fsmonitor from changing what is written: the
// bytes must be the commit's, as jj writes them. Git without --attr-source
// (before 2.42) fails here, and the launch makes a full checkout.
const MOVE_TO_BASE = `
G="$1"; I="$2"; S="$3"; B="$4"
g() {
  GIT_INDEX_FILE="$I" GIT_ATTR_NOSYSTEM=1 git --git-dir="$G" --work-tree=. --attr-source=${EMPTY_TREE} \\
    -c core.autocrlf=false -c core.symlinks=true -c core.attributesFile=/dev/null -c core.hooksPath=/dev/null \\
    -c core.fsmonitor=false -c core.splitIndex=false "$@"
}
g read-tree "$S"
g read-tree --reset -u --no-sparse-checkout "$S" "$B"
jj rebase -r @ -d "$B"
`;

/** Ready snapshots further than this many files from the base wait for the base's own build. */
const NEAR_SNAPSHOT_FILES = 500;
const WAIT_FOR_BUILD_MS = 90_000;

/** The Git directory behind the jj repo (`.git` when colocated), or null. */
async function gitDirOf(repoRoot: string): Promise<string | null> {
  const result = await runCommand("jj", ["git", "root", "--ignore-working-copy"], { cwd: repoRoot, allowFailure: true });
  const dir = result.stdout.trim();
  return result.code === 0 && dir ? dir : null;
}

/** How many files differ between two commits (Infinity when Git cannot tell). */
async function filesApart(gitDir: string, from: string, to: string): Promise<number> {
  const result = await runCommand("git", ["--git-dir", gitDir, "diff-tree", "-r", "-z", "--no-renames", "--name-only", from, to], { allowFailure: true });
  return result.code === 0 ? result.stdout.split("\0").length - 1 : Infinity;
}

/** The ready snapshot fewest files away from `base` (the newer one on a tie). */
async function nearestSnapshot(gitDir: string | null, snaps: WarmSnapshot[], base: string): Promise<{ snap: WarmSnapshot; files: number }> {
  const files = await Promise.all(snaps.map((snap) => (gitDir ? filesApart(gitDir, snap.commit, base) : Infinity)));
  let best = 0;
  for (let i = 1; i < snaps.length; i += 1) if (files[i]! < files[best]!) best = i;
  return { snap: snaps[best]!, files: files[best]! };
}

/**
 * Waits up to `timeoutMs` for the warm build of `commit` to become ready.
 * Returns null when it fails, or no live builder holds its lock.
 */
async function waitForWarmBuild(repoRoot: string, commit: string, timeoutMs: number): Promise<WarmSnapshot | null> {
  const key = commit.slice(0, 12);
  const marker = (suffix: string) => path.join(warmDir(repoRoot), `${key}.${suffix}`);
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    if (fs.existsSync(marker("ready"))) return readySnapshots(repoRoot).find((snap) => snap.key === key) ?? null;
    if (fs.existsSync(marker("failed"))) return null;
    // A detached builder takes the lock right after it starts.
    if (Date.now() - started > 5_000 && !builderAlive(marker("building"))) return null;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  return null;
}

function builderAlive(lock: string): boolean {
  let pid: number;
  try {
    pid = Number(fs.readFileSync(lock, "utf8").trim());
  } catch {
    return false;
  }
  if (!pid) return true; // lock taken, pid not written yet
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * Creates `targetPath` as a fork of the warm snapshot nearest its base: an
 * overlay fork on Linux, an APFS clone on macOS. A base without its own
 * snapshot starts a build for it; when every ready snapshot is far away, the
 * launch waits a while for that build. Returns a reason (after cleaning up) when
 * there is no snapshot or the fork cannot be made here; the caller then makes
 * a full jj checkout.
 */
export async function createForkWorkspace(params: {
  repoRoot: string;
  targetPath: string;
  workspaceName: string;
  description?: string;
  /** Start from this change (a branch of another agent) instead of main. */
  atChangeId?: string;
}): Promise<{ snapshot: WarmSnapshot } | { reason: string }> {
  if (process.platform !== "linux" && process.platform !== "darwin") return { reason: "not Linux or macOS" };
  const base = params.atChangeId
    ? await commitOf(params.repoRoot, params.atChangeId)
    : await currentBaseCommit(params.repoRoot);
  if (params.atChangeId && !base) return { reason: `change ${params.atChangeId} not found` };
  const snaps = readySnapshots(params.repoRoot);
  // Projects with nothing to install or build keep the plain checkout.
  const warmable = warmCommands(params.repoRoot).length > 0;
  let skipped: string | null = null;
  let snapshot = snaps.find((snap) => snap.commit === base);
  if (base && !snapshot && warmable) skipped = startWarmBuild(params.repoRoot, base);
  let gitDir: string | null = null;
  if (base && !snapshot && snaps.length > 0) {
    gitDir = await gitDirOf(params.repoRoot);
    const nearest = await nearestSnapshot(gitDir, snaps, base);
    snapshot = nearest.snap;
    // Far from every ready snapshot: the base's own build (seeded from the
    // previous one) is usually quicker than patching thousands of files.
    if (nearest.files > NEAR_SNAPSHOT_FILES && warmable && !skipped) {
      snapshot = (await waitForWarmBuild(params.repoRoot, base, WAIT_FOR_BUILD_MS)) ?? snapshot;
    }
  }
  snapshot ??= snaps[0];
  if (!snapshot) return { reason: skipped ?? (warmable ? "no warm snapshot yet (building)" : "nothing to install or build") };
  const rudderJj = rudderJjPath();
  if (!rudderJj) return { reason: "rudder-jj is not available" };
  if (process.platform === "darwin") return await createCloneWorkspace(params, snapshot, base ?? snapshot.commit);
  const move = base && base !== snapshot.commit ? base : null;
  if (move) {
    gitDir ??= await gitDirOf(params.repoRoot);
    if (!gitDir) return { reason: "no Git store behind the jj repo" };
    // A conflicted commit's Git tree holds jj's conflict layout, not files.
    const conflicted = await runCommand(
      "jj",
      ["log", "--no-graph", "--ignore-working-copy", "-r", `${snapshot.commit}|${move}`, "-T", 'if(conflict, "conflict\\n")'],
      { cwd: params.repoRoot, allowFailure: true },
    );
    if (conflicted.code !== 0 || conflicted.stdout.trim()) return { reason: `${move.slice(0, 12)} has conflicts` };
  }
  // Claim the snapshot before adding the fork: a warm build that finishes
  // meanwhile prunes snapshots no `.fork/lower` names, and the nearest one is
  // often not among the newest.
  const state = forkStateDir(params.targetPath);
  await fsp.mkdir(state, { recursive: true });
  await fsp.writeFile(path.join(state, "lower"), snapshot.dir);
  const added = await runCommand(
    "jj",
    [
      "rudder-fork-add", params.targetPath, "--name", params.workspaceName, "-r", snapshot.commit,
      "--lower", snapshot.dir, "--lowers-root", path.join(warmDir(params.repoRoot), "lowers"),
      "-m", params.description ?? "",
    ],
    { cwd: params.repoRoot, allowFailure: true },
  );
  if (added.code !== 0) {
    await removeForkState(params.targetPath);
    await fsp.rm(params.targetPath, { recursive: true, force: true });
    return { reason: added.stderr.trim() || "rudder-jj rudder-fork-add failed" };
  }
  try {
    for (const sub of ["jjmnt", "bin"]) await fsp.mkdir(path.join(state, sub), { recursive: true });
    await fsp.symlink(rudderJj, path.join(state, "bin", "jj"));
    await fsp.writeFile(path.join(state, "enter"), ENTER_SCRIPT, { mode: 0o755 });
    const probe = spawnSync(path.join(state, "enter"), ["true"], { cwd: params.targetPath, stdio: ["ignore", "ignore", "pipe"] });
    if (probe.status !== 0) throw new Error(`overlay mount unavailable: ${probe.stderr?.toString().trim()}`);
    // The snapshot supplies dependencies and build output; the code must be
    // current. Move the fork's change onto the main checkout's base so only the
    // files that differ are written into the upper layer.
    if (move && gitDir) {
      // Write the differing files through the mounted overlay (it records
      // deletions itself), then move @ onto the base. The tree is then
      // identical, so the working copy keeps the warm lower.
      const index = path.join(state, "move-index");
      const moved = await runCommand(path.join(state, "enter"), ["sh", "-ec", MOVE_TO_BASE, "sh", gitDir, index, snapshot.commit, move], {
        cwd: params.targetPath, allowFailure: true,
      });
      await fsp.rm(index, { force: true });
      if (moved.code !== 0) throw new Error(`could not move fork to ${move.slice(0, 12)}: ${moved.stderr.trim()}`);
    }
    return { snapshot };
  } catch (error) {
    await runCommand("jj", ["workspace", "forget", params.workspaceName], { cwd: params.repoRoot, allowFailure: true });
    await removeForkState(params.targetPath);
    await fsp.rm(params.targetPath, { recursive: true, force: true });
    return { reason: error instanceof Error ? error.message : String(error) };
  }
}

/**
 * macOS: `targetPath` becomes an APFS copy-on-write clone of the warm snapshot
 * (`rudder-jj rudder-clone-add`), checked out at `revision` (the base, or the
 * change to branch from). It is an ordinary jj workspace: no mount and no
 * `enter` script, so commands run in it directly. Its files, dependencies and
 * build output share the snapshot's data blocks, and it inherits the
 * snapshot's recorded file states, so jj trusts them instead of hashing. Only
 * files that differ between the snapshot's tree and `revision` are written.
 * A clone no longer needs the snapshot once made, so pruning ignores clones.
 * A failed clone (another volume, no clonefile) leaves nothing behind and the
 * caller makes a full checkout; there is no byte-copy fallback.
 */
async function createCloneWorkspace(
  params: { repoRoot: string; targetPath: string; workspaceName: string; description?: string },
  snapshot: WarmSnapshot,
  revision: string,
): Promise<{ snapshot: WarmSnapshot } | { reason: string }> {
  if (fs.existsSync(params.targetPath)) return { reason: `${params.targetPath} already exists` };
  const added = await runCommand(
    "jj",
    [
      "rudder-clone-add", params.targetPath, "--name", params.workspaceName, "-r", revision,
      "--snapshot", snapshot.dir, "-m", params.description ?? "",
    ],
    { cwd: params.repoRoot, allowFailure: true },
  );
  if (added.code === 0) return { snapshot };
  // rudder-clone-add publishes the workspace only once its files are in
  // place, so a failure leaves at most the partly cloned directory.
  await fsp.rm(params.targetPath, { recursive: true, force: true });
  return { reason: added.stderr.trim() || "rudder-jj rudder-clone-add failed" };
}

/** Removes `<workspace>.fork/` (OverlayFS leaves a mode-000 `work/work`). */
export async function removeForkState(workspace: string): Promise<void> {
  const state = forkStateDir(workspace);
  await fsp.chmod(path.join(workspace, ".jj", "working_copy", "work", "work"), 0o700).catch(() => undefined);
  await fsp.chmod(path.join(state, "work", "work"), 0o700).catch(() => undefined);
  await fsp.rm(state, { recursive: true, force: true });
}

// Detached builder entry: node warm-snapshot.js <repoRoot> <commit>
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const [repoRoot, commit] = process.argv.slice(2);
  if (repoRoot && commit) await buildWarmSnapshot(repoRoot, commit);
}
