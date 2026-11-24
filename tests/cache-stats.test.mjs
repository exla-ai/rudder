// Unit and integration tests for cloud/worker/cache-stats.mjs using temporary
// directories, fake /proc trees, and injected command runners (no live daemons
// or network calls).
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  agentWorkspaceMode,
  createMachineStatsCollector,
  projectWarmBadge,
} from "../cloud/worker/cache-stats.mjs";

function makeTempDir(t, prefix = "rudder-cache-stats-") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => {
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return dir;
}

function repoSlug(repoDir) {
  const base = path.basename(repoDir).toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 48) || "repo";
  const hash = createHash("sha1").update(repoDir).digest("hex").slice(0, 10);
  return `${base}-${hash}`;
}

function makeProjectLayout(root, name = "demo-repo") {
  const repoDir = path.join(root, name);
  const wsDir = path.join(repoDir, ".rudder-workspaces", repoSlug(repoDir));
  const warmDir = path.join(wsDir, ".warm");
  fs.mkdirSync(warmDir, { recursive: true });
  return { repoDir, wsDir, warmDir };
}

function writeGitHead(repoDir, commit) {
  const gitDir = path.join(repoDir, ".git");
  fs.mkdirSync(gitDir, { recursive: true });
  fs.writeFileSync(path.join(gitDir, "HEAD"), `${commit}\n`, "utf8");
}

function writeReadySnapshot(warmDir, { key, commit, builtAt, layers = 1, fileBytes = 128, buildSeconds = null }) {
  const layerDirs = [];
  for (let i = 0; i < layers; i += 1) {
    const layerKey = i === 0 ? key : `${key.slice(0, 10)}${String(i).padStart(2, "0")}`;
    const layerDir = path.join(warmDir, layerKey);
    fs.mkdirSync(layerDir, { recursive: true });
    if (i === 0 && fileBytes > 0) {
      fs.writeFileSync(path.join(layerDir, "payload.bin"), Buffer.alloc(fileBytes));
    }
    layerDirs.push(layerDir);
  }
  const snapDir = layerDirs[0];
  if (layers > 1) {
    fs.writeFileSync(`${snapDir}.lowerdir`, layerDirs.join(":"), "utf8");
  }
  const snap = {
    key,
    commit,
    dir: snapDir,
    builtAt,
    ...(layers > 1 ? { layers: layerDirs } : {}),
  };
  fs.writeFileSync(path.join(warmDir, `${key}.ready`), JSON.stringify(snap), "utf8");
  if (buildSeconds !== null) {
    fs.writeFileSync(
      path.join(warmDir, `${key}.log`),
      `[rudder] warm snapshot ${key}: cargo build\n[rudder] warm snapshot ${key}: ready in ${buildSeconds} s (${layers} layer(s))\n`,
      "utf8",
    );
  }
  return snap;
}

function makeForkWorkspace(wsDir, runId, snapshotDir, upperFiles = 0) {
  const ws = path.join(wsDir, runId);
  const fork = `${ws}.fork`;
  const upper = path.join(ws, ".jj", "working_copy", "upper");
  fs.mkdirSync(upper, { recursive: true });
  fs.mkdirSync(fork, { recursive: true });
  fs.writeFileSync(path.join(fork, "enter"), "#!/bin/sh\n", { mode: 0o755 });
  fs.writeFileSync(path.join(fork, "lower"), snapshotDir, "utf8");
  for (let i = 0; i < upperFiles; i += 1) {
    fs.writeFileSync(path.join(upper, `file-${i}.txt`), `v${i}\n`, "utf8");
  }
  return ws;
}

function makeFakeProc(root, { unixLines = [], tcpLines = [], mountLines = [] } = {}) {
  const proc = path.join(root, "proc");
  fs.mkdirSync(path.join(proc, "pressure"), { recursive: true });
  fs.mkdirSync(path.join(proc, "net"), { recursive: true });
  fs.writeFileSync(
    path.join(proc, "meminfo"),
    [
      "MemTotal:       32768000 kB",
      "MemFree:         4096000 kB",
      "MemAvailable:   20480000 kB",
      "SwapTotal:       4194304 kB",
      "SwapFree:        3145728 kB",
    ].join("\n") + "\n",
    "utf8",
  );
  fs.writeFileSync(
    path.join(proc, "pressure", "memory"),
    "some avg10=12.50 avg60=10.00 avg300=5.00 total=100\nfull avg10=4.25 avg60=3.00 avg300=1.00 total=50\n",
    "utf8",
  );
  fs.writeFileSync(
    path.join(proc, "pressure", "io"),
    "some avg10=8.00 avg60=4.00 avg300=2.00 total=20\nfull avg10=1.00 avg60=0.50 avg300=0.10 total=10\n",
    "utf8",
  );
  fs.writeFileSync(
    path.join(proc, "pressure", "cpu"),
    "some avg10=3.50 avg60=2.00 avg300=1.00 total=30\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
    "utf8",
  );
  fs.writeFileSync(path.join(proc, "loadavg"), "1.75 2.25 3.50 2/300 12345\n", "utf8");
  fs.writeFileSync(
    path.join(proc, "mounts"),
    mountLines.join("\n") + (mountLines.length ? "\n" : ""),
    "utf8",
  );
  fs.writeFileSync(
    path.join(proc, "net", "unix"),
    ["Num       RefCount Protocol Flags    Type St Inode Path", ...unixLines].join("\n") + "\n",
    "utf8",
  );
  fs.writeFileSync(
    path.join(proc, "net", "tcp"),
    ["  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode", ...tcpLines].join("\n") + "\n",
    "utf8",
  );
  return proc;
}

function sccacheJson({
  compileRequests = 0,
  hits = {},
  misses = {},
  cacheErrors = {},
  readErrors = 0,
  writeErrors = 0,
  nonCacheable = 0,
  compilations = 0,
  readHitSecs = 0,
  readHitNanos = 0,
  compilerSecs = 0,
  compilerNanos = 0,
  multiLevel = null,
  cacheLocation = 'Local disk: "/tmp/sccache"',
  version = "0.18.0",
} = {}) {
  return JSON.stringify({
    stats: {
      compile_requests: compileRequests,
      requests_unsupported_compiler: 0,
      requests_not_compile: 0,
      requests_not_cacheable: 0,
      requests_executed: compileRequests,
      cache_errors: { counts: cacheErrors, adv_counts: {} },
      cache_hits: { counts: hits, adv_counts: {} },
      cache_misses: { counts: misses, adv_counts: {} },
      cache_timeouts: 0,
      cache_read_errors: readErrors,
      non_cacheable_compilations: nonCacheable,
      forced_recaches: 0,
      cache_write_errors: writeErrors,
      cache_writes: compilations,
      cache_write_duration: { secs: 0, nanos: 0 },
      cache_read_hit_duration: { secs: readHitSecs, nanos: readHitNanos },
      compilations,
      compiler_write_duration: { secs: compilerSecs, nanos: compilerNanos },
      compile_fails: 0,
      not_cached: {},
      dist_compiles: {},
      dist_errors: 0,
      multi_level: multiLevel,
    },
    cache_location: cacheLocation,
    cache_size: null,
    max_cache_size: 21474836480,
    use_preprocessor_cache_mode: true,
    version,
    basedirs: [],
  });
}

// ---------------------------------------------------------------------------
// 1. projectWarmBadge tests
// ---------------------------------------------------------------------------

test("projectWarmBadge reports none, ready, stale, building (live vs dead pid), and failed", (t) => {
  const root = makeTempDir(t);
  const { repoDir, wsDir, warmDir } = makeProjectLayout(root, "alpha");

  assert.throws(() => projectWarmBadge(""), TypeError);

  // Empty .warm -> none
  assert.deepEqual(projectWarmBadge(repoDir), {
    status: "none",
    key: null,
    commit: null,
    layers: 0,
    maxLayers: 8,
    builtAt: null,
    forkCount: 0,
  });

  // Dead PID in .building -> ignored -> still none
  fs.writeFileSync(path.join(warmDir, "dead00000001.building"), "99999999", "utf8");
  assert.equal(projectWarmBadge(repoDir).status, "none");

  // Live PID in .building -> building
  fs.writeFileSync(path.join(warmDir, "live00000002.building"), String(process.pid), "utf8");
  assert.deepEqual(projectWarmBadge(repoDir), {
    status: "building",
    key: "live00000002",
    commit: null,
    layers: 0,
    maxLayers: 8,
    builtAt: null,
    forkCount: 0,
  });
  fs.rmSync(path.join(warmDir, "live00000002.building"));
  fs.rmSync(path.join(warmDir, "dead00000001.building"));

  // Only .failed -> failed
  fs.writeFileSync(path.join(warmDir, "fail00000003.failed"), "Error: cargo build failed\n", "utf8");
  assert.deepEqual(projectWarmBadge(repoDir), {
    status: "failed",
    key: "fail00000003",
    commit: null,
    layers: 0,
    maxLayers: 8,
    builtAt: null,
    forkCount: 0,
  });

  // Ready snapshot matching HEAD + 2 forks -> ready
  const headCommit = "11eec25736c07beeba277325b1be8424e6926d6a";
  const snap = writeReadySnapshot(warmDir, {
    key: "11eec25736c0",
    commit: headCommit,
    builtAt: "2026-11-24T00:37:34.283Z",
    layers: 3,
  });
  writeGitHead(repoDir, headCommit);
  makeForkWorkspace(wsDir, "agent-one-abcde", snap.dir, 2);
  makeForkWorkspace(wsDir, "agent-two-fghij", snap.dir, 0);

  const readyBadge = projectWarmBadge(repoDir);
  assert.deepEqual(readyBadge, {
    status: "ready",
    key: "11eec25736c0",
    commit: headCommit,
    layers: 3,
    maxLayers: 8,
    builtAt: "2026-11-24T00:37:34.283Z",
    forkCount: 2,
  });

  // Stable across repeated calls (non-volatile for ETag)
  assert.equal(JSON.stringify(projectWarmBadge(repoDir)), JSON.stringify(readyBadge));

  // HEAD moves to a newer commit -> stale
  writeGitHead(repoDir, "22ff00112233445566778899aabbccddeeff0011");
  const staleBadge = projectWarmBadge(repoDir);
  assert.equal(staleBadge.status, "stale");
  assert.equal(staleBadge.key, "11eec25736c0");
  assert.equal(staleBadge.layers, 3);
  assert.equal(staleBadge.forkCount, 2);

  // If the new base commit itself failed -> failed
  fs.writeFileSync(path.join(warmDir, "22ff00112233.failed"), "Error: npm ci failed\n", "utf8");
  assert.equal(projectWarmBadge(repoDir).status, "failed");

  // Live .building takes precedence over ready/stale/failed
  fs.writeFileSync(path.join(warmDir, "22ff00112233.building"), String(process.pid), "utf8");
  assert.equal(projectWarmBadge(repoDir).status, "building");
});

// ---------------------------------------------------------------------------
// 2. agentWorkspaceMode tests
// ---------------------------------------------------------------------------

test("agentWorkspaceMode distinguishes overlay forks (stacked and flat), plain checkouts, and missing dirs", (t) => {
  const root = makeTempDir(t);
  const { wsDir, warmDir } = makeProjectLayout(root, "beta");

  const stacked = writeReadySnapshot(warmDir, {
    key: "de4609ec9c06",
    commit: "de4609ec9c060000000000000000000000000000",
    builtAt: "2026-11-24T00:00:00.000Z",
    layers: 4,
  });
  const flat = writeReadySnapshot(warmDir, {
    key: "8075c51f2f3c",
    commit: "8075c51f2f3c0000000000000000000000000000",
    builtAt: "2026-11-23T00:00:00.000Z",
    layers: 1,
  });

  const stackedWs = makeForkWorkspace(wsDir, "task-stacked-12345", stacked.dir);
  const flatWs = makeForkWorkspace(wsDir, "task-flat-67890", flat.dir);
  const plainWs = path.join(wsDir, "task-plain-abcde");
  fs.mkdirSync(plainWs, { recursive: true });

  assert.deepEqual(agentWorkspaceMode(stackedWs), {
    kind: "overlay-fork",
    snapshotKey: "de4609ec9c06",
    layers: 4,
  });
  assert.deepEqual(agentWorkspaceMode(`${stackedWs}/`), {
    kind: "overlay-fork",
    snapshotKey: "de4609ec9c06",
    layers: 4,
  });
  assert.deepEqual(agentWorkspaceMode(flatWs), {
    kind: "overlay-fork",
    snapshotKey: "8075c51f2f3c",
    layers: 1,
  });
  assert.deepEqual(agentWorkspaceMode(plainWs), { kind: "plain" });
  assert.equal(agentWorkspaceMode(path.join(wsDir, "does-not-exist")), null);
  assert.equal(agentWorkspaceMode(null), null);
  assert.equal(agentWorkspaceMode(""), null);
});

// ---------------------------------------------------------------------------
// 3. createMachineStatsCollector.collect(): host, mounts, VFS, warmBytes, toolCache
// ---------------------------------------------------------------------------

test("collect() parses host /proc, mount counts, .warm sizing (skipping build & symlinks), forks, and tool cache", async (t) => {
  const root = makeTempDir(t);
  const home = path.join(root, "home");
  fs.mkdirSync(home, { recursive: true });

  const { repoDir, wsDir, warmDir } = makeProjectLayout(root, "gamma");
  const commit = "11eec25736c07beeba277325b1be8424e6926d6a";
  const snap = writeReadySnapshot(warmDir, {
    key: "11eec25736c0",
    commit,
    builtAt: "2026-11-24T00:37:34.283Z",
    layers: 3,
    fileBytes: 1000,
    buildSeconds: "20.4",
  });

  // Put huge decoy files inside .warm/build and via a symlink: BOTH must be skipped!
  const buildBindDir = path.join(warmDir, "build");
  fs.mkdirSync(buildBindDir, { recursive: true });
  fs.writeFileSync(path.join(buildBindDir, "huge-workspace-file.bin"), Buffer.alloc(500_000));
  const outsideFile = path.join(root, "outside-huge.bin");
  fs.writeFileSync(outsideFile, Buffer.alloc(400_000));
  fs.mkdirSync(path.join(warmDir, "lower-links"), { recursive: true });
  fs.symlinkSync(snap.dir, path.join(warmDir, "lower-links", "tree-link"));
  fs.symlinkSync(outsideFile, path.join(warmDir, "dangling-or-external-link"));

  // Add a failed marker with error message
  fs.writeFileSync(path.join(warmDir, "oldfail12345.failed"), "Error: npm run build failed\nstack trace\n", "utf8");

  // Create two forks (one active, one idle) and one plain workspace
  const activeWs = makeForkWorkspace(wsDir, "agent-active-aaaaa", snap.dir, 7);
  const idleWs = makeForkWorkspace(wsDir, "agent-idle-bbbbb", snap.dir, 2);
  fs.mkdirSync(path.join(wsDir, "agent-plain-ccccc"), { recursive: true });

  // Populate tool cache: one fresh file, one 48h old file, and one symlink (ignored)
  const tcRoot = path.join(home, ".cache", "rudder", "tool-cache");
  fs.mkdirSync(path.join(tcRoot, "sccache"), { recursive: true });
  fs.mkdirSync(path.join(tcRoot, "cargo"), { recursive: true });
  const freshFile = path.join(tcRoot, "sccache", "fresh.obj");
  const oldFile = path.join(tcRoot, "sccache", "old.obj");
  fs.writeFileSync(freshFile, Buffer.alloc(2048));
  fs.writeFileSync(oldFile, Buffer.alloc(1024));
  const nowMs = Date.now();
  const twoDaysAgo = new Date(nowMs - 48 * 3600_000);
  fs.utimesSync(oldFile, twoDaysAgo, twoDaysAgo);
  fs.symlinkSync(outsideFile, path.join(tcRoot, "cargo", "config.toml"));

  const proc = makeFakeProc(root, {
    mountLines: [
      "overlay / overlay rw,lowerdir=a:b 0 0",
      "overlay /usr/local/bin/jj overlay rw,lowerdir=a:b 0 0",
      "/dev/nvme0n1 /workspace ext4 rw,noatime 0 0",
      "overlay /workspace/code/gamma/.rudder-workspaces/gamma-123/agent-a overlay rw,userxattr 0 0",
      "overlay /workspace/code/gamma/.rudder-workspaces/gamma-123/agent-b overlay rw,userxattr 0 0",
      "rudderfs /workspace/code/gamma/.rudder-workspaces/gamma-123/agent-c fuse.rudder rw 0 0",
      "fusectl /sys/fs/fuse/connections fusectl rw 0 0",
      "sshfs /mnt/remote fuse.sshfs rw 0 0",
    ],
  });

  const runCommand = async (cmd, args) => {
    if (cmd === "jj" && args.includes("@-")) {
      return { code: 0, stdout: `${commit}\n`, stderr: "" };
    }
    if (cmd === "sccache" && args[0] === "--version") {
      return { code: 0, stdout: "sccache 0.18.0\n", stderr: "" };
    }
    throw new Error(`unexpected command: ${cmd} ${args.join(" ")}`);
  };

  const collector = createMachineStatsCollector({
    home,
    procRoot: proc,
    mounts: [root, "/definitely/not/mounted/rudder-test"],
    cacheTtlMs: 20_000,
    runCommand,
    env: { HOME: home, RUDDER_WORKER_FUSE_READY: "1" },
    platform: "linux",
    now: () => nowMs,
  });

  const remoteCache = {
    proxyActive: true,
    proxyUrl: "http://127.0.0.1:4227",
    upstreamConnected: true,
    sccache: { hits: 10, misses: 2, writes: 2, errors: 0, bytesRead: 1024, bytesWritten: 512, lastActivityAt: nowMs },
    turbo: { hits: 4, misses: 1, writes: 1, errors: 0, bytesRead: 256, bytesWritten: 128, lastActivityAt: nowMs },
    recent: [{ tool: "sccache", op: "GET", status: "hit", bytes: 1024, ms: 12, at: nowMs }],
  };

  const stats = await collector.collect({
    projects: [
      {
        workspaceId: "ws-1",
        name: "gamma",
        repo: "exla-ai/gamma",
        dir: repoDir,
        agents: [
          { id: "agent-active-aaaaa", status: "working", workspacePath: activeWs },
          { id: "agent-idle-bbbbb", status: "done", workspacePath: idleWs },
        ],
      },
    ],
    remoteCache,
  });

  // Host assertions
  assert.equal(stats.collectedAt, nowMs);
  assert.equal(stats.host.memTotalBytes, 32768000 * 1024);
  assert.equal(stats.host.memAvailableBytes, 20480000 * 1024);
  assert.equal(stats.host.swapTotalBytes, 4194304 * 1024);
  assert.equal(stats.host.swapFreeBytes, 3145728 * 1024);
  assert.ok(stats.host.cpus >= 1);
  assert.equal(stats.host.load1, 1.75);
  assert.equal(stats.host.load5, 2.25);
  assert.equal(stats.host.load15, 3.5);
  assert.equal(stats.host.disks.length, 1);
  assert.equal(stats.host.disks[0].mount, root);
  assert.ok(stats.host.disks[0].totalBytes > 0);
  assert.deepEqual(stats.host.pressure, {
    memorySome: 12.5,
    memoryFull: 4.25,
    ioSome: 8,
    cpuSome: 3.5,
  });

  // VFS assertions
  assert.equal(stats.vfs.activeMode, "overlayfs");
  assert.equal(stats.vfs.fuseReady, true);
  assert.equal(stats.vfs.fuseMounts, 2); // fuse.rudder + fuse.sshfs (NOT fusectl)
  assert.equal(stats.vfs.overlayMounts, 2); // only overlay mounts under /.rudder-workspaces/
  assert.equal(stats.vfs.totalForks, 2);
  assert.equal(stats.vfs.totalPlainWorkspaces, 1);

  assert.equal(stats.vfs.projects.length, 1);
  const proj = stats.vfs.projects[0];
  assert.equal(proj.workspaceId, "ws-1");
  assert.equal(proj.baseCommit, "11eec25736c0");
  assert.equal(proj.status, "ready");
  assert.equal(proj.latestKey, "11eec25736c0");
  assert.equal(proj.latestCommit, commit);
  assert.equal(proj.buildSeconds, 20.4);
  assert.equal(proj.layers, 3);
  assert.equal(proj.maxLayers, 8);
  assert.equal(proj.readyCount, 1);
  assert.equal(proj.failedCount, 1);
  assert.equal(proj.lastError, "Error: npm run build failed");
  assert.ok(proj.warmBytes >= 1000 && proj.warmBytes < 50_000, `warmBytes=${proj.warmBytes} must exclude .warm/build and symlinks`);
  assert.equal(stats.vfs.totalWarmBytes, proj.warmBytes);

  assert.deepEqual(proj.forks, [
    { runId: "agent-active-aaaaa", snapshotKey: "11eec25736c0", layers: 3, upperFiles: 7, active: true },
    { runId: "agent-idle-bbbbb", snapshotKey: "11eec25736c0", layers: 3, upperFiles: 2, active: false },
  ]);
  assert.deepEqual(proj.plainWorkspaces, ["agent-plain-ccccc"]);

  // Tool cache assertions
  assert.equal(stats.toolCache.root, tcRoot);
  assert.equal(stats.toolCache.totalBytes, 3072);
  const sccacheEntry = stats.toolCache.entries.find((e) => e.name === "sccache");
  assert.equal(sccacheEntry.bytes, 3072);
  assert.equal(sccacheEntry.limitBytes, 20 * 1024 ** 3);
  assert.equal(sccacheEntry.fileCount, 2);
  assert.equal(sccacheEntry.recentFiles24h, 1);
  assert.ok(sccacheEntry.lastWriteAt !== null);

  const cargoEntry = stats.toolCache.entries.find((e) => e.name === "cargo");
  assert.equal(cargoEntry.bytes, 0, "symlink in cargo dir must not be counted");
  assert.equal(cargoEntry.fileCount, 0);

  // sccache: zero listening sockets -> never invoked --show-stats!
  assert.equal(stats.toolCache.sccache.available, true);
  assert.equal(stats.toolCache.sccache.version, "0.18.0");
  assert.equal(stats.toolCache.sccache.activeDaemons, 0);
  assert.equal(stats.toolCache.sccache.hitRate, null);

  // remoteCache passthrough
  assert.deepEqual(stats.remoteCache, remoteCache);
});

// ---------------------------------------------------------------------------
// 4. sccache multi-daemon aggregation + persistent high-water-mark accumulation
// ---------------------------------------------------------------------------

test("sccache aggregates multiple listening daemons and persists totals across daemon exit and restart", async (t) => {
  const root = makeTempDir(t);
  const home = path.join(root, "home");
  fs.mkdirSync(home, { recursive: true });
  const statsFile = path.join(home, ".rudder", "sccache-stats.json");

  const proc = makeFakeProc(root, {
    unixLines: [
      "0000000000000000: 00000002 00000000 00010000 0001 01 1001 @rudder-sccache-agent1",
      // Connected (non-listening) socket must be ignored!
      "0000000000000000: 00000003 00000000 00000000 0001 03 1002 @rudder-sccache-agent1",
      "0000000000000000: 00000002 00000000 00010000 0001 01 1003 @rudder-sccache-agent2",
      // Non-rudder socket must be ignored!
      "0000000000000000: 00000002 00000000 00010000 0001 01 1004 @other-daemon",
    ],
    tcpLines: [
      "   0: 0100007F:1082 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 2001",
      // Non-LISTEN TCP entry on port 4226 must be ignored!
      "   1: 0100007F:1082 0100007F:9999 01 00000000:00000000 00:00000000 00000000 1000 0 2002",
    ],
  });

  // Live daemon state keyed by SCCACHE_SERVER_UDS (or "TCP" when unset)
  const daemons = new Map();
  const queriedUds = [];

  daemons.set("\\x00rudder-sccache-agent1", sccacheJson({
    compileRequests: 20,
    hits: { Rust: 15 },
    misses: { Rust: 5 },
    nonCacheable: 1,
    readErrors: 1,
    compilations: 5,
    readHitSecs: 0,
    readHitNanos: 150_000_000, // 150ms total -> 10ms avg
    compilerSecs: 5,
    compilerNanos: 0, // 5000ms total -> 1000ms avg
    cacheLocation: "Multi-level (2 levels)",
    multiLevel: [
      { name: "L0 (disk)", hits: 10, misses: 10, writes: 10, write_failures: 0, backfills_from: 0, backfills_to: 5 },
      { name: "L1 (webdav)", hits: 5, misses: 5, writes: 5, write_failures: 0, backfills_from: 5, backfills_to: 0 },
    ],
  }));
  daemons.set("\\x00rudder-sccache-agent2", sccacheJson({
    compileRequests: 10,
    hits: { Rust: 6, "C/C++": 2 },
    misses: { Rust: 2 },
    cacheErrors: { Rust: 1 },
    writeErrors: 1,
    compilations: 2,
    readHitSecs: 0,
    readHitNanos: 80_000_000,
    compilerSecs: 2,
    compilerNanos: 0,
    cacheLocation: "Multi-level (2 levels)",
    multiLevel: [
      { name: "L0 (disk)", hits: 6, misses: 4, writes: 4, write_failures: 0, backfills_from: 0, backfills_to: 2 },
      { name: "L1 (webdav)", hits: 2, misses: 2, writes: 2, write_failures: 0, backfills_from: 2, backfills_to: 0 },
    ],
  }));
  daemons.set("TCP", sccacheJson({
    compileRequests: 5,
    hits: { Rust: 2 },
    misses: { Rust: 3 },
    compilations: 3,
    readHitSecs: 0,
    readHitNanos: 20_000_000,
    compilerSecs: 3,
    compilerNanos: 0,
    cacheLocation: 'Local disk: "/home/u/.cache/rudder/tool-cache/sccache"',
    multiLevel: null,
  }));

  const runCommand = async (cmd, args, opts) => {
    if (cmd === "sccache" && args[0] === "--version") {
      return { code: 0, stdout: "sccache 0.18.0\n", stderr: "" };
    }
    if (cmd === "sccache" && args.includes("--show-stats")) {
      const key = opts?.env?.SCCACHE_SERVER_UDS ?? "TCP";
      queriedUds.push(key);
      const out = daemons.get(key);
      return out ? { code: 0, stdout: out, stderr: "" } : { code: 2, stdout: "", stderr: "no server" };
    }
    return { code: 1, stdout: "", stderr: "unexpected" };
  };

  const collector = createMachineStatsCollector({
    home,
    procRoot: proc,
    mounts: [root],
    sccacheStatsFile: statsFile,
    runCommand,
    env: { HOME: home },
    platform: "linux",
  });

  // Pass 1: all 3 daemons active
  const r1 = await collector.collect();
  assert.deepEqual(queriedUds.sort(), ["TCP", "\\x00rudder-sccache-agent1", "\\x00rudder-sccache-agent2"]);
  assert.equal(r1.toolCache.sccache.activeDaemons, 3);
  assert.equal(r1.toolCache.sccache.mode, "multilevel");
  assert.equal(r1.toolCache.sccache.cacheLocation, "Multi-level (2 levels)");
  assert.equal(r1.toolCache.sccache.compileRequests, 35); // 20 + 10 + 5
  assert.equal(r1.toolCache.sccache.cacheHits, 25); // 15 + 8 + 2
  assert.equal(r1.toolCache.sccache.cacheMisses, 10); // 5 + 2 + 3
  assert.equal(r1.toolCache.sccache.hitRate, 71.4); // 25/35 * 100 = 71.428 -> 71.4
  assert.equal(r1.toolCache.sccache.nonCacheable, 1);
  assert.equal(r1.toolCache.sccache.errors, 3); // 1 readError + 1 cacheError + 1 writeError
  assert.equal(r1.toolCache.sccache.avgReadHitMs, 10); // 250ms / 25 hits = 10.0
  assert.equal(r1.toolCache.sccache.avgCompilerMs, 1000); // 10000ms / 10 compilations = 1000.0
  assert.deepEqual(r1.toolCache.sccache.levels, [
    { name: "L0 (disk)", hits: 16, misses: 14, writes: 14, backfills: 7 },
    { name: "L1 (webdav)", hits: 7, misses: 7, writes: 7, backfills: 7 },
  ]);
  assert.ok(fs.existsSync(statsFile), "cumulative sccache stats persisted atomically");

  // Default inactive remoteCache shape when omitted
  assert.equal(r1.remoteCache.proxyActive, false);
  assert.equal(r1.remoteCache.proxyUrl, null);
  assert.deepEqual(r1.remoteCache.recent, []);

  // Pass 2: agent1 grows (+5 reqs, +4 hits, +1 miss), agent2 EXITS, TCP restarts with smaller counts (2 reqs, 2 hits)
  fs.writeFileSync(
    path.join(proc, "net", "unix"),
    [
      "Num       RefCount Protocol Flags    Type St Inode Path",
      "0000000000000000: 00000002 00000000 00010000 0001 01 1001 @rudder-sccache-agent1",
    ].join("\n") + "\n",
    "utf8",
  );
  daemons.set("\\x00rudder-sccache-agent1", sccacheJson({
    compileRequests: 25,
    hits: { Rust: 19 },
    misses: { Rust: 6 },
    nonCacheable: 1,
    readErrors: 1,
    compilations: 6,
    readHitSecs: 0,
    readHitNanos: 190_000_000,
    compilerSecs: 6,
    compilerNanos: 0,
    cacheLocation: "Multi-level (2 levels)",
    multiLevel: [
      { name: "L0 (disk)", hits: 13, misses: 12, writes: 12, write_failures: 0, backfills_from: 0, backfills_to: 6 },
      { name: "L1 (webdav)", hits: 6, misses: 6, writes: 6, write_failures: 0, backfills_from: 6, backfills_to: 0 },
    ],
  }));
  daemons.delete("\\x00rudder-sccache-agent2");
  daemons.set("TCP", sccacheJson({
    compileRequests: 2, // < 5 -> restarted daemon!
    hits: { Rust: 2 },
    misses: {},
    compilations: 0,
    readHitSecs: 0,
    readHitNanos: 20_000_000,
    compilerSecs: 0,
    compilerNanos: 0,
    multiLevel: null,
  }));

  const r2 = await collector.collect();
  assert.equal(r2.toolCache.sccache.activeDaemons, 2);
  // Totals must NEVER lose agent2's prior 10 reqs or TCP's prior 5 reqs:
  // 35 + 5 (agent1 delta) + 2 (restarted TCP) = 42
  assert.equal(r2.toolCache.sccache.compileRequests, 42);
  assert.equal(r2.toolCache.sccache.cacheHits, 31); // 25 + 4 + 2
  assert.equal(r2.toolCache.sccache.cacheMisses, 11); // 10 + 1 + 0
  assert.deepEqual(r2.toolCache.sccache.levels, [
    { name: "L0 (disk)", hits: 19, misses: 16, writes: 16, backfills: 8 },
    { name: "L1 (webdav)", hits: 8, misses: 8, writes: 8, backfills: 8 },
  ]);

  // Pass 3: ALL daemons exit AND a fresh collector instance loads persisted totals
  fs.writeFileSync(path.join(proc, "net", "unix"), "Num RefCount Protocol Flags Type St Inode Path\n", "utf8");
  fs.writeFileSync(path.join(proc, "net", "tcp"), "  sl  local_address rem_address st\n", "utf8");
  const collector2 = createMachineStatsCollector({
    home,
    procRoot: proc,
    mounts: [root],
    sccacheStatsFile: statsFile,
    runCommand,
    env: { HOME: home },
    platform: "linux",
  });
  const r3 = await collector2.collect();
  assert.equal(r3.toolCache.sccache.activeDaemons, 0);
  assert.equal(r3.toolCache.sccache.compileRequests, 42);
  assert.equal(r3.toolCache.sccache.cacheHits, 31);
  assert.equal(r3.toolCache.sccache.cacheMisses, 11);
  assert.equal(r3.toolCache.sccache.mode, "multilevel");
});

// ---------------------------------------------------------------------------
// 5. Cache TTL hit, expiration, refresh bypass, and prune()
// ---------------------------------------------------------------------------

test("collect() caches disk walks for cacheTtlMs, honors refresh: true, and prune() frees over-limit files and busts cache", async (t) => {
  const root = makeTempDir(t);
  const home = path.join(root, "home");
  const tcRoot = path.join(home, ".cache", "rudder", "tool-cache");
  const npmDir = path.join(tcRoot, "npm");
  fs.mkdirSync(npmDir, { recursive: true });

  const { repoDir, warmDir } = makeProjectLayout(root, "delta");
  const snap = writeReadySnapshot(warmDir, {
    key: "aaaa1111bbbb",
    commit: "aaaa1111bbbb0000000000000000000000000000",
    builtAt: "2026-11-24T00:00:00.000Z",
    fileBytes: 500,
  });

  // 3 files in npm cache: 100 + 100 + 100 = 300 bytes
  for (const [idx, name] of ["oldest.tgz", "middle.tgz", "newest.tgz"].entries()) {
    const file = path.join(npmDir, name);
    fs.writeFileSync(file, Buffer.alloc(100));
    const ts = new Date(1_700_000_000_000 + idx * 60_000);
    fs.utimesSync(file, ts, ts);
  }

  const proc = makeFakeProc(root);
  let clock = 1_700_000_500_000;
  const runCommand = async () => ({ code: 1, stdout: "", stderr: "" });

  const collector = createMachineStatsCollector({
    home,
    procRoot: proc,
    mounts: [root],
    cacheTtlMs: 20_000,
    runCommand,
    toolCacheLimits: { npm: 250, sccache: 1024 },
    env: { HOME: home },
    platform: "linux",
    now: () => clock,
  });

  const projects = [{ workspaceId: "ws-d", name: "delta", repo: "exla-ai/delta", dir: repoDir }];

  // Initial collect
  const c1 = await collector.collect({ projects });
  assert.equal(c1.toolCache.totalBytes, 300);
  const initialWarmBytes = c1.vfs.projects[0].warmBytes;

  // Add files to both tool-cache and .warm while inside TTL
  fs.writeFileSync(path.join(npmDir, "extra.tgz"), Buffer.alloc(150));
  fs.writeFileSync(path.join(snap.dir, "extra-warm.bin"), Buffer.alloc(400));
  clock += 5_000; // still within 20s TTL

  // Cached hit: disk walks NOT re-run
  const c2 = await collector.collect({ projects });
  assert.equal(c2.toolCache.totalBytes, 300);
  assert.equal(c2.vfs.projects[0].warmBytes, initialWarmBytes);

  // refresh: true bypasses cache
  const c3 = await collector.collect({ projects, refresh: true });
  assert.equal(c3.toolCache.totalBytes, 450);
  assert.equal(c3.vfs.projects[0].warmBytes, initialWarmBytes + 400);

  // Add another file and advance clock past TTL -> automatically refreshes
  fs.writeFileSync(path.join(npmDir, "later.tgz"), Buffer.alloc(50));
  clock += 25_000;
  const c4 = await collector.collect({ projects });
  assert.equal(c4.toolCache.totalBytes, 500);

  // Prune over-limit npm cache (limit 250, current 500 -> should delete oldest 3 files = 300 bytes, leaving 200 <= 250)
  const pruned = await collector.prune({ projects });
  assert.equal(pruned.ok, true);
  assert.equal(pruned.toolCacheFreedBytes, 300);
  assert.equal(pruned.warmFreedBytes, 0);
  assert.equal(pruned.freedBytes, 300);

  // Disk-walk cache MUST be busted immediately after prune (even with clock unchanged!)
  const c5 = await collector.collect({ projects });
  assert.equal(c5.toolCache.totalBytes, 200);
  assert.equal(fs.existsSync(path.join(npmDir, "oldest.tgz")), false);
  assert.equal(fs.existsSync(path.join(npmDir, "extra.tgz")), true);
  assert.equal(fs.existsSync(path.join(npmDir, "later.tgz")), true);
});

test("prune() integrates distDir warm-snapshot and tool-cache pruners and darwin reports apfs-clone mode", async (t) => {
  const root = makeTempDir(t);
  const home = path.join(root, "home");
  const distDir = path.join(root, "dist");
  fs.mkdirSync(home, { recursive: true });
  fs.mkdirSync(distDir, { recursive: true });

  const { repoDir, warmDir } = makeProjectLayout(root, "macproj");
  const oldSnap = writeReadySnapshot(warmDir, {
    key: "111111111111",
    commit: "1111111111110000000000000000000000000000",
    builtAt: "2026-11-23T00:00:00.000Z",
    fileBytes: 600,
  });
  writeReadySnapshot(warmDir, {
    key: "222222222222",
    commit: "2222222222220000000000000000000000000000",
    builtAt: "2026-11-24T00:00:00.000Z",
    fileBytes: 400,
  });

  fs.writeFileSync(
    path.join(distDir, "tool-cache.js"),
    `export function pruneToolCache() { return 1234; }\n`,
    "utf8",
  );
  fs.writeFileSync(
    path.join(distDir, "warm-snapshot.js"),
    `import fs from "node:fs";
export async function pruneWarmSnapshots() {
  fs.rmSync(${JSON.stringify(oldSnap.dir)}, { recursive: true, force: true });
  fs.rmSync(${JSON.stringify(path.join(warmDir, "111111111111.ready"))}, { force: true });
}
`,
    "utf8",
  );

  const collector = createMachineStatsCollector({
    home,
    procRoot: path.join(root, "nonexistent-proc"),
    mounts: [root],
    runCommand: async () => ({ code: 1, stdout: "", stderr: "" }),
    env: { HOME: home },
    platform: "darwin",
  });

  const projects = [{ workspaceId: "ws-m", name: "macproj", repo: "exla-ai/macproj", dir: repoDir }];
  const before = await collector.collect({ projects });
  assert.equal(before.vfs.activeMode, "apfs-clone");
  assert.ok(before.host.memTotalBytes > 0);
  assert.equal(before.host.pressure.memorySome, null);
  const warmBefore = before.vfs.projects[0].warmBytes;

  const pruned = await collector.prune({ projects, distDir });
  assert.equal(pruned.ok, true);
  assert.equal(pruned.toolCacheFreedBytes, 1234);
  assert.ok(pruned.warmFreedBytes >= 600, `expected >= 600 freed, got ${pruned.warmFreedBytes}`);
  assert.equal(pruned.freedBytes, pruned.toolCacheFreedBytes + pruned.warmFreedBytes);

  const after = await collector.collect({ projects });
  assert.equal(after.vfs.projects[0].warmBytes, warmBefore - pruned.warmFreedBytes);
});

