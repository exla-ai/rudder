import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  MemoryRemoteCacheStorage,
  createRemoteBuildCacheHandler,
} from "../cloud/dist/remote-build-cache.js";
import { startCacheProxy } from "../cloud/worker/cache-proxy.mjs";
import { createMachineStatsCollector } from "../cloud/worker/cache-stats.mjs";
import { toolCacheEnv } from "../dist/tool-cache.js";

function hasTools(...names) {
  return names.every((name) => spawnSync("sh", ["-c", `command -v ${name}`], { stdio: "ignore" }).status === 0);
}

function runAsync(cmd, args, { cwd, env, timeoutMs = 30_000 } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => { stdout += d; });
    child.stderr.on("data", (d) => { stderr += d; });
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`${cmd} timed out after ${timeoutMs}ms: ${stderr}`));
    }, timeoutMs);
    child.on("error", (err) => { clearTimeout(timer); reject(err); });
    child.on("close", (status) => { clearTimeout(timer); resolve({ status, stdout, stderr }); });
  });
}

test("full remote cache pipeline: rustc -> sccache multilevel -> home cache-proxy -> cloud handler -> machineStats", async (t) => {
  if (!hasTools("sccache", "rustc")) {
    t.skip("sccache or rustc not available");
    return;
  }

  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rudder-e2e-cache-"));
  const serversToStop = [];
  t.after(() => {
    for (const env of serversToStop) {
      spawnSync("sccache", ["--stop-server"], { env, stdio: "ignore", timeout: 5_000 });
    }
    fs.rmSync(root, { recursive: true, force: true });
  });

  // 1. Cloud control plane remote build cache handler (/api/rudder/cache/*)
  const storage = new MemoryRemoteCacheStorage();
  const handler = createRemoteBuildCacheHandler({
    storage,
    bucket: "rudder-test-bucket",
    authenticate: (req) => {
      assert.equal(req.headers.authorization, "Bearer rdrw_home_secret");
      assert.equal(req.headers["x-rudder-workspace-id"], "home_e2e");
      return "acct_e2e";
    },
  });
  const cloudServer = http.createServer((req, res) => {
    const url = new URL(req.url || "/", "http://127.0.0.1");
    void handler(req, res, url);
  });
  await new Promise((resolve) => cloudServer.listen(0, "127.0.0.1", resolve));
  t.after(() => cloudServer.close());
  const cloudAddr = cloudServer.address();
  const cloudUrl = `http://127.0.0.1:${cloudAddr.port}`;

  // 2. Home loopback cache proxy (isolates workerToken from agents)
  const statsFile = path.join(root, "remote-cache-stats.json");
  const proxy = startCacheProxy({
    cloudUrl,
    homeId: "home_e2e",
    workerToken: "rdrw_home_secret",
    statsFile,
  });
  t.after(() => proxy.close());
  const proxyInfo = await proxy.ready;

  // 3. Machine 1 builds a Rust crate at the canonical workspace path (e.g. .warm/build or CARGO_HOME crate)
  // Cold local disk + remote -> compiles and uploads to Cloud S3 WebDAV.
  const ws = path.join(root, "warm-build");
  const out = path.join(ws, "target");
  const cache1 = path.join(root, "tool-cache-1");
  const cache2 = path.join(root, "tool-cache-2");
  fs.mkdirSync(out, { recursive: true });
  fs.writeFileSync(path.join(ws, "lib.rs"), "pub fn answer(x: u32) -> u32 { x.wrapping_add(42) }\n");
  const sharedCargo = path.join(root, "shared-cargo");
  fs.mkdirSync(sharedCargo, { recursive: true });

  const buildBaseEnv = (cacheRoot, udsName) => {
    const base = {
      ...process.env,
      HOME: root,
      RUSTUP_HOME: process.env.RUSTUP_HOME || path.join(os.homedir(), ".rustup"),
      RUDDER_REMOTE_CACHE_URL: proxyInfo.url,
      RUDDER_REMOTE_CACHE_TOKEN: proxyInfo.token,
      JJ_CONFIG: path.join(root, "jj.toml"),
    };
    for (const k of Object.keys(base)) {
      if (k.startsWith("SCCACHE_")) delete base[k];
    }
    const tc = toolCacheEnv("agent", { root: cacheRoot, env: base, workspace: ws });
    assert.equal(tc.SCCACHE_MULTILEVEL_CHAIN, "disk,webdav");
    assert.equal(tc.SCCACHE_WEBDAV_ENDPOINT, `${proxyInfo.url}/sccache`);
    assert.equal(tc.TURBO_API, `${proxyInfo.url}/turbo`);
    return { ...base, ...tc, CARGO_HOME: sharedCargo, SCCACHE_SERVER_UDS: `\\x00${udsName}` };
  };

  const env1 = buildBaseEnv(cache1, `rudder-sccache-e2e-1-${process.pid}`);
  serversToStop.push(env1);
  const c1 = await runAsync(
    "sccache",
    ["rustc", "--crate-name", "rudder_e2e_probe", "--crate-type", "lib", "--edition=2021", "--emit=link", "lib.rs", "--out-dir", "target"],
    { cwd: ws, env: env1 },
  );
  assert.equal(c1.status, 0, `first compile failed: ${c1.stderr}`);

  // Multilevel sccache writes L0 (disk) synchronously and uploads L1 (webdav)
  // asynchronously in the background; wait briefly for the upload to settle.
  const writeDeadline = Date.now() + 5_000;
  while (Date.now() < writeDeadline && (handler.statsForAccount("acct_e2e").sccache.writes < 1 || proxy.snapshot().sccache.writes < 1)) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  const cloudAfter1 = handler.statsForAccount("acct_e2e");
  assert.ok(cloudAfter1.sccache.writes >= 1, `expected cloud sccache write, got ${JSON.stringify(cloudAfter1.sccache)}`);
  assert.ok(cloudAfter1.sccache.bytesWritten > 0);
  await runAsync("sccache", ["--stop-server"], { env: env1 }).catch(() => {});

  // 4. Machine 2 starts with a completely empty local disk cache and clean output dir -> MUST hit remote Cloud WebDAV cache!
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
  const env2 = buildBaseEnv(cache2, `rudder-sccache-e2e-2-${process.pid}`);
  serversToStop.push(env2);
  const c2 = await runAsync(
    "sccache",
    ["rustc", "--crate-name", "rudder_e2e_probe", "--crate-type", "lib", "--edition=2021", "--emit=link", "lib.rs", "--out-dir", "target"],
    { cwd: ws, env: env2 },
  );
  assert.equal(c2.status, 0, `second compile failed: ${c2.stderr}`);

  const cloudAfter2 = handler.statsForAccount("acct_e2e");
  assert.ok(cloudAfter2.sccache.hits >= 1, `expected cloud sccache hit, got ${JSON.stringify(cloudAfter2.sccache)}`);
  assert.ok(cloudAfter2.sccache.bytesRead > 0);
  assert.ok(cloudAfter2.recent.some((e) => e.tool === "sccache" && e.op === "hit"));

  // 5. Verify Turborepo v8 artifact round-trip through the same loopback proxy -> cloud handler
  const putTurbo = await fetch(`${proxyInfo.url}/turbo/v8/artifacts/abcd1234efgh5678`, {
    method: "PUT",
    headers: { authorization: `Bearer ${proxyInfo.token}`, "x-artifact-tag": "tag-e2e-1" },
    body: Buffer.from("turbo-artifact-bytes"),
  });
  assert.equal(putTurbo.status, 200);
  const getTurbo = await fetch(`${proxyInfo.url}/turbo/v8/artifacts/abcd1234efgh5678`, {
    headers: { authorization: `Bearer ${proxyInfo.token}` },
  });
  assert.equal(getTurbo.status, 200);
  assert.equal(getTurbo.headers.get("x-artifact-tag"), "tag-e2e-1");
  assert.equal(await getTurbo.text(), "turbo-artifact-bytes");

  // 6. Verify machineStats collector aggregates proxy + toolCache + sccache cleanly
  const collector = createMachineStatsCollector({
    home: root,
    env: env2,
    sccacheStatsFile: path.join(root, "sccache-stats.json"),
  });
  const snap = await collector.collect({
    projects: [],
    remoteCache: proxy.snapshot(),
    refresh: true,
  });
  assert.equal(snap.remoteCache.proxyActive, true);
  assert.equal(snap.remoteCache.upstreamConnected, true);
  assert.ok(snap.remoteCache.sccache.hits >= 1);
  assert.ok(snap.remoteCache.sccache.writes >= 1);
  assert.equal(snap.remoteCache.turbo.hits, 1);
  assert.equal(snap.remoteCache.turbo.writes, 1);
  assert.ok(snap.remoteCache.recent.length >= 3);
  assert.equal(snap.toolCache.sccache.available, true);
  assert.equal(snap.toolCache.sccache.mode, "multilevel");
  assert.ok(snap.toolCache.sccache.cacheHits >= 1, `expected sccache hit in machineStats, got ${JSON.stringify(snap.toolCache.sccache)}`);
});
