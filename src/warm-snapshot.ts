/**
 * Warm snapshots, overlay forks (Linux) and clone forks (macOS).
 *
 * A warm snapshot holds one base commit's files plus the dependencies and
 * build output the trusted builder made for it (with the shared tool cache,
 * src/tool-cache.ts). It is never written again once ready: agents get a fork
 * of it instead of a full checkout.
 *
 * Snapshots follow trunk. One detached builder per project (`startWarmBuild`
 * spawns this file; launches and integrations start it) watches the base new
 * workers start from (`currentBaseCommit`) and builds a snapshot for it
 * whenever it moves, coalescing quick moves to the latest. A refresh starts
 * from the nearest ready snapshot, so install and build reuse its outputs:
 * - Linux: the new snapshot is a thin OverlayFS layer on top of the previous
 *   snapshot's layers. The builder mounts the stack at the fixed path
 *   `.warm/build`, applies the base change with one Git two-tree checkout and
 *   runs the warm commands there; their writes land in `.warm/<key>/`, and
 *   `.warm/<key>.lowerdir` lists the whole stack, top first. A refresh on a
 *   stack MAX_LAYERS deep copies the stack's merged view into a plain
 *   directory instead (compaction).
 * - macOS: the new snapshot is an APFS clone of the previous one.
 * A project's first snapshot is a jj workspace (`rudderwarm-<key>`). Branch
 * launches never build: they fork the nearest base snapshot and patch the
 * difference.
 *
 * On macOS a fork is an APFS copy-on-write clone of the snapshot (see
 * createCloneWorkspace). On Linux it is an overlay fork:
 *
 * A fork is a jj workspace on the `rudder-overlay` working copy, added by
 * `rudder-jj rudder-fork-add`: its directory on disk holds only `.jj`, the
 * agent's private OverlayFS layers live in `.jj/working_copy/{upper,work}`,
 * and the warm snapshot (its top layer, linked into `.warm/lower-links/<tree>`)
 * is the lower layer. Snapshot reads only the upper layer (no tree scan);
 * check-out repoints the lower (no copy). Only rudder-jj can load such a
 * workspace. Next to it, `<workspace>.fork/` holds:
 * - `lower`: the warm snapshot it was made from (for pruning);
 * - `alias`: the builder's path, where build output may point (see ENTER_SCRIPT);
 * - `bin/jj`: rudder-jj, first on PATH inside the fork;
 * - `enter`: runs a command in a private user+mount namespace with the overlay
 *   mounted at the workspace path and `.jj` bound back, as the real user.
 * The native PTY layer runs every agent command whose cwd has a sibling
 * `.fork/enter` through it.
 *
 * Pruning keeps the newest snapshot, every layer a fork or a kept snapshot
 * mounts, and deletes the rest.
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
/** Newest ready snapshots kept even when no fork or newer stack uses them. */
const KEEP_READY = 1;
/** A refresh on a stack this deep makes a flat copy instead of another layer. */
const MAX_LAYERS = 8;
/** The builder's lock and pid, one per project. */
const BUILDER_LOCK = "builder.pid";
const FOLLOW_POLL_MS = 1_000;
/** The builder exits after this long without the base moving. */
const FOLLOW_IDLE_MS = 30 * 60_000;
/** After a refresh, a small base move waits this long (or as long as the refresh took). */
const REFRESH_GAP_MS = 60_000;

export type WarmSnapshot = {
  key: string;
  commit: string;
  /** The snapshot's directory: the whole tree, or the top layer of a stack. */
  dir: string;
  builtAt: string;
  /** OverlayFS layers, top first (`dir` first), when the snapshot is stacked. */
  layers?: string[];
};

/** `.rudder-workspaces/<repo>/` (where agent workspaces live). */
function workspacesDir(repoRoot: string): string {
  return path.dirname(workspacePath(repoRoot, "warm-probe"));
}

export function warmDir(repoRoot: string): string {
  return path.join(workspacesDir(repoRoot), ".warm");
}

/** Where the Linux builder mounts the snapshot it builds, and forks alias themselves. */
function buildPath(repoRoot: string): string {
  return path.join(warmDir(repoRoot), "build");
}

/**
 * Tree links for forks made by this version. Forks from before stacked
 * snapshots use `.warm/lowers` and an enter script that mounts one layer, so
 * they never resolve a tree to a stacked snapshot's top layer.
 */
function forkLowersRoot(repoRoot: string): string {
  return path.join(warmDir(repoRoot), "lower-links");
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

/** A snapshot directory's layers, top first: the list in `<dir>.lowerdir`, else the directory itself. */
function layersOf(dir: string): string[] {
  try {
    const layers = fs.readFileSync(`${dir}.lowerdir`, "utf8").split(/[:\n]/).filter(Boolean);
    if (layers.length > 0) return layers;
  } catch {
    /* a flat snapshot */
  }
  return [dir];
}

function realPath(file: string): string {
  try {
    return fs.realpathSync(file);
  } catch {
    return path.resolve(file);
  }
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
      if (layersOf(snap.dir).every((layer) => fs.existsSync(layer))) out.push(snap);
    } catch {
      /* torn marker: ignore */
    }
  }
  return out.sort((a, b) => b.builtAt.localeCompare(a.builtAt));
}

/** Commands from rudder.toml `[warm] commands = [...]`, else detected from lockfiles. */
export function warmCommands(dir: string): string[] {
  return commandsFor(
    (file) => fs.existsSync(path.join(dir, file)),
    (file) => {
      try {
        return fs.readFileSync(path.join(dir, file), "utf8");
      } catch {
        return null;
      }
    },
  );
}

/** warmCommands for a commit's tree, read from the Git store (a stacked build has no full checkout). */
function warmCommandsAt(gitDir: string, commit: string): string[] {
  const listed = spawnSync("git", ["--git-dir", gitDir, "ls-tree", "--name-only", "-z", commit], { encoding: "utf8" });
  const names = new Set(listed.status === 0 ? listed.stdout.split("\0") : []);
  return commandsFor(
    (file) => names.has(file),
    (file) => {
      if (!names.has(file)) return null;
      const shown = spawnSync("git", ["--git-dir", gitDir, "show", `${commit}:${file}`], { encoding: "utf8" });
      return shown.status === 0 ? shown.stdout : null;
    },
  );
}

function commandsFor(has: (file: string) => boolean, read: (file: string) => string | null): string[] {
  const configured = readConfiguredCommands(read("rudder.toml"));
  if (configured) return configured;
  const commands: string[] = [];
  if (has("pnpm-lock.yaml")) commands.push("pnpm install --frozen-lockfile");
  else if (has("yarn.lock")) commands.push("yarn install --frozen-lockfile");
  else if (has("package-lock.json")) commands.push("npm ci");
  else if (has("package.json")) commands.push("npm install");
  if (has("package.json")) {
    try {
      const pkg = JSON.parse(read("package.json") ?? "") as { scripts?: Record<string, string> };
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

function readConfiguredCommands(text: string | null): string[] | null {
  if (text === null) return null;
  const section = text.split(/^\s*\[warm\]\s*$/m)[1]?.split(/^\s*\[/m)[0];
  const match = section?.match(/^\s*commands\s*=\s*\[([\s\S]*?)\]/m);
  if (!match) return null;
  return [...match[1]!.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((m) => JSON.parse(`"${m[1]}"`) as string);
}

/** Git's empty tree. As the attribute source it turns off in-tree .gitattributes. */
const EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// Runs in the directory to update. $1 = git dir, $2 = throwaway index file,
// $3 = commit whose files are there, $4 = commit to move to. jj commits are
// Git commits in the colocated repo, so Git's two-tree checkout (what
// `git checkout` does to switch branches) writes exactly the paths that
// differ, in one process: removals (whiteouts in an overlay's upper layer),
// executable bits, symlinks and file/directory swaps. It uses its own index
// file; the repo's index and refs are never written. The overrides keep
// line-ending, filter and attribute settings, hooks, sparse checkout and
// fsmonitor from changing what is written: the bytes must be the commit's, as
// jj writes them. Git without --attr-source (before 2.42) fails here.
const CHECKOUT_DIFF = `
G="$1"; I="$2"; S="$3"; B="$4"
g() {
  GIT_INDEX_FILE="$I" GIT_ATTR_NOSYSTEM=1 git --git-dir="$G" --work-tree=. --attr-source=${EMPTY_TREE} \\
    -c core.autocrlf=false -c core.symlinks=true -c core.attributesFile=/dev/null -c core.hooksPath=/dev/null \\
    -c core.fsmonitor=false -c core.splitIndex=false "$@"
}
g read-tree "$S"
g read-tree --reset -u --no-sparse-checkout "$S" "$B"
`;

/** A fork moves its files, then its change, onto the base (the launch makes a full checkout if this fails). */
const MOVE_TO_BASE = `${CHECKOUT_DIFF}jj rebase -r @ -d "$B"\n`;

// Runs "$@" as the real user in a private user+mount namespace where the
// fixed build path $B shows the snapshot being built:
// - overlay: the layers $L (lowerdir list) under the new layer $UP (work dir $WK);
// - stack: the layers $L, read-only (needs two or more);
// - bind: the plain directory $L.
// Every step mounts afresh; nothing outside the namespace sees $B change.
const BUILD_ENTER = `
B="$1"; MODE="$2"; L="$3"; UP="$4"; WK="$5"; U="$6"; G="$7"; shift 7
case "$MODE" in
  overlay) mount -t overlay overlay -o "lowerdir=$L,upperdir=$UP,workdir=$WK,userxattr" "$B" ;;
  stack) mount -t overlay overlay -o "lowerdir=$L,userxattr" "$B" ;;
  *) mount --bind "$L" "$B" ;;
esac
cd "$B"
exec unshare -U --map-user="$U" --map-group="$G" -- "$@"
`;

type BuildView = { mount: string; mode: "overlay" | "stack" | "bind"; lowers: string; upper?: string; work?: string };

let namespacesChecked: boolean | undefined;
/** Rootless user+mount namespaces (what forks and stacked builds need). */
function namespacesWork(): boolean {
  namespacesChecked ??= process.platform === "linux"
    && spawnSync("unshare", ["-rm", "--propagation", "private", "true"], { stdio: "ignore" }).status === 0;
  return namespacesChecked;
}

/** One build step: `command` in `view` (or in `cwd` without one), output to the log. */
function runStep(log: number, env: NodeJS.ProcessEnv, view: BuildView | null, cwd: string, command: string, args: string[]): boolean {
  const [program, argv] = view
    ? ["unshare", [
        "-rm", "--propagation", "private", "sh", "-ec", BUILD_ENTER, "sh",
        view.mount, view.mode, view.lowers, view.upper ?? "", view.work ?? "",
        String(process.getuid?.() ?? 0), String(process.getgid?.() ?? 0), command, ...args,
      ]]
    : [command, args];
  return spawnSync(program, argv, { cwd, env, stdio: ["ignore", log, log] }).status === 0;
}

/** Removes a directory tree, including read-only directories a build left behind. */
async function removeTree(dir: string): Promise<void> {
  try {
    await fsp.rm(dir, { recursive: true, force: true });
  } catch {
    spawnSync("chmod", ["-R", "u+rwX", dir], { stdio: "ignore" });
    await fsp.rm(dir, { recursive: true, force: true }).catch(() => undefined);
  }
}

/** Is `commit` conflicted? Its Git tree then holds jj's conflict layout, not files. */
async function conflicted(repoRoot: string, revset: string): Promise<boolean> {
  const result = await runCommand("jj", ["log", "--no-graph", "--ignore-working-copy", "-r", revset, "-T", 'if(conflict, "conflict\\n")'], {
    cwd: repoRoot,
    allowFailure: true,
  });
  return result.code !== 0 || result.stdout.trim() !== "";
}

type BuildContext = {
  repoRoot: string;
  key: string;
  commit: string;
  dir: string;
  log: number;
  say: (line: string) => void;
};

/**
 * Builds the warm snapshot for `commit` (blocking), on top of the nearest
 * ready snapshot when there is one. Returns it (or the ready snapshot with the
 * same files), or null when another builder holds the key or the build failed
 * (a failed key is not retried; a new base commit gets a new key).
 */
export async function buildWarmSnapshot(
  repoRoot: string,
  commit: string,
  options: { maxLayers?: number } = {},
): Promise<WarmSnapshot | null> {
  const key = commit.slice(0, 12);
  const root = warmDir(repoRoot);
  await fsp.mkdir(root, { recursive: true });
  const marker = (suffix: string) => path.join(root, `${key}.${suffix}`);
  if (fs.existsSync(marker("ready")) || fs.existsSync(marker("failed"))) {
    return readySnapshots(repoRoot).find((snap) => snap.key === key) ?? null;
  }
  if (fs.existsSync(marker("building")) && !lockAlive(marker("building"))) await fsp.rm(marker("building"), { force: true });
  let lock: fs.promises.FileHandle;
  try {
    lock = await fsp.open(marker("building"), "wx");
  } catch {
    return null;
  }
  const dir = path.join(root, key);
  const log = fs.openSync(marker("log"), "a");
  const ctx: BuildContext = {
    repoRoot, key, commit, dir, log,
    say: (line) => fs.writeSync(log, `[rudder] warm snapshot ${key}: ${line}\n`),
  };
  try {
    await lock.write(String(process.pid));
    if (await conflicted(repoRoot, commit)) throw new Error(`${key} has conflicts`);
    const gitDir = await gitDirOf(repoRoot);
    const snaps = readySnapshots(repoRoot);
    const nearest = snaps.length > 0 ? await nearestSnapshot(gitDir, snaps, commit) : null;
    if (nearest?.files === 0) {
      ctx.say(`same files as ${nearest.snap.key}`);
      return nearest.snap;
    }
    const started = Date.now();
    let snap: WarmSnapshot | null = null;
    if (nearest && gitDir && namespacesWork()) {
      snap = await buildOnLayers(ctx, gitDir, nearest.snap, options.maxLayers ?? MAX_LAYERS);
    } else if (nearest && process.platform === "darwin") {
      snap = await buildFromClone(ctx, nearest.snap);
    }
    snap ??= await buildFromScratch(ctx, nearest?.snap ?? null);
    ctx.say(`ready in ${((Date.now() - started) / 1000).toFixed(1)} s (${snap.layers?.length ?? 1} layer(s))`);
    await fsp.writeFile(marker("ready"), JSON.stringify(snap));
    await pruneWarmSnapshots(repoRoot);
    return snap;
  } catch (error) {
    ctx.say(`failed: ${error instanceof Error ? error.message : String(error)}`);
    await removeSnapshotFiles(repoRoot, key, dir);
    await fsp.writeFile(marker("failed"), String(error)).catch(() => undefined);
    return null;
  } finally {
    fs.closeSync(log);
    await lock.close();
    await fsp.rm(marker("building"), { force: true });
  }
}

/**
 * Linux refresh on `previous`: a new OverlayFS layer over its layers, or, when
 * its stack is `maxLayers` deep, a plain copy of its merged view (compaction).
 * The base change and the warm commands run in that view at the fixed build
 * path, so they see the previous outputs at the paths they were made at.
 * Returns null (after cleaning up) when the view cannot be set up; the caller
 * then builds from scratch. A failing warm command throws.
 */
async function buildOnLayers(ctx: BuildContext, gitDir: string, previous: WarmSnapshot, maxLayers: number): Promise<WarmSnapshot | null> {
  const { repoRoot, key, commit, dir, log, say } = ctx;
  const layers = layersOf(previous.dir);
  const mount = buildPath(repoRoot);
  const work = `${dir}.work`;
  const env = { ...process.env, ...toolCacheEnv("builder", { workspace: mount }) };
  await fsp.mkdir(mount, { recursive: true });
  await removeSnapshotFiles(repoRoot, key, dir);
  await fsp.mkdir(dir);
  const flat = layers.length >= maxLayers;
  let view: BuildView;
  if (flat) {
    const started = Date.now();
    const source: BuildView = { mount, mode: layers.length > 1 ? "stack" : "bind", lowers: layers.join(":") };
    // The first snapshot's .jj belongs to its jj workspace. Forks need only an
    // empty .jj to mount their own over.
    const copy = 'find . -mindepth 1 -maxdepth 1 ! -name .jj -exec cp -a -t "$1" {} +; mkdir "$1/.jj"';
    const copied = runStep(log, env, source, dir, "sh", ["-ec", copy, "sh", dir]);
    if (!copied) {
      say(`could not copy ${previous.key}'s ${layers.length} layers; building from scratch`);
      await removeSnapshotFiles(repoRoot, key, dir);
      return null;
    }
    say(`compacted ${previous.key}'s ${layers.length} layers into a plain copy in ${((Date.now() - started) / 1000).toFixed(1)} s`);
    view = { mount, mode: "bind", lowers: dir };
  } else {
    await fsp.mkdir(work);
    view = { mount, mode: "overlay", lowers: layers.join(":"), upper: dir, work };
  }
  const index = `${dir}.index`;
  say(`files of ${commit.slice(0, 12)} on top of ${previous.key}`);
  const applied = runStep(log, env, view, dir, "sh", ["-ec", CHECKOUT_DIFF, "sh", gitDir, index, previous.commit, commit]);
  await fsp.rm(index, { force: true });
  if (!applied) {
    say(`could not apply the base change on ${previous.key}; building from scratch`);
    await removeSnapshotFiles(repoRoot, key, dir);
    return null;
  }
  for (const command of warmCommandsAt(gitDir, commit)) {
    say(command);
    if (!runStep(log, env, view, dir, "sh", ["-c", command])) throw new Error(`${command} failed`);
  }
  await removeWorkDir(work);
  if (flat) return { key, commit, dir, builtAt: new Date().toISOString() };
  const stack = [dir, ...layers];
  await fsp.writeFile(`${dir}.lowerdir`, stack.join(":"));
  return { key, commit, dir, builtAt: new Date().toISOString(), layers: stack };
}

/**
 * macOS refresh: the new snapshot is an APFS clone of `previous` checked out at
 * the commit (`rudder-jj rudder-clone-add` writes only the files that differ),
 * so the warm commands start from its outputs. Null when the clone fails.
 */
async function buildFromClone(ctx: BuildContext, previous: WarmSnapshot): Promise<WarmSnapshot | null> {
  const { repoRoot, key, commit, dir, log, say } = ctx;
  const name = `${WARM_WORKSPACE_PREFIX}${key}`;
  await runCommand("jj", ["workspace", "forget", name], { cwd: repoRoot, allowFailure: true });
  await removeTree(dir);
  const added = await runCommand(
    "jj",
    ["rudder-clone-add", dir, "--name", name, "-r", commit, "--snapshot", previous.dir, "-m", ""],
    { cwd: repoRoot, allowFailure: true },
  );
  if (added.code !== 0) {
    say(`clone of ${previous.key} failed (${added.stderr.trim()}); building from scratch`);
    await removeTree(dir);
    return null;
  }
  const env = { ...process.env, ...toolCacheEnv("builder", { workspace: dir }) };
  for (const command of warmCommands(dir)) {
    say(command);
    if (!runStep(log, env, null, dir, "sh", ["-c", command])) throw new Error(`${command} failed`);
  }
  await runCommand("jj", ["status"], { cwd: dir, allowFailure: true });
  return { key, commit, dir, builtAt: new Date().toISOString() };
}

/**
 * A full checkout of the commit as a jj workspace, seeded with a flat
 * previous snapshot's untracked outputs. On Linux the warm commands run at the
 * fixed build path, as for layers built later on top of it.
 */
async function buildFromScratch(ctx: BuildContext, previous: WarmSnapshot | null): Promise<WarmSnapshot> {
  const { repoRoot, key, commit, dir, log, say } = ctx;
  const name = `${WARM_WORKSPACE_PREFIX}${key}`;
  await runCommand("jj", ["workspace", "forget", name], { cwd: repoRoot, allowFailure: true });
  await removeSnapshotFiles(repoRoot, key, dir);
  const added = await runCommand("jj", ["workspace", "add", dir, "--name", name, "-r", commit], { cwd: repoRoot, allowFailure: true });
  if (added.code !== 0) throw new Error(added.stderr.trim() || "jj workspace add failed");
  if (previous && layersOf(previous.dir).length === 1) await seedFromPrevious(previous.dir, dir);
  let view: BuildView | null = null;
  if (namespacesWork()) {
    view = { mount: buildPath(repoRoot), mode: "bind", lowers: dir };
    await fsp.mkdir(view.mount, { recursive: true });
  }
  const env = { ...process.env, ...toolCacheEnv("builder", { workspace: view?.mount ?? dir }) };
  for (const command of warmCommands(dir)) {
    say(command);
    if (!runStep(log, env, view, dir, "sh", ["-c", command])) throw new Error(`${command} failed`);
  }
  // Record file stats for the whole tree once; macOS clones reuse this state.
  if (process.platform === "darwin") await runCommand("jj", ["status"], { cwd: dir, allowFailure: true });
  return { key, commit, dir, builtAt: new Date().toISOString() };
}

/** OverlayFS leaves a mode-000 `work/work` in its work directory. */
async function removeWorkDir(work: string): Promise<void> {
  await fsp.chmod(path.join(work, "work"), 0o700).catch(() => undefined);
  await removeTree(work);
}

/** Removes a snapshot's directory and side files (not its markers or log). */
async function removeSnapshotFiles(repoRoot: string, key: string, dir: string): Promise<void> {
  if (fs.existsSync(path.join(dir, ".jj", "repo"))) {
    await runCommand("jj", ["workspace", "forget", `${WARM_WORKSPACE_PREFIX}${key}`], { cwd: repoRoot, allowFailure: true });
  }
  await removeWorkDir(`${dir}.work`);
  await removeTree(dir);
  for (const side of [`${dir}.lowerdir`, `${dir}.index`]) await fsp.rm(side, { force: true });
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
 * Makes sure the project's builder is following the base: starts the detached
 * builder unless one is running or there is nothing to install or build.
 * Never waits. Returns why it did not start one when the disk lacks room.
 */
export function startWarmBuild(repoRoot: string): string | null {
  if (warmCommands(repoRoot).length === 0) return null;
  const root = warmDir(repoRoot);
  fs.mkdirSync(root, { recursive: true });
  if (lockAlive(path.join(root, BUILDER_LOCK))) return null;
  const skipped = warmBuildSkipReason(root);
  if (skipped) return skipped;
  const log = fs.openSync(path.join(root, "builder.log"), "w");
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), repoRoot], {
    detached: true,
    stdio: ["ignore", log, log],
  });
  child.unref();
  fs.closeSync(log);
  return null;
}

/** Takes the project's builder lock (pid written atomically). False when a live builder holds it. */
function takeBuilderLock(lock: string): boolean {
  const tmp = `${lock}.${process.pid}`;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    fs.writeFileSync(tmp, String(process.pid));
    try {
      fs.linkSync(tmp, lock);
      return true;
    } catch {
      if (lockAlive(lock)) return false;
      fs.rmSync(lock, { force: true });
    } finally {
      fs.rmSync(tmp, { force: true });
    }
  }
  return false;
}

function releaseBuilderLock(lock: string): void {
  try {
    if (fs.readFileSync(lock, "utf8").trim() === String(process.pid)) fs.rmSync(lock, { force: true });
  } catch {
    /* already gone */
  }
}

/** Names of the repo's operation heads: they change with every jj operation. */
function opHeads(repoRoot: string): string {
  try {
    return fs.readdirSync(path.join(repoRoot, ".jj", "repo", "op_heads", "heads")).sort().join(",");
  } catch {
    return String(Math.floor(Date.now() / 10_000));
  }
}

/**
 * The builder's loop: whenever the operation log moves, read the base; once
 * the same new base is seen on two polls in a row, build its snapshot. Moves
 * during a build coalesce: the next check sees only the latest base. A small
 * move right after a refresh waits REFRESH_GAP_MS (or as long as that refresh
 * took); a move far from every snapshot builds at once. Exits after
 * FOLLOW_IDLE_MS without a move, or when the repo is gone.
 */
async function followBase(repoRoot: string): Promise<void> {
  const root = warmDir(repoRoot);
  await fsp.mkdir(root, { recursive: true });
  const lock = path.join(root, BUILDER_LOCK);
  if (!takeBuilderLock(lock)) return;
  let heads = "";
  let pending = null as { base: string; urgent: boolean } | null;
  let done: string | null = null;
  let nextRefresh = 0;
  let active = Date.now();
  try {
    while (Date.now() - active < FOLLOW_IDLE_MS && fs.existsSync(path.join(repoRoot, ".jj"))) {
      const now = opHeads(repoRoot);
      if (now !== heads || pending) {
        // The base only moves with the operation log.
        const base = now !== heads ? await currentBaseCommit(repoRoot) : pending!.base;
        heads = now;
        if (!base || base === done) {
          pending = null;
        } else if (base !== pending?.base) {
          active = Date.now();
          const files = await baseDistance(repoRoot, base);
          if (files === 0) done = base;
          pending = files === 0 ? null : { base, urgent: files > NEAR_SNAPSHOT_FILES };
        } else if (pending.urgent || Date.now() >= nextRefresh) {
          pending = null;
          done = base;
          if (warmCommands(repoRoot).length > 0 && !warmBuildSkipReason(root)) {
            const started = Date.now();
            process.stdout.write(`[rudder] warm builder: base moved to ${base.slice(0, 12)}\n`);
            await buildWarmSnapshot(repoRoot, base);
            nextRefresh = Date.now() + Math.max(REFRESH_GAP_MS, Date.now() - started);
          }
          active = Date.now();
          continue;
        }
      }
      await new Promise((resolve) => setTimeout(resolve, FOLLOW_POLL_MS));
    }
  } finally {
    releaseBuilderLock(lock);
  }
}

/** Files between `base` and the nearest ready snapshot or build attempt (0 when it has one). */
async function baseDistance(repoRoot: string, base: string): Promise<number> {
  const key = base.slice(0, 12);
  if (["ready", "failed"].some((s) => fs.existsSync(path.join(warmDir(repoRoot), `${key}.${s}`)))) return 0;
  const snaps = readySnapshots(repoRoot);
  if (snaps.length === 0) return Infinity;
  return (await nearestSnapshot(await gitDirOf(repoRoot), snaps, base)).files;
}

/**
 * Deletes ready snapshots beyond the newest `keep`, unless a fork or a kept
 * snapshot mounts them: a stacked snapshot keeps every layer under it.
 */
export async function pruneWarmSnapshots(repoRoot: string, keep = KEEP_READY): Promise<void> {
  const snaps = readySnapshots(repoRoot);
  const kept = snaps.slice(0, keep);
  for (const snap of snaps.slice(keep)) {
    // Asked again for each one: a launch may claim a snapshot meanwhile.
    if (layersInUse(repoRoot, kept).has(realPath(snap.dir))) continue;
    await fsp.rm(path.join(warmDir(repoRoot), `${snap.key}.ready`), { force: true });
    for (const links of [forkLowersRoot(repoRoot), path.join(warmDir(repoRoot), "lowers")]) await removeLowerLinks(links, snap.dir);
    await removeSnapshotFiles(repoRoot, snap.key, snap.dir);
    await fsp.rm(path.join(warmDir(repoRoot), `${snap.key}.log`), { force: true });
  }
  await pruneOldMarkers(repoRoot);
}

/**
 * Every layer (real path) a kept snapshot or a fork mounts. A fork counts both
 * the snapshot it was made from (`.fork/lower`: its agent's namespace may still
 * hold that mount) and what its working copy points at now.
 */
function layersInUse(repoRoot: string, kept: WarmSnapshot[]): Set<string> {
  const used = new Set<string>();
  const add = (dir: string) => {
    for (const layer of layersOf(realPath(dir))) used.add(realPath(layer));
  };
  for (const snap of kept) add(snap.dir);
  const parent = workspacesDir(repoRoot);
  let names: string[] = [];
  try {
    names = fs.readdirSync(parent).filter((n) => n.endsWith(".fork"));
  } catch {
    /* no workspaces */
  }
  for (const name of names) {
    for (const file of [path.join(parent, name, "lower"), path.join(parent, name.slice(0, -".fork".length), ".jj", "working_copy", "lower")]) {
      try {
        const dir = fs.readFileSync(file, "utf8").trim();
        if (dir) add(dir);
      } catch {
        /* not written (yet) */
      }
    }
  }
  return used;
}

/** Drops logs and failure markers of builds that left no snapshot, a day on. */
async function pruneOldMarkers(repoRoot: string): Promise<void> {
  const root = warmDir(repoRoot);
  const names = await fsp.readdir(root).catch(() => [] as string[]);
  const cutoff = Date.now() - 24 * 3600_000;
  for (const name of names) {
    const match = /^(.+)\.(log|failed)$/.exec(name);
    if (!match || match[1] === "builder" || names.includes(`${match[1]}.ready`)) continue;
    const stat = await fsp.stat(path.join(root, name)).catch(() => null);
    if (stat && stat.mtimeMs < cutoff) await fsp.rm(path.join(root, name), { force: true });
  }
}

/** Drops tree links into a snapshot that is being deleted. */
async function removeLowerLinks(root: string, target: string): Promise<void> {
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
L="$(readlink -f "$(cat "$W/lower")")"
# A stacked warm snapshot lists all its layers next to its top layer.
if [ -f "$L.lowerdir" ]; then L="$(cat "$L.lowerdir")"; fi
mount -t overlay overlay -o "lowerdir=$L,upperdir=$W/upper,workdir=$W/work,userxattr" "$M"
[ -d "$M/.jj" ] || mkdir "$M/.jj"
mount --bind "$F/jjmnt" "$M/.jj"
# Build output can name the path the snapshot was built at; show this fork there.
A="$(cat "$F/alias" 2>/dev/null || true)"
if [ -n "$A" ] && [ -d "$A" ]; then mount --rbind "$M" "$A"; fi
# Login shells rebuild PATH; make every jj on it this fork-aware rudder-jj.
J="$(readlink -f "$F/bin/jj")"
for d in $(printf %s "$PATH" | tr : " ") /usr/local/bin /usr/bin "$HOME/.local/bin"; do
  [ -f "$d/jj" ] && [ "$(readlink -f "$d/jj")" != "$J" ] && mount --bind "$J" "$d/jj" 2>/dev/null || true
done
cd "$M"
PATH="$F/bin:$PATH"; export PATH
exec unshare -U --map-user="$U" --map-group="$G" -- "$@"' sh "$F" "$M" "$U" "$G" "$@"
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
 * Waits up to `timeoutMs` for the builder to make a snapshot of `base`, or one
 * within NEAR_SNAPSHOT_FILES of it (the base may move on meanwhile). Returns
 * null when its build fails or no live builder is left to make it.
 */
async function waitForNearSnapshot(repoRoot: string, gitDir: string | null, base: string, timeoutMs: number): Promise<WarmSnapshot | null> {
  const root = warmDir(repoRoot);
  const key = base.slice(0, 12);
  const started = Date.now();
  let seen = "";
  while (Date.now() - started < timeoutMs) {
    const snaps = readySnapshots(repoRoot);
    const own = snaps.find((snap) => snap.commit === base);
    if (own) return own;
    if (fs.existsSync(path.join(root, `${key}.failed`))) return null;
    const keys = snaps.map((snap) => snap.key).join();
    if (snaps.length > 0 && keys !== seen) {
      seen = keys;
      gitDir ??= await gitDirOf(repoRoot);
      const nearest = await nearestSnapshot(gitDir, snaps, base);
      if (nearest.files <= NEAR_SNAPSHOT_FILES) return nearest.snap;
    }
    // A detached builder takes its lock right after it starts.
    if (Date.now() - started > 5_000 && !lockAlive(path.join(root, BUILDER_LOCK)) && !lockAlive(path.join(root, `${key}.building`))) return null;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  return null;
}

/** Does a live process hold this pid lock? An empty lock counts as live for 10 s. */
function lockAlive(lock: string): boolean {
  let text: string;
  try {
    text = fs.readFileSync(lock, "utf8").trim();
  } catch {
    return false;
  }
  const pid = Number(text);
  if (!pid) {
    try {
      return Date.now() - fs.statSync(lock).mtimeMs < 10_000;
    } catch {
      return false;
    }
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * Creates `targetPath` as a fork of the warm snapshot nearest its base: an
 * overlay fork on Linux, an APFS clone on macOS. A launch never builds; it
 * makes sure the project's builder runs. When every ready snapshot is far from
 * the base (or there is none yet), a launch from the base waits a while for
 * the builder; a branch launch forks the nearest base snapshot and patches the
 * difference. Returns a reason (after cleaning up) when there is no snapshot
 * or the fork cannot be made here; the caller then makes a full jj checkout.
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
  // Without these no fork can be made, so do not build or wait for a snapshot.
  const rudderJj = rudderJjPath();
  if (!rudderJj) return { reason: "rudder-jj is not available" };
  if (process.platform === "linux" && !namespacesWork()) return { reason: "rootless user namespaces unavailable" };
  const base = params.atChangeId
    ? await commitOf(params.repoRoot, params.atChangeId)
    : await currentBaseCommit(params.repoRoot);
  if (params.atChangeId && !base) return { reason: `change ${params.atChangeId} not found` };
  // Projects with nothing to install or build keep the plain checkout.
  const warmable = warmCommands(params.repoRoot).length > 0;
  const skipped = warmable ? startWarmBuild(params.repoRoot) : null;
  const snaps = readySnapshots(params.repoRoot);
  const canWait = Boolean(base && warmable && !skipped && !params.atChangeId);
  let snapshot = snaps.find((snap) => snap.commit === base);
  let gitDir: string | null = null;
  if (base && !snapshot && snaps.length > 0) {
    gitDir = await gitDirOf(params.repoRoot);
    const nearest = await nearestSnapshot(gitDir, snaps, base);
    snapshot = nearest.snap;
    // Far from every ready snapshot: the base's own build (on top of the
    // nearest one) is usually quicker than patching thousands of files.
    if (nearest.files > NEAR_SNAPSHOT_FILES && canWait) {
      snapshot = (await waitForNearSnapshot(params.repoRoot, gitDir, base, WAIT_FOR_BUILD_MS)) ?? snapshot;
    }
  } else if (base && !snapshot && canWait) {
    // A project's first launch: its first snapshot is often ready soon.
    snapshot = (await waitForNearSnapshot(params.repoRoot, gitDir, base, WAIT_FOR_BUILD_MS)) ?? undefined;
  }
  snapshot ??= snaps[0];
  if (!snapshot) return { reason: skipped ?? (warmable ? "no warm snapshot yet (building)" : "nothing to install or build") };
  if (process.platform === "darwin") return await createCloneWorkspace(params, snapshot, base ?? snapshot.commit);
  const move = base && base !== snapshot.commit ? base : null;
  if (move) {
    gitDir ??= await gitDirOf(params.repoRoot);
    if (!gitDir) return { reason: "no Git store behind the jj repo" };
    if (await conflicted(params.repoRoot, `${snapshot.commit}|${move}`)) return { reason: `${move.slice(0, 12)} has conflicts` };
  }
  // Claim the snapshot before adding the fork: a refresh that finishes
  // meanwhile prunes snapshots no `.fork/lower` names. One that was already
  // pruning may have taken it; then make a full checkout.
  const state = forkStateDir(params.targetPath);
  await fsp.mkdir(state, { recursive: true });
  await fsp.writeFile(path.join(state, "lower"), snapshot.dir);
  if (!layersOf(snapshot.dir).every((layer) => fs.existsSync(layer))) {
    await removeForkState(params.targetPath);
    return { reason: `warm snapshot ${snapshot.key} was pruned` };
  }
  const added = await runCommand(
    "jj",
    [
      "rudder-fork-add", params.targetPath, "--name", params.workspaceName, "-r", snapshot.commit,
      "--lower", snapshot.dir, "--lowers-root", forkLowersRoot(params.repoRoot),
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
    await fsp.writeFile(path.join(state, "alias"), buildPath(params.repoRoot));
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

// Detached builder entry: node warm-snapshot.js <repoRoot> follows the base;
// with a commit it builds that one snapshot.
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const [repoRoot, commit] = process.argv.slice(2);
  if (repoRoot && commit) await buildWarmSnapshot(repoRoot, commit);
  else if (repoRoot) await followBase(repoRoot);
}
