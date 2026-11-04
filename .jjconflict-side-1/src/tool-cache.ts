// One Rudder tool cache per machine. Tool-native caches (sccache, pnpm, npm,
// uv, pip, cargo, Turborepo, Nx) point at shared directories so parallel
// agents reuse each other's compile and install work.
//
// Trusted-writer rule: the warm-snapshot builder writes shared results; agents
// read. sccache supports this directly (SCCACHE_LOCAL_RW_MODE=READ_ONLY). The
// other tools have no read-only mode, so agents share them read-write. That is
// acceptable because these stores are content-addressed or keyed by exact
// package version/hash and the tools write entries atomically (temp file plus
// rename), so a concurrent writer can add entries but cannot change what an
// existing key resolves to. All agents on a machine run as the same user.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export type ToolCacheRole = "agent" | "builder";

export interface ToolCacheOptions {
  root?: string;
  env?: NodeJS.ProcessEnv;
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
    out.SCCACHE_LOCAL_RW_MODE = role === "builder" ? "READ_WRITE" : "READ_ONLY";
  }
  if (has("pnpm")) use("npm_config_store_dir", "pnpm");
  if (has("npm")) use("npm_config_cache", "npm");
  if (has("uv")) use("UV_CACHE_DIR", "uv");
  if (has("pip") || has("pip3")) use("PIP_CACHE_DIR", "pip");
  if (has("cargo") && use("CARGO_HOME", "cargo")) prepareCargoHome(out.CARGO_HOME, env);
  use("TURBO_CACHE_DIR", "turbo");
  use("NX_CACHE_DIRECTORY", "nx");
  return out;
}

interface CacheFile { file: string; size: number; atime: number }

function listFiles(dir: string, out: CacheFile[] = []): CacheFile[] {
  let entries: fs.Dirent[];
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return out; }
  for (const entry of entries) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) listFiles(file, out);
    else if (entry.isFile()) {
      try { const st = fs.statSync(file); out.push({ file, size: st.size, atime: Math.max(st.atimeMs, st.mtimeMs) }); } catch { /* raced */ }
    }
  }
  return out;
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
