// One Rudder tool cache per machine. Tool-native caches (sccache, pnpm, npm,
// uv, pip, cargo, Turborepo, Nx) point at shared directories so parallel
// agents reuse each other's compile and install work.
//
// Agents and the warm-snapshot builder share every cache read-write. These
// stores are content-addressed or keyed by exact package version/hash and the
// tools write entries atomically (temp file plus rename), so a concurrent
// writer can add entries but cannot change what an existing key resolves to.
// All agents on a machine run as the same user, so a read-only mode would not
// protect the directory anyway. sccache in particular runs one server per
// machine and the first process to start it picks its mode; a read-only agent
// server also never sees entries another server adds after it started. So
// sccache is read-write for everyone and parallel agents reuse each other's
// compiled crates.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export type ToolCacheRole = "agent" | "builder";

export interface ToolCacheOptions {
  root?: string;
  env?: NodeJS.ProcessEnv;
  /**
   * The directory the agent (or warm builder) builds in. sccache keys hold
   * absolute paths (the target dir, --out-dir), so it is stripped from them
   * (SCCACHE_BASEDIRS) and crates compiled in one workspace hit in another.
   */
  workspace?: string;
  /** Override tool detection (tests). */
  hasTool?: (name: string) => boolean;
}

/** Per-directory size limits in bytes, enforced by pruneToolCache. */
export const TOOL_CACHE_LIMITS: Record<string, number> = {
  sccache: 20 * 1024 ** 3,
  pnpm: 10 * 1024 ** 3,
  npm: 5 * 1024 ** 3,
  uv: 10 * 1024 ** 3,
  pip: 5 * 1024 ** 3,
  cargo: 5 * 1024 ** 3,
  turbo: 5 * 1024 ** 3,
  nx: 5 * 1024 ** 3,
};

export function toolCacheRoot(env: NodeJS.ProcessEnv = process.env): string {
  if (env.RUDDER_TOOL_CACHE_DIR) return env.RUDDER_TOOL_CACHE_DIR;
  // Never derive this from anything that differs between processes or
  // versions (such as a Cloud supervisor's RUDDER_CACHE_DIR): Cargo records
  // CARGO_HOME's paths in its build fingerprints, so a moved CARGO_HOME
  // rebuilds every dependency in every warm snapshot and checkout.
  const base = env.XDG_CACHE_HOME || path.join(env.HOME || os.homedir(), ".cache");
  return path.join(base, "rudder", "tool-cache");
}

export function commandOnPath(name: string, env: NodeJS.ProcessEnv = process.env): boolean {
  for (const dir of (env.PATH ?? "").split(path.delimiter)) {
    if (!dir) continue;
    try { fs.accessSync(path.join(dir, name), fs.constants.X_OK); return true; } catch { /* next */ }
  }
  return false;
}

// Shared CARGO_HOME holds the registry/git download caches. The user's cargo
// config and credentials are linked in so behavior matches their own home.
function prepareCargoHome(dir: string, env: NodeJS.ProcessEnv): void {
  const userHome = env.CARGO_HOME || path.join(env.HOME || os.homedir(), ".cargo");
  for (const name of ["config.toml", "config", "credentials.toml", "credentials"]) {
    const source = path.join(userHome, name);
    const target = path.join(dir, name);
    if (!fs.existsSync(source) || fs.existsSync(target)) continue;
    try { fs.symlinkSync(source, target); } catch { /* raced with another agent */ }
  }
}

/**
 * Env vars for an agent process or the trusted builder. Tools that are not
 * installed are skipped. Turborepo and Nx are per-project dev dependencies, so
 * their cache dirs are always set (harmless when unused).
 */
export function toolCacheEnv(role: ToolCacheRole, options: ToolCacheOptions = {}): Record<string, string> {
  const env = options.env ?? process.env;
  const root = options.root ?? toolCacheRoot(env);
  const has = options.hasTool ?? ((name: string) => commandOnPath(name, env));
  const out: Record<string, string> = {};
  const use = (key: string, name: string): boolean => {
    const dir = path.join(root, name);
    try { fs.mkdirSync(dir, { recursive: true }); } catch { return false; }
    out[key] = dir;
    return true;
  };
  if (has("sccache") && use("SCCACHE_DIR", "sccache")) {
    out.RUSTC_WRAPPER = "sccache";
    out.SCCACHE_CACHE_SIZE = `${Math.floor(TOOL_CACHE_LIMITS.sccache / 1024 ** 3)}G`;
    out.SCCACHE_LOCAL_RW_MODE = "READ_WRITE";
    if (options.workspace) out.SCCACHE_BASEDIRS = options.workspace;
  }
  if (has("pnpm")) use("npm_config_store_dir", "pnpm");
  if (has("npm")) use("npm_config_cache", "npm");
  if (has("uv")) use("UV_CACHE_DIR", "uv");
  if (has("pip") || has("pip3")) use("PIP_CACHE_DIR", "pip");
  if (has("cargo") && use("CARGO_HOME", "cargo")) prepareCargoHome(out.CARGO_HOME, env);
  use("TURBO_CACHE_DIR", "turbo");
  use("NX_CACHE_DIRECTORY", "nx");
  if (role === "agent" && env.RUDDER_BURST_SOCKET) out.RUDDER_BURST_SOCKET = env.RUDDER_BURST_SOCKET;
  if (env.RUDDER_BUCK2_CACHE_ADDRESS) out.RUDDER_BUCK2_CACHE_ADDRESS = env.RUDDER_BUCK2_CACHE_ADDRESS;
  const remoteBase = (env.RUDDER_REMOTE_CACHE_URL ?? "").trim().replace(/\/+$/, "");
  const remoteToken = (env.RUDDER_REMOTE_CACHE_TOKEN ?? "").trim();
  if (env.RUDDER_REMOTE_CACHE !== "0" && remoteBase && remoteToken) {
    if (out.SCCACHE_DIR) {
      out.SCCACHE_MULTILEVEL_CHAIN = "disk,webdav";
      out.SCCACHE_WEBDAV_ENDPOINT = `${remoteBase}/sccache`;
      out.SCCACHE_WEBDAV_TOKEN = remoteToken;
      out.SCCACHE_WEBDAV_RW_MODE = "READ_WRITE";
      out.SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY = "ignore";
    }
    out.TURBO_API = `${remoteBase}/turbo`;
    out.TURBO_TOKEN = remoteToken;
    out.TURBO_TEAM = "rudder";
  }
  if (role === "agent") Object.assign(out, agentPagerEnv(root, env, has("jj")));
  return out;
}

const AGENT_JJ_CONFIG = '[ui]\npaginate = "never"\n';

// Agents can run commands in a terminal of their own (Antigravity always
// does). There jj and git page long output through `less`, which waits for a
// key nobody presses and hangs the agent. jj ignores PAGER, so its setting is
// a config file listed after the user's own config files, which JJ_CONFIG
// keeps loading.
function agentPagerEnv(root: string, env: NodeJS.ProcessEnv, hasJj: boolean): Record<string, string> {
  const out: Record<string, string> = { PAGER: "cat", GIT_PAGER: "cat" };
  if (!hasJj) return out;
  const file = path.join(root, "jj-agent.toml");
  if (!fs.existsSync(file) || fs.readFileSync(file, "utf8") !== AGENT_JJ_CONFIG) {
    const temp = `${file}.${process.pid}.tmp`;
    fs.writeFileSync(temp, AGENT_JJ_CONFIG);
    fs.renameSync(temp, file);
  }
  const user = env.JJ_CONFIG
    ? env.JJ_CONFIG.split(path.delimiter)
    : execFileSync("jj", ["config", "path", "--user"], { env, encoding: "utf8" }).split("\n");
  out.JJ_CONFIG = [...user.filter((entry) => entry && entry !== file), file].join(path.delimiter);
  return out;
}

interface CacheFile { file: string; size: number; atime: number; mtime: number }

export interface ToolCacheEntryUsage {
  name: string;
  bytes: number;
  limitBytes: number;
  fileCount: number;
  recentFiles24h: number;
  lastWriteAt: number | null;
}

export interface ToolCacheUsage {
  root: string;
  totalBytes: number;
  entries: ToolCacheEntryUsage[];
}

function listFiles(dir: string, out: CacheFile[] = []): CacheFile[] {
  let entries: fs.Dirent[];
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return out; }
  for (const entry of entries) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) listFiles(file, out);
    else if (entry.isFile()) {
      try {
        const st = fs.lstatSync(file);
        if (st.isFile()) out.push({ file, size: st.size, atime: Math.max(st.atimeMs, st.mtimeMs), mtime: st.mtimeMs });
      } catch { /* raced */ }
    }
  }
  return out;
}

/**
 * Measures disk usage for every directory in `limits` under `root`.
 * Symlinks are never followed.
 */
export function toolCacheUsage(
  root: string = toolCacheRoot(),
  limits: Record<string, number> = TOOL_CACHE_LIMITS,
): ToolCacheUsage {
  const cutoff = Date.now() - 24 * 3600_000;
  let totalBytes = 0;
  const entries: ToolCacheEntryUsage[] = [];
  for (const [name, limitBytes] of Object.entries(limits)) {
    const files = listFiles(path.join(root, name));
    let bytes = 0;
    let recentFiles24h = 0;
    let maxMtime = 0;
    for (const f of files) {
      bytes += f.size;
      if (f.mtime >= cutoff) recentFiles24h += 1;
      if (f.mtime > maxMtime) maxMtime = f.mtime;
    }
    totalBytes += bytes;
    entries.push({
      name,
      bytes,
      limitBytes,
      fileCount: files.length,
      recentFiles24h,
      lastWriteAt: maxMtime > 0 ? maxMtime : null,
    });
  }
  return { root, totalBytes, entries };
}

/**
 * Deletes least recently used files until each cache dir is within its limit.
 * Symlinks (linked cargo config) are never followed or removed. Run from the
 * trusted builder or an idle maintenance pass. Returns bytes freed.
 */
export function pruneToolCache(root: string = toolCacheRoot(), limits: Record<string, number> = TOOL_CACHE_LIMITS): number {
  let freed = 0;
  for (const [name, limit] of Object.entries(limits)) {
    const files = listFiles(path.join(root, name)).sort((a, b) => a.atime - b.atime);
    let total = files.reduce((sum, f) => sum + f.size, 0);
    for (const f of files) {
      if (total <= limit) break;
      try { fs.unlinkSync(f.file); total -= f.size; freed += f.size; } catch { /* in use or gone */ }
    }
  }
  return freed;
}
