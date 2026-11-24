// Machine resource, VFS/warm-snapshot, tool-cache, and sccache statistics
// collector for the home supervisor (home.mjs).
//
// Exports:
// - projectWarmBadge(repoDir): fast, synchronous, zero-subprocess, non-volatile
//   summary for publishState() so /api/app/state ETags stay stable.
// - agentWorkspaceMode(workspacePath): fast, synchronous, non-volatile workspace
//   mode check for agentSummary().
// - createMachineStatsCollector(options): full machine snapshot collector and
//   cache pruner for GET /api/app/machine and POST /api/app/machine/prune.

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { pidAlive } from "./agent-status.mjs";
import { readHostStats } from "./telemetry.mjs";

const MAX_LAYERS = 8;
const MAX_WALK_DEPTH = 32;
const MAX_WALK_ENTRIES = 250_000;
const MAX_UPPER_FILES = 10_000;
const LOG_TAIL_BYTES = 8192;
const FAILED_MAX_BYTES = 4096;
const DAY_MS = 24 * 3600_000;
const EMPTY_LOCK_GRACE_MS = 10_000;
const SCCACHE_TIMEOUT_MS = 2_000;
const JJ_TIMEOUT_MS = 3_000;

const ACTIVE_AGENT_STATUSES = new Set([
  "working",
  "waiting",
  "starting",
  "running",
  "steering",
  "verifying",
  "created",
]);

/** Default per-tool cache limits in bytes, matching src/tool-cache.ts. */
export const DEFAULT_TOOL_CACHE_LIMITS = {
  sccache: 20 * 1024 ** 3,
  pnpm: 10 * 1024 ** 3,
  npm: 5 * 1024 ** 3,
  uv: 10 * 1024 ** 3,
  pip: 5 * 1024 ** 3,
  cargo: 5 * 1024 ** 3,
  turbo: 5 * 1024 ** 3,
  nx: 5 * 1024 ** 3,
};

// ---------------------------------------------------------------------------
// Workspace and warm-snapshot path helpers (matching src/state.ts & util.ts).
// ---------------------------------------------------------------------------

function slugify(inputValue, fallback = "repo") {
  const slug = String(inputValue)
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  return slug || fallback;
}

function shortHash(value) {
  return createHash("sha1").update(value).digest("hex").slice(0, 10);
}

function stripTrailingSlash(value) {
  if (value === "/") return "/";
  return value.replace(/\/+$/, "") || "/";
}

function repoWorkspacesDir(repoDir) {
  const clean = stripTrailingSlash(repoDir);
  const root = path.join(clean, ".rudder-workspaces");
  const exactSlug = `${slugify(path.basename(clean), "repo")}-${shortHash(clean)}`;
  const exact = path.join(root, exactSlug);
  if (fs.existsSync(exact)) return exact;

  const resolved = path.resolve(clean);
  if (resolved !== clean) {
    const resolvedSlug = `${slugify(path.basename(resolved), "repo")}-${shortHash(resolved)}`;
    const resolvedExact = path.join(root, resolvedSlug);
    if (fs.existsSync(resolvedExact)) return resolvedExact;
  }

  let entries;
  try {
    entries = fs.readdirSync(root, { withFileTypes: true });
  } catch {
    return exact;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const candidate = path.join(root, entry.name);
    if (fs.existsSync(path.join(candidate, ".warm"))) return candidate;
  }
  return exact;
}

function realPath(file) {
  try {
    return fs.realpathSync(file);
  } catch {
    return path.resolve(file);
  }
}

function layersOf(dir) {
  try {
    const layers = fs
      .readFileSync(`${dir}.lowerdir`, "utf8")
      .split(/[:\n]/)
      .map((item) => item.trim())
      .filter(Boolean);
    if (layers.length > 0) return layers;
  } catch {
    // Flat snapshot without a .lowerdir stack file.
  }
  return [dir];
}

function lockAlive(lockFile, nowMs = Date.now()) {
  let text;
  try {
    text = fs.readFileSync(lockFile, "utf8").trim();
  } catch {
    return false;
  }
  const pid = Number(text);
  if (!text) {
    try {
      return nowMs - fs.statSync(lockFile).mtimeMs < EMPTY_LOCK_GRACE_MS;
    } catch {
      return false;
    }
  }
  if (!Number.isInteger(pid) || pid <= 0) return false;
  return pidAlive(pid);
}

/**
 * Synchronously reads the 12-char HEAD commit from a Git/colocated-jj repo
 * without spawning a subprocess. Returns null when unavailable.
 */
function readGitHeadShort(repoDir) {
  const dotGit = path.join(repoDir, ".git");
  let gitDir = dotGit;
  try {
    const stat = fs.statSync(dotGit);
    if (stat.isFile()) {
      const pointer = fs.readFileSync(dotGit, "utf8").trim();
      const match = /^gitdir:\s*(.+)$/i.exec(pointer);
      if (!match) return null;
      gitDir = path.resolve(repoDir, match[1].trim());
    } else if (!stat.isDirectory()) {
      return null;
    }
  } catch {
    return null;
  }

  let head;
  try {
    head = fs.readFileSync(path.join(gitDir, "HEAD"), "utf8").trim();
  } catch {
    return null;
  }
  if (/^[0-9a-f]{12,40}$/i.test(head)) return head.slice(0, 12).toLowerCase();
  if (!head.startsWith("ref:")) return null;

  const ref = head.slice(4).trim();
  if (!ref) return null;
  try {
    const target = fs.readFileSync(path.join(gitDir, ref), "utf8").trim();
    if (/^[0-9a-f]{12,40}$/i.test(target)) return target.slice(0, 12).toLowerCase();
  } catch {
    // Ref may live in packed-refs.
  }
  try {
    const packed = fs.readFileSync(path.join(gitDir, "packed-refs"), "utf8");
    for (const line of packed.split("\n")) {
      if (!line || line.startsWith("#") || line.startsWith("^")) continue;
      const parts = line.trim().split(/\s+/);
      if (parts.length >= 2 && parts[1] === ref && /^[0-9a-f]{12,40}$/i.test(parts[0])) {
        return parts[0].slice(0, 12).toLowerCase();
      }
    }
  } catch {
    // No packed-refs file.
  }
  return null;
}

function readWarmState(warmDir, nowMs = Date.now()) {
  let names;
  try {
    names = fs.readdirSync(warmDir);
  } catch {
    return { ready: [], building: [], failed: [] };
  }

  const ready = [];
  const building = [];
  const failed = [];

  for (const name of names) {
    if (name.endsWith(".ready")) {
      try {
        const snap = JSON.parse(fs.readFileSync(path.join(warmDir, name), "utf8"));
        if (!snap || typeof snap !== "object" || typeof snap.key !== "string" || typeof snap.dir !== "string") {
          continue;
        }
        const stack = layersOf(snap.dir);
        if (!stack.every((layer) => fs.existsSync(layer))) continue;
        const layers = Array.isArray(snap.layers) && snap.layers.length > 0 ? snap.layers.length : stack.length;
        ready.push({
          key: snap.key,
          commit: typeof snap.commit === "string" ? snap.commit : null,
          dir: snap.dir,
          builtAt: typeof snap.builtAt === "string" ? snap.builtAt : "",
          layers,
        });
      } catch {
        // Ignore torn marker.
      }
    } else if (name.endsWith(".building")) {
      const key = name.slice(0, -".building".length);
      if (key && lockAlive(path.join(warmDir, name), nowMs)) {
        building.push(key);
      }
    } else if (name.endsWith(".failed")) {
      const key = name.slice(0, -".failed".length);
      if (!key || key === "builder") continue;
      let mtimeMs = 0;
      try {
        mtimeMs = fs.statSync(path.join(warmDir, name)).mtimeMs;
      } catch {
        continue;
      }
      failed.push({ key, file: path.join(warmDir, name), mtimeMs });
    }
  }

  ready.sort((a, b) => b.builtAt.localeCompare(a.builtAt) || b.key.localeCompare(a.key));
  building.sort();
  failed.sort((a, b) => b.mtimeMs - a.mtimeMs || b.key.localeCompare(a.key));
  return { ready, building, failed };
}

function deriveWarmStatus({ ready, building, failed }, baseCommit) {
  if (building.length > 0) return "building";
  if (ready.length === 0) {
    return failed.length > 0 ? "failed" : "none";
  }
  if (!baseCommit) return "ready";
  const latest = ready[0];
  const matchesBase =
    latest.key === baseCommit ||
    (typeof latest.commit === "string" && latest.commit.toLowerCase().startsWith(baseCommit.toLowerCase()));
  if (matchesBase) return "ready";
  if (failed.some((entry) => entry.key === baseCommit)) return "failed";
  return "stale";
}

function countProjectForks(workspacesDir) {
  let entries;
  try {
    entries = fs.readdirSync(workspacesDir, { withFileTypes: true });
  } catch {
    return 0;
  }
  let count = 0;
  for (const entry of entries) {
    if (!entry.isDirectory() || !entry.name.endsWith(".fork")) continue;
    if (fs.existsSync(path.join(workspacesDir, entry.name, "enter"))) {
      count += 1;
    }
  }
  return count;
}

// ---------------------------------------------------------------------------
// Export 1: projectWarmBadge(repoDir)
// ---------------------------------------------------------------------------

/**
 * Fast, synchronous, zero-subprocess, non-volatile warm-snapshot summary for a
 * project checkout. Suitable for publishState() without churning ETags.
 */
export function projectWarmBadge(repoDir) {
  if (typeof repoDir !== "string" || !repoDir.trim()) {
    throw new TypeError("projectWarmBadge: repoDir must be a non-empty string");
  }
  const wsDir = repoWorkspacesDir(repoDir);
  const warmDir = path.join(wsDir, ".warm");
  const state = readWarmState(warmDir);
  const baseCommit = readGitHeadShort(repoDir);
  const status = deriveWarmStatus(state, baseCommit);
  const latest = state.ready[0] ?? null;
  const forkCount = countProjectForks(wsDir);

  let key = null;
  if (latest) {
    key = latest.key;
  } else if (state.building.length > 0) {
    key = state.building[0];
  } else if (state.failed.length > 0) {
    key = state.failed[0].key;
  }

  return {
    status,
    key,
    commit: latest?.commit ?? null,
    layers: latest?.layers ?? 0,
    maxLayers: MAX_LAYERS,
    builtAt: latest?.builtAt || null,
    forkCount,
  };
}

// ---------------------------------------------------------------------------
// Export 2: agentWorkspaceMode(workspacePath)
// ---------------------------------------------------------------------------

/**
 * Fast, synchronous, non-volatile workspace mode inspection for an agent row.
 */
export function agentWorkspaceMode(workspacePath) {
  if (typeof workspacePath !== "string" || !workspacePath.trim()) {
    return null;
  }
  const ws = stripTrailingSlash(workspacePath);
  const forkDir = `${ws}.fork`;
  if (fs.existsSync(path.join(forkDir, "enter"))) {
    let lowerRaw = "";
    try {
      lowerRaw = fs.readFileSync(path.join(forkDir, "lower"), "utf8").trim();
    } catch {
      lowerRaw = "";
    }
    if (!lowerRaw) {
      return { kind: "overlay-fork", snapshotKey: null, layers: 0 };
    }
    const resolvedLower = realPath(lowerRaw);
    const snapshotKey = path.basename(resolvedLower) || null;
    const layers = layersOf(resolvedLower).length;
    return { kind: "overlay-fork", snapshotKey, layers };
  }
  if (fs.existsSync(ws)) {
    return { kind: "plain" };
  }
  return null;
}

// ---------------------------------------------------------------------------
// Safe bounded filesystem walks, log/error parsing, and /proc inspection.
// ---------------------------------------------------------------------------

function walkDirBytes(rootDir, { skipTopNames = null, maxDepth = MAX_WALK_DEPTH, maxEntries = MAX_WALK_ENTRIES } = {}) {
  let bytes = 0;
  let seen = 0;
  const walk = (dir, depth) => {
    if (depth > maxDepth || seen >= maxEntries) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (seen >= maxEntries) return;
      if (depth === 0 && skipTopNames?.has(entry.name)) continue;
      if (entry.isSymbolicLink()) continue;
      seen += 1;
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full, depth + 1);
      } else if (entry.isFile()) {
        try {
          bytes += fs.lstatSync(full).size;
        } catch {
          // Raced or inaccessible.
        }
      }
    }
  };
  walk(rootDir, 0);
  return bytes;
}

function walkToolCacheDir(dir, nowMs) {
  let bytes = 0;
  let fileCount = 0;
  let recentFiles24h = 0;
  let lastWriteAt = null;
  let seen = 0;
  const cutoff = nowMs - DAY_MS;

  const walk = (current, depth) => {
    if (depth > MAX_WALK_DEPTH || seen >= MAX_WALK_ENTRIES) return;
    let entries;
    try {
      entries = fs.readdirSync(current, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (seen >= MAX_WALK_ENTRIES) return;
      if (entry.isSymbolicLink()) continue;
      seen += 1;
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) {
        walk(full, depth + 1);
      } else if (entry.isFile()) {
        try {
          const st = fs.lstatSync(full);
          bytes += st.size;
          fileCount += 1;
          if (st.mtimeMs >= cutoff) recentFiles24h += 1;
          const mtime = Math.floor(st.mtimeMs);
          if (lastWriteAt === null || mtime > lastWriteAt) lastWriteAt = mtime;
        } catch {
          // Raced or inaccessible.
        }
      }
    }
  };
  walk(dir, 0);
  return { bytes, fileCount, recentFiles24h, lastWriteAt };
}

function countUpperFiles(upperDir, cap = MAX_UPPER_FILES) {
  let count = 0;
  const walk = (dir, depth) => {
    if (depth > MAX_WALK_DEPTH || count >= cap) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (count >= cap) return;
      if (entry.isDirectory()) {
        walk(path.join(dir, entry.name), depth + 1);
      } else {
        count += 1;
      }
    }
  };
  walk(upperDir, 0);
  return count;
}

function readTail(filePath, maxBytes) {
  let fd;
  try {
    fd = fs.openSync(filePath, "r");
    const stat = fs.fstatSync(fd);
    const length = Math.min(stat.size, maxBytes);
    if (length <= 0) return "";
    const buf = Buffer.alloc(length);
    const read = fs.readSync(fd, buf, 0, length, stat.size - length);
    return buf.subarray(0, read).toString("utf8");
  } catch {
    return "";
  } finally {
    if (fd !== undefined) {
      try { fs.closeSync(fd); } catch { /* ignore */ }
    }
  }
}

function readBuildSeconds(warmDir, key) {
  if (!key) return null;
  const tail = readTail(path.join(warmDir, `${key}.log`), LOG_TAIL_BYTES);
  if (!tail) return null;
  let lastMatch = null;
  for (const match of tail.matchAll(/ready in (\d+(?:\.\d+)?)\s*s\b/g)) {
    lastMatch = match[1];
  }
  if (lastMatch === null) return null;
  const value = Number(lastMatch);
  return Number.isFinite(value) ? value : null;
}

function readLastError(warmDir, failedEntry) {
  if (!failedEntry) return null;
  try {
    const raw = fs.readFileSync(failedEntry.file, "utf8").slice(0, FAILED_MAX_BYTES).trim();
    const firstLine = raw.split(/\r?\n/).map((line) => line.trim()).find(Boolean);
    if (firstLine) return firstLine.slice(0, 400);
  } catch {
    // Fall through to check the build log if the failed marker vanished or is unreadable.
  }
  const tail = readTail(path.join(warmDir, `${failedEntry.key}.log`), LOG_TAIL_BYTES);
  const lines = tail.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  for (let i = lines.length - 1; i >= 0; i -= 1) {
    const failedMatch = /failed:\s*(.+)$/.exec(lines[i]);
    if (failedMatch) return failedMatch[1].trim().slice(0, 400);
  }
  return lines.length > 0 ? lines[lines.length - 1].slice(0, 400) : null;
}

function parseProcMounts(procRoot) {
  let text = "";
  try {
    text = fs.readFileSync(path.join(procRoot, "mounts"), "utf8");
  } catch {
    return { fuseMounts: 0, overlayMounts: 0 };
  }
  let fuseMounts = 0;
  let overlayMounts = 0;
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const parts = trimmed.split(/\s+/);
    if (parts.length < 3) continue;
    const mountpoint = parts[1];
    const fstype = parts[2];
    if (/^fuse($|\.|blk)/i.test(fstype) || fstype.toLowerCase().includes("rudder")) {
      fuseMounts += 1;
    }
    if (fstype === "overlay" && mountpoint.includes("/.rudder-workspaces/")) {
      overlayMounts += 1;
    }
  }
  return { fuseMounts, overlayMounts };
}

/**
 * Finds currently listening sccache sockets from /proc/net/unix and
 * /proc/net/tcp without spawning or starting any daemon.
 */
function findListeningSccacheSockets(procRoot) {
  const abstract = new Set();
  let unixText = "";
  try {
    unixText = fs.readFileSync(path.join(procRoot, "net", "unix"), "utf8");
  } catch {
    unixText = "";
  }
  for (const line of unixText.split("\n")) {
    const cols = line.trim().split(/\s+/);
    // Format: Num RefCount Protocol Flags Type St Inode Path
    if (cols.length < 8) continue;
    const flags = Number.parseInt(cols[3], 16);
    const st = cols[5];
    const sockPath = cols[7];
    if (!Number.isFinite(flags) || (flags & 0x10000) === 0 || st !== "01") continue;
    if (/^@rudder-sccache-\S+$/.test(sockPath)) {
      abstract.add(sockPath);
    }
  }

  let hasDefaultTcp = false;
  let tcpText = "";
  try {
    tcpText = fs.readFileSync(path.join(procRoot, "net", "tcp"), "utf8");
  } catch {
    tcpText = "";
  }
  for (const line of tcpText.split("\n")) {
    const cols = line.trim().split(/\s+/);
    // Format: sl local_address rem_address st ...
    // 0100007F:1082 = 127.0.0.1:4226, 0A = TCP_LISTEN
    if (cols.length >= 4 && cols[1].toUpperCase() === "0100007F:1082" && cols[3].toUpperCase() === "0A") {
      hasDefaultTcp = true;
      break;
    }
  }

  const sockets = [];
  for (const sock of [...abstract].sort()) {
    sockets.push({ id: sock, kind: "abstract", name: sock.slice(1) });
  }
  if (hasDefaultTcp) {
    sockets.push({ id: "tcp:127.0.0.1:4226", kind: "tcp" });
  }
  return sockets;
}

// ---------------------------------------------------------------------------
// Command execution helper (bounded timeout, non-throwing).
// ---------------------------------------------------------------------------

function defaultRunCommand(command, args, { cwd, env, timeoutMs = 2_000 } = {}) {
  return new Promise((resolve) => {
    let stdout = "";
    let stderr = "";
    let child;
    let settled = false;
    const finish = (result) => {
      if (settled) return;
      settled = true;
      resolve(result);
    };
    try {
      child = spawn(command, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    } catch (error) {
      finish({ code: -1, stdout: "", stderr: error?.message || String(error) });
      return;
    }
    const timer = setTimeout(() => {
      try { child.kill("SIGKILL"); } catch { /* ignore */ }
      child.stdout?.destroy();
      child.stderr?.destroy();
      finish({ code: -1, stdout, stderr: stderr || "command timed out" });
    }, timeoutMs);
    timer.unref?.();
    child.stdout?.on("data", (chunk) => { stdout += chunk; });
    child.stderr?.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => {
      clearTimeout(timer);
      finish({ code: -1, stdout, stderr: error?.message || String(error) });
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      finish({ code: code ?? -1, stdout, stderr });
    });
  });
}

// ---------------------------------------------------------------------------
// Tool cache root and local LRU pruner (matching src/tool-cache.ts).
// ---------------------------------------------------------------------------

function resolveToolCacheRoot(home, env) {
  if (env.RUDDER_TOOL_CACHE_DIR) return env.RUDDER_TOOL_CACHE_DIR;
  const useXdg = !home || home === env.HOME;
  const base = (useXdg && env.XDG_CACHE_HOME) || path.join(home || env.HOME || os.homedir(), ".cache");
  return path.join(base, "rudder", "tool-cache");
}

function listPruneFiles(dir, out = []) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    if (entry.isSymbolicLink()) continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      listPruneFiles(full, out);
    } else if (entry.isFile()) {
      try {
        const st = fs.lstatSync(full);
        out.push({ file: full, size: st.size, atime: Math.max(st.atimeMs, st.mtimeMs) });
      } catch {
        // Raced.
      }
    }
  }
  return out;
}

function pruneToolCacheInline(root, limits) {
  let freed = 0;
  for (const [name, limit] of Object.entries(limits)) {
    const files = listPruneFiles(path.join(root, name)).sort((a, b) => a.atime - b.atime);
    let total = files.reduce((sum, item) => sum + item.size, 0);
    for (const item of files) {
      if (total <= limit) break;
      try {
        fs.unlinkSync(item.file);
        total -= item.size;
        freed += item.size;
      } catch {
        // In use or already deleted.
      }
    }
  }
  return freed;
}

// ---------------------------------------------------------------------------
// sccache JSON parsing and persistent high-water-mark accumulation.
// ---------------------------------------------------------------------------

function num(value) {
  const n = Number(value);
  return Number.isFinite(n) && n >= 0 ? n : 0;
}

function sumCounts(value) {
  if (typeof value === "number") return num(value);
  if (!value || typeof value !== "object") return 0;
  const counts = value.counts && typeof value.counts === "object" ? value.counts : value;
  let total = 0;
  for (const v of Object.values(counts)) {
    if (typeof v === "number" && Number.isFinite(v) && v >= 0) total += v;
  }
  return total;
}

function durationMs(value) {
  if (!value || typeof value !== "object") return 0;
  return num(value.secs) * 1000 + num(value.nanos) / 1e6;
}

function round1(value) {
  return Math.round(value * 10) / 10;
}

function parseSccacheSnapshot(payload) {
  if (!payload || typeof payload !== "object" || !payload.stats || typeof payload.stats !== "object") {
    return null;
  }
  const s = payload.stats;
  const compileRequests = num(s.compile_requests);
  const cacheHits = sumCounts(s.cache_hits);
  const cacheMisses = sumCounts(s.cache_misses);
  const nonCacheable = num(s.non_cacheable_compilations);
  const errors = sumCounts(s.cache_errors) + num(s.cache_read_errors) + num(s.cache_write_errors);
  const compilations = typeof s.compilations === "number" ? num(s.compilations) : cacheMisses;
  const readHitDurationMs = durationMs(s.cache_read_hit_duration);
  const compilerDurationMs = durationMs(s.compiler_write_duration);

  const levels = {};
  let multilevel = false;
  if (Array.isArray(s.multi_level) && s.multi_level.length > 0) {
    multilevel = true;
    for (const level of s.multi_level) {
      if (!level || typeof level !== "object" || typeof level.name !== "string") continue;
      const backfills =
        typeof level.backfills === "number"
          ? num(level.backfills)
          : num(level.backfills_to) + num(level.backfills_from);
      levels[level.name] = {
        name: level.name,
        hits: num(level.hits),
        misses: num(level.misses),
        writes: num(level.writes),
        backfills,
      };
    }
  }

  return {
    version: typeof payload.version === "string" && payload.version.trim() ? payload.version.trim() : null,
    cacheLocation: typeof payload.cache_location === "string" && payload.cache_location.trim()
      ? payload.cache_location.trim()
      : null,
    mode: multilevel ? "multilevel" : "local-disk",
    counters: {
      compileRequests,
      cacheHits,
      cacheMisses,
      nonCacheable,
      errors,
      compilations,
      readHitDurationMs,
      compilerDurationMs,
      levels,
    },
  };
}

function emptySccacheState() {
  return {
    version: 1,
    mode: "local-disk",
    cacheLocation: null,
    totals: {
      compileRequests: 0,
      cacheHits: 0,
      cacheMisses: 0,
      nonCacheable: 0,
      errors: 0,
      compilations: 0,
      readHitDurationMs: 0,
      compilerDurationMs: 0,
      levels: {},
    },
    sockets: {},
  };
}

function loadSccacheState(filePath) {
  let raw;
  try {
    raw = fs.readFileSync(filePath, "utf8");
  } catch (error) {
    if (error?.code === "ENOENT") return emptySccacheState();
    throw error;
  }
  const parsed = JSON.parse(raw);
  if (!parsed || typeof parsed !== "object" || !parsed.totals || typeof parsed.totals !== "object") {
    return emptySccacheState();
  }
  const base = emptySccacheState();
  base.mode = parsed.mode === "multilevel" ? "multilevel" : "local-disk";
  base.cacheLocation = typeof parsed.cacheLocation === "string" ? parsed.cacheLocation : null;
  for (const field of [
    "compileRequests",
    "cacheHits",
    "cacheMisses",
    "nonCacheable",
    "errors",
    "compilations",
    "readHitDurationMs",
    "compilerDurationMs",
  ]) {
    base.totals[field] = num(parsed.totals[field]);
  }
  if (parsed.totals.levels && typeof parsed.totals.levels === "object") {
    for (const [name, level] of Object.entries(parsed.totals.levels)) {
      if (!level || typeof level !== "object") continue;
      base.totals.levels[name] = {
        name: typeof level.name === "string" ? level.name : name,
        hits: num(level.hits),
        misses: num(level.misses),
        writes: num(level.writes),
        backfills: num(level.backfills),
      };
    }
  }
  if (parsed.sockets && typeof parsed.sockets === "object") {
    base.sockets = parsed.sockets;
  }
  return base;
}

function saveSccacheStateAtomic(filePath, state) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const tmp = `${filePath}.${process.pid}.${Date.now()}.${Math.random().toString(36).slice(2, 8)}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`, "utf8");
  fs.renameSync(tmp, filePath);
}

const SCALAR_SCCACHE_FIELDS = [
  "compileRequests",
  "cacheHits",
  "cacheMisses",
  "nonCacheable",
  "errors",
  "compilations",
  "readHitDurationMs",
  "compilerDurationMs",
];

function mergeSccacheDaemons(state, snapshotsBySocket) {
  let changed = false;

  // Drop exited sockets so a future daemon reusing the same socket name starts
  // cleanly from zero rather than comparing against an old daemon's high-water mark.
  for (const socketId of Object.keys(state.sockets)) {
    if (!snapshotsBySocket.has(socketId)) {
      delete state.sockets[socketId];
      changed = true;
    }
  }

  let activeMode = null;
  let activeLocation = null;

  for (const [socketId, snap] of snapshotsBySocket.entries()) {
    if (snap.mode === "multilevel") activeMode = "multilevel";
    else if (activeMode === null) activeMode = "local-disk";
    if (snap.cacheLocation && (snap.mode === "multilevel" || activeLocation === null)) {
      activeLocation = snap.cacheLocation;
    }

    const prev = state.sockets[socketId];
    const cur = snap.counters;
    const restarted =
      !prev ||
      cur.compileRequests < num(prev.compileRequests) ||
      cur.cacheHits < num(prev.cacheHits) ||
      cur.cacheMisses < num(prev.cacheMisses);

    for (const field of SCALAR_SCCACHE_FIELDS) {
      const delta = restarted ? cur[field] : Math.max(0, cur[field] - num(prev[field]));
      if (delta > 0) {
        state.totals[field] += delta;
        changed = true;
      }
    }

    const prevLevels = prev?.levels && typeof prev.levels === "object" ? prev.levels : {};
    for (const [name, curLevel] of Object.entries(cur.levels)) {
      const prevLevel = prevLevels[name];
      const levelRestarted =
        restarted ||
        !prevLevel ||
        curLevel.hits < num(prevLevel.hits) ||
        curLevel.misses < num(prevLevel.misses) ||
        curLevel.writes < num(prevLevel.writes) ||
        curLevel.backfills < num(prevLevel.backfills);

      const target = (state.totals.levels[name] ??= { name, hits: 0, misses: 0, writes: 0, backfills: 0 });
      for (const field of ["hits", "misses", "writes", "backfills"]) {
        const delta = levelRestarted ? curLevel[field] : Math.max(0, curLevel[field] - num(prevLevel[field]));
        if (delta > 0) {
          target[field] += delta;
          changed = true;
        }
      }
    }

    const prevJson = prev ? JSON.stringify(prev) : "";
    const curJson = JSON.stringify(cur);
    if (prevJson !== curJson) {
      state.sockets[socketId] = cur;
      changed = true;
    }
  }

  if (activeMode && state.mode !== activeMode) {
    state.mode = activeMode;
    changed = true;
  }
  if (activeLocation && state.cacheLocation !== activeLocation) {
    state.cacheLocation = activeLocation;
    changed = true;
  }

  return changed;
}

function defaultRemoteCache() {
  return {
    proxyActive: false,
    proxyUrl: null,
    upstreamConnected: null,
    sccache: { hits: 0, misses: 0, writes: 0, errors: 0, bytesRead: 0, bytesWritten: 0, lastActivityAt: null },
    turbo: { hits: 0, misses: 0, writes: 0, errors: 0, bytesRead: 0, bytesWritten: 0, lastActivityAt: null },
    recent: [],
  };
}

// ---------------------------------------------------------------------------
// Export 3: createMachineStatsCollector(options)
// ---------------------------------------------------------------------------

export function createMachineStatsCollector({
  home,
  procRoot = "/proc",
  mounts = ["/workspace", "/"],
  cacheTtlMs = 20_000,
  sccacheStatsFile,
  runCommand = defaultRunCommand,
  toolCacheLimits = DEFAULT_TOOL_CACHE_LIMITS,
  env = process.env,
  platform = process.platform,
  now = () => Date.now(),
} = {}) {
  const resolvedHome = home || env.HOME || os.homedir();
  const statsFile = sccacheStatsFile || path.join(resolvedHome, ".rudder", "sccache-stats.json");
  const limits = { ...toolCacheLimits };

  let sccacheInfoPromise = null;
  let cachedToolCache = null; // { at, root, totalBytes, entries }
  const cachedWarmBytes = new Map(); // warmDir -> { at, bytes }

  function bustDiskWalkCache() {
    cachedToolCache = null;
    cachedWarmBytes.clear();
  }

  function getSccacheInfo() {
    sccacheInfoPromise ??= (async () => {
      const result = await runCommand("sccache", ["--version"], { env, timeoutMs: SCCACHE_TIMEOUT_MS });
      if (result && result.code === 0) {
        const match = /(\d+\.\d+\.\d+[^\s]*)/.exec(String(result.stdout || ""));
        return { available: true, version: match ? match[1] : String(result.stdout || "").trim() || null };
      }
      return { available: false, version: null };
    })();
    return sccacheInfoPromise;
  }

  async function currentBaseCommitShort(repoDir) {
    if (!repoDir || !fs.existsSync(repoDir)) return null;
    const first = await runCommand(
      "jj",
      ["log", "--no-graph", "--ignore-working-copy", "-r", "@-", "-T", 'commit_id ++ "\\n"'],
      { cwd: repoDir, env, timeoutMs: JJ_TIMEOUT_MS },
    );
    if (!first || first.code !== 0) return null;
    const ids = String(first.stdout || "")
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean);
    if (ids.length === 1) return ids[0].slice(0, 12);
    if (ids.length < 2) return null;
    const merge = await runCommand(
      "jj",
      ["log", "--no-graph", "--ignore-working-copy", "-r", "@", "-T", 'commit_id ++ "\\n"'],
      { cwd: repoDir, env, timeoutMs: JJ_TIMEOUT_MS },
    );
    const id = String(merge?.stdout || "").trim();
    return merge?.code === 0 && id ? id.slice(0, 12) : null;
  }

  function readWarmBytesCached(warmDir, nowMs, refresh) {
    if (!refresh) {
      const hit = cachedWarmBytes.get(warmDir);
      if (hit && nowMs - hit.at < cacheTtlMs) {
        return hit.bytes;
      }
    }
    const bytes = fs.existsSync(warmDir)
      ? walkDirBytes(warmDir, { skipTopNames: new Set(["build"]) })
      : 0;
    cachedWarmBytes.set(warmDir, { at: nowMs, bytes });
    return bytes;
  }

  function collectToolCacheDisk(tcRoot, nowMs, refresh) {
    if (!refresh && cachedToolCache && cachedToolCache.root === tcRoot && nowMs - cachedToolCache.at < cacheTtlMs) {
      return { totalBytes: cachedToolCache.totalBytes, entries: cachedToolCache.entries };
    }
    const entries = [];
    let totalBytes = 0;
    for (const [name, limitBytes] of Object.entries(limits)) {
      const dir = path.join(tcRoot, name);
      const stats = fs.existsSync(dir)
        ? walkToolCacheDir(dir, nowMs)
        : { bytes: 0, fileCount: 0, recentFiles24h: 0, lastWriteAt: null };
      totalBytes += stats.bytes;
      entries.push({
        name,
        bytes: stats.bytes,
        limitBytes,
        fileCount: stats.fileCount,
        recentFiles24h: stats.recentFiles24h,
        lastWriteAt: stats.lastWriteAt,
      });
    }
    cachedToolCache = { at: nowMs, root: tcRoot, totalBytes, entries };
    return { totalBytes, entries };
  }

  function collectHost() {
    if (platform === "darwin" && !fs.existsSync(procRoot)) {
      const [load1, load5, load15] = os.loadavg();
      const disks = [];
      for (const mount of mounts) {
        try {
          const stat = fs.statfsSync(mount);
          disks.push({ mount, totalBytes: stat.blocks * stat.bsize, freeBytes: stat.bavail * stat.bsize });
        } catch {
          // Not mounted here.
        }
      }
      return {
        memTotalBytes: os.totalmem(),
        memAvailableBytes: os.freemem(),
        swapTotalBytes: null,
        swapFreeBytes: null,
        cpus: os.cpus().length,
        load1: Number.isFinite(load1) ? load1 : null,
        load5: Number.isFinite(load5) ? load5 : null,
        load15: Number.isFinite(load15) ? load15 : null,
        disks,
        pressure: { memorySome: null, memoryFull: null, ioSome: null, cpuSome: null },
      };
    }

    const raw = readHostStats(procRoot, mounts);
    // telemetry.mjs normalizes PSI to 0..1 ratios for OTel; convert back to
    // kernel percentages (0..100) for machine-panel.js thresholds and display.
    const psiPct = (resource, kind) => {
      const entry = raw.pressure.find((item) => item.resource === resource && item.kind === kind && item.window === "avg10");
      return entry ? Math.round(entry.value * 10_000) / 100 : null;
    };
    const finite = (value) => (typeof value === "number" && Number.isFinite(value) ? value : null);
    return {
      memTotalBytes: raw.memTotal,
      memAvailableBytes: raw.memAvailable,
      swapTotalBytes: raw.swapTotal,
      swapFreeBytes: raw.swapFree,
      cpus: raw.cpus,
      load1: finite(raw.load?.["1m"]),
      load5: finite(raw.load?.["5m"]),
      load15: finite(raw.load?.["15m"]),
      disks: raw.disks.map((disk) => ({
        mount: disk.mount,
        totalBytes: disk.total,
        freeBytes: disk.free,
      })),
      pressure: {
        memorySome: psiPct("memory", "some"),
        memoryFull: psiPct("memory", "full"),
        ioSome: psiPct("io", "some"),
        cpuSome: psiPct("cpu", "some"),
      },
    };
  }

  function collectProjectWorkspaces(wsDir, agents = []) {
    const byPath = new Map();
    const byId = new Map();
    for (const agent of Array.isArray(agents) ? agents : []) {
      if (!agent || typeof agent !== "object") continue;
      if (typeof agent.id === "string" && agent.id) byId.set(agent.id, agent);
      if (typeof agent.workspacePath === "string" && agent.workspacePath) {
        byPath.set(path.resolve(stripTrailingSlash(agent.workspacePath)), agent);
      }
    }

    let entries;
    try {
      entries = fs.readdirSync(wsDir, { withFileTypes: true });
    } catch {
      return { forks: [], plainWorkspaces: [] };
    }

    const forks = [];
    const plainWorkspaces = [];

    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      if (entry.name.startsWith(".") || entry.name.endsWith(".fork")) continue;
      const wsPath = path.join(wsDir, entry.name);
      const mode = agentWorkspaceMode(wsPath);
      if (mode?.kind === "overlay-fork") {
        const matched = byPath.get(path.resolve(wsPath)) ?? byId.get(entry.name) ?? null;
        const runId = matched?.id ?? entry.name;
        const active = matched ? ACTIVE_AGENT_STATUSES.has(String(matched.status || "")) : false;
        const upperFiles = countUpperFiles(path.join(wsPath, ".jj", "working_copy", "upper"));
        forks.push({
          runId,
          snapshotKey: mode.snapshotKey,
          layers: mode.layers,
          upperFiles,
          active,
        });
      } else {
        plainWorkspaces.push(entry.name);
      }
    }

    forks.sort((a, b) => Number(b.active) - Number(a.active) || a.runId.localeCompare(b.runId));
    plainWorkspaces.sort();
    return { forks, plainWorkspaces };
  }

  async function collectSccache() {
    const [info, listening] = await Promise.all([
      getSccacheInfo(),
      Promise.resolve(findListeningSccacheSockets(procRoot)),
    ]);

    const queried = await Promise.all(
      listening.map(async (socket) => {
        const queryEnv = { ...env };
        if (socket.kind === "abstract") {
          // sccache parses the literal "\x00" prefix for Linux abstract sockets.
          queryEnv.SCCACHE_SERVER_UDS = `\\x00${socket.name}`;
        } else {
          delete queryEnv.SCCACHE_SERVER_UDS;
          delete queryEnv.SCCACHE_SERVER_PORT;
        }
        const res = await runCommand("sccache", ["--show-stats", "--stats-format", "json"], {
          env: queryEnv,
          timeoutMs: SCCACHE_TIMEOUT_MS,
        });
        if (!res || res.code !== 0) return null;
        let parsed;
        try {
          parsed = JSON.parse(String(res.stdout || ""));
        } catch {
          return null;
        }
        const snap = parseSccacheSnapshot(parsed);
        return snap ? [socket.id, snap] : null;
      }),
    );

    const activeBySocket = new Map();
    let daemonVersion = null;
    for (const item of queried) {
      if (!item) continue;
      const [id, snap] = item;
      activeBySocket.set(id, snap);
      if (!daemonVersion && snap.version) daemonVersion = snap.version;
    }

    const persisted = loadSccacheState(statsFile);
    const changed = mergeSccacheDaemons(persisted, activeBySocket);
    if (changed) {
      saveSccacheStateAtomic(statsFile, persisted);
    }

    const totals = persisted.totals;
    const lookups = totals.cacheHits + totals.cacheMisses;
    const hitRate = lookups > 0 ? round1((totals.cacheHits / lookups) * 100) : null;
    const avgReadHitMs = totals.cacheHits > 0 ? round1(totals.readHitDurationMs / totals.cacheHits) : null;
    const avgCompilerMs = totals.compilations > 0 ? round1(totals.compilerDurationMs / totals.compilations) : null;
    const levels = Object.values(totals.levels).map(({ name, hits, misses, writes, backfills }) => ({
      name,
      hits,
      misses,
      writes,
      backfills,
    }));

    return {
      available: info.available || activeBySocket.size > 0,
      version: info.version ?? daemonVersion,
      mode: persisted.mode,
      cacheLocation: persisted.cacheLocation,
      activeDaemons: activeBySocket.size,
      compileRequests: totals.compileRequests,
      cacheHits: totals.cacheHits,
      cacheMisses: totals.cacheMisses,
      hitRate,
      nonCacheable: totals.nonCacheable,
      errors: totals.errors,
      avgReadHitMs,
      avgCompilerMs,
      levels,
    };
  }

  async function collect({ projects = [], remoteCache = null, refresh = false } = {}) {
    const nowMs = now();
    const host = collectHost();
    const { fuseMounts, overlayMounts } = parseProcMounts(procRoot);
    const tcRoot = resolveToolCacheRoot(resolvedHome, env);
    const toolDisk = collectToolCacheDisk(tcRoot, nowMs, refresh);

    const [sccache, projectStats] = await Promise.all([
      collectSccache(),
      Promise.all(
        (Array.isArray(projects) ? projects : []).map(async (project) => {
          const wsDir = repoWorkspacesDir(project.dir);
          const warmDir = path.join(wsDir, ".warm");
          const [baseCommit, warmState] = await Promise.all([
            currentBaseCommitShort(project.dir),
            Promise.resolve(readWarmState(warmDir, nowMs)),
          ]);
          const status = deriveWarmStatus(warmState, baseCommit);
          const latest = warmState.ready[0] ?? null;
          const latestKey =
            latest?.key ??
            (warmState.building.length > 0 ? warmState.building[0] : null) ??
            (warmState.failed.length > 0 ? warmState.failed[0].key : null);
          const warmBytes = readWarmBytesCached(warmDir, nowMs, refresh);
          const { forks, plainWorkspaces } = collectProjectWorkspaces(wsDir, project.agents);

          return {
            workspaceId: project.workspaceId,
            name: project.name,
            repo: project.repo ?? null,
            baseCommit,
            status,
            latestKey,
            latestCommit: latest?.commit ?? null,
            builtAt: latest?.builtAt || null,
            buildSeconds: latest ? readBuildSeconds(warmDir, latest.key) : null,
            layers: latest?.layers ?? 0,
            maxLayers: MAX_LAYERS,
            readyCount: warmState.ready.length,
            failedCount: warmState.failed.length,
            lastError: warmState.failed.length > 0 ? readLastError(warmDir, warmState.failed[0]) : null,
            warmBytes,
            forks,
            plainWorkspaces,
          };
        }),
      ),
    ]);

    const totalForks = projectStats.reduce((sum, p) => sum + p.forks.length, 0);
    const totalPlainWorkspaces = projectStats.reduce((sum, p) => sum + p.plainWorkspaces.length, 0);
    const totalWarmBytes = projectStats.reduce((sum, p) => sum + p.warmBytes, 0);
    const hasWarmActivity = projectStats.some((p) => p.readyCount > 0 || p.status === "building");

    let activeMode = "plain";
    if (platform === "darwin") {
      if (hasWarmActivity) activeMode = "apfs-clone";
    } else if (platform === "linux") {
      if (totalForks > 0 || overlayMounts > 0 || hasWarmActivity) activeMode = "overlayfs";
    }

    return {
      collectedAt: nowMs,
      host,
      vfs: {
        activeMode,
        fuseReady: env.RUDDER_WORKER_FUSE_READY === "1",
        fuseMounts,
        overlayMounts,
        totalForks,
        totalPlainWorkspaces,
        totalWarmBytes,
        projects: projectStats,
      },
      toolCache: {
        root: tcRoot,
        totalBytes: toolDisk.totalBytes,
        entries: toolDisk.entries,
        sccache,
      },
      remoteCache: remoteCache ?? defaultRemoteCache(),
    };
  }

  async function prune({ projects = [], env: callEnv, distDir } = {}) {
    const effectiveEnv = callEnv ?? env;
    const tcRoot = resolveToolCacheRoot(resolvedHome, effectiveEnv);

    let toolCacheFreedBytes = 0;
    const distToolCache = distDir ? path.join(distDir, "tool-cache.js") : null;
    if (distToolCache && fs.existsSync(distToolCache)) {
      const mod = await import(pathToFileURL(distToolCache).href);
      toolCacheFreedBytes = Math.max(0, Number(mod.pruneToolCache(tcRoot, limits)) || 0);
    } else {
      toolCacheFreedBytes = pruneToolCacheInline(tcRoot, limits);
    }

    const projectList = Array.isArray(projects) ? projects : [];
    const warmDirs = projectList
      .filter((project) => project && typeof project.dir === "string" && project.dir)
      .map((project) => ({
        dir: project.dir,
        warmDir: path.join(repoWorkspacesDir(project.dir), ".warm"),
      }));

    const measureWarm = () =>
      warmDirs.reduce(
        (sum, entry) => sum + (fs.existsSync(entry.warmDir) ? walkDirBytes(entry.warmDir, { skipTopNames: new Set(["build"]) }) : 0),
        0,
      );

    const warmBefore = measureWarm();
    const distWarmSnapshot = distDir ? path.join(distDir, "warm-snapshot.js") : null;
    if (distWarmSnapshot && fs.existsSync(distWarmSnapshot) && warmDirs.length > 0) {
      const mod = await import(pathToFileURL(distWarmSnapshot).href);
      for (const entry of warmDirs) {
        await mod.pruneWarmSnapshots(entry.dir);
      }
    }
    const warmAfter = measureWarm();
    const warmFreedBytes = Math.max(0, warmBefore - warmAfter);

    bustDiskWalkCache();

    return {
      ok: true,
      toolCacheFreedBytes,
      warmFreedBytes,
      freedBytes: toolCacheFreedBytes + warmFreedBytes,
    };
  }

  return { collect, prune };
}
