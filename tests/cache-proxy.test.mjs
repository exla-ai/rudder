import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { startCacheProxy } from "../cloud/worker/cache-proxy.mjs";
import { createCleanupJob, pruneHomeToolCache } from "../cloud/worker/cleanup.mjs";
import { pruneWarmSnapshots, warmDir } from "../dist/warm-snapshot.js";

function tempDir(t, prefix = "rudder-cache-proxy-") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test("cache-proxy validates required config and enforces constant-time bearer auth and routes", async (t) => {
  assert.throws(() => startCacheProxy({ homeId: "h1", workerToken: "wt" }), /cloudUrl/);
  assert.throws(() => startCacheProxy({ cloudUrl: "https://cloud.test", workerToken: "wt" }), /homeId/);
  assert.throws(() => startCacheProxy({ cloudUrl: "https://cloud.test", homeId: "h1" }), /workerToken/);
  assert.throws(() => startCacheProxy({ cloudUrl: "https://cloud.test", homeId: "h1", workerToken: "wt", token: "   " }), /token/);

  const proxy = startCacheProxy({
    cloudUrl: "https://cloud.test/",
    homeId: "home-123",
    workerToken: "secret-worker-token",
    fetchImpl: async () => new Response("ok", { status: 200 }),
  });
  t.after(() => proxy.close());
  const { url, token, port } = await proxy.ready;

  assert.match(token, /^rdrc_[0-9a-f]+$/);
  assert.equal(proxy.token, token);
  assert.equal(proxy.url, url);
  assert.equal(url, `http://127.0.0.1:${port}`);
  assert.equal(proxy.snapshot().upstreamConnected, null);
  assert.equal(proxy.snapshot().proxyActive, true);
  assert.equal(proxy.snapshot().proxyUrl, url);

  // Missing or wrong auth -> 401
  const noAuth = await fetch(`${url}/sccache/abc`);
  assert.equal(noAuth.status, 401);
  const badAuth = await fetch(`${url}/sccache/abc`, { headers: { authorization: "Bearer wrong" } });
  assert.equal(badAuth.status, 401);
  const badScheme = await fetch(`${url}/sccache/abc`, { headers: { authorization: `Basic ${token}` } });
  assert.equal(badScheme.status, 401);

  // Unknown route -> 404 JSON
  const unknown = await fetch(`${url}/other/path`, { headers: { authorization: `Bearer ${token}` } });
  assert.equal(unknown.status, 404);
  assert.deepEqual(await unknown.json(), { error: "not found" });

  // Disallowed method -> 405 JSON
  const badMethod = await fetch(`${url}/sccache/abc`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}` },
  });
  assert.equal(badMethod.status, 405);
});

test("cache-proxy forwards sccache WebDAV and Turborepo requests with worker auth and tracks metrics", async (t) => {
  const dir = tempDir(t);
  const statsFile = path.join(dir, "nested", "remote-cache-stats.json");
  const seen = [];
  const store = new Map();

  const fetchImpl = async (targetUrl, init) => {
    const parsed = new URL(targetUrl);
    const body = init.body ? Buffer.from(init.body) : Buffer.alloc(0);
    seen.push({
      url: targetUrl,
      method: init.method,
      headers: init.headers,
      body,
    });
    assert.equal(init.headers.authorization, "Bearer secret-worker-token");
    assert.equal(init.headers["x-rudder-workspace-id"], "home-xyz");

    if (parsed.pathname === "/api/rudder/cache/sccache" && init.method === "OPTIONS") {
      return new Response(null, { status: 200, headers: { dav: "1", allow: "OPTIONS, GET, HEAD, PROPFIND, MKCOL, PUT, DELETE" } });
    }
    if (parsed.pathname === "/api/rudder/cache/sccache/a/b" && init.method === "PROPFIND") {
      return new Response("<multistatus/>", { status: 207, headers: { "content-type": "application/xml" } });
    }
    if (parsed.pathname === "/api/rudder/cache/sccache/a/b" && init.method === "MKCOL") {
      return new Response(null, { status: 201 });
    }
    if (parsed.pathname === "/api/rudder/cache/turbo/v8/artifacts/status" && init.method === "GET") {
      return new Response(JSON.stringify({ status: "enabled" }), { status: 200, headers: { "content-type": "application/json" } });
    }
    if (parsed.pathname === "/api/rudder/cache/turbo/v8/artifacts/events" && init.method === "POST") {
      return new Response("{}", { status: 200, headers: { "content-type": "application/json" } });
    }
    if (init.method === "PUT") {
      store.set(parsed.pathname, { body, tag: init.headers["x-artifact-tag"] ?? null });
      return new Response(null, { status: 201 });
    }
    if (init.method === "GET") {
      const entry = store.get(parsed.pathname);
      if (!entry) return new Response("missing", { status: 404 });
      const headers = { "content-type": "application/octet-stream" };
      if (entry.tag) headers["x-artifact-tag"] = entry.tag;
      return new Response(entry.body, { status: 200, headers });
    }
    if (init.method === "HEAD") {
      const entry = store.get(parsed.pathname);
      if (!entry) return new Response(null, { status: 404 });
      return new Response(null, { status: 200, headers: { "content-length": String(entry.body.length) } });
    }
    if (init.method === "DELETE") {
      store.delete(parsed.pathname);
      return new Response(null, { status: 204 });
    }
    return new Response("unexpected", { status: 400 });
  };

  const proxy = startCacheProxy({
    cloudUrl: "https://cloud.example",
    homeId: "home-xyz",
    workerToken: "secret-worker-token",
    statsFile,
    fetchImpl,
  });
  const { url, token } = await proxy.ready;
  const auth = { authorization: `Bearer ${token}` };

  // WebDAV OPTIONS, PROPFIND (with depth), MKCOL, and .sccache_check probes
  const opt = await fetch(`${url}/sccache`, { method: "OPTIONS", headers: auth });
  assert.equal(opt.status, 200);
  assert.equal(opt.headers.get("dav"), "1");

  const prop = await fetch(`${url}/sccache/a/b`, { method: "PROPFIND", headers: { ...auth, depth: "1" } });
  assert.equal(prop.status, 207);
  assert.equal(seen.at(-1).headers.depth, "1");

  const mkcol = await fetch(`${url}/sccache/a/b`, { method: "MKCOL", headers: auth });
  assert.equal(mkcol.status, 201);

  // .sccache_check must not count toward sccache hits/misses/writes
  assert.equal((await fetch(`${url}/sccache/.sccache_check`, { method: "PUT", headers: auth, body: "probe" })).status, 201);
  assert.equal((await fetch(`${url}/sccache/.sccache_check`, { method: "GET", headers: auth })).status, 200);
  assert.equal((await fetch(`${url}/sccache/.sccache_check`, { method: "DELETE", headers: auth })).status, 204);

  // Real sccache miss -> put -> hit
  assert.equal((await fetch(`${url}/sccache/obj/hash1`, { headers: auth })).status, 404);
  const sccachePayload = Buffer.from("compiled-rlib-bytes");
  assert.equal(
    (await fetch(`${url}/sccache/obj/hash1`, {
      method: "PUT",
      headers: { ...auth, "content-type": "application/octet-stream" },
      body: sccachePayload,
    })).status,
    201,
  );
  const sccacheHit = await fetch(`${url}/sccache/obj/hash1`, { headers: auth });
  assert.equal(sccacheHit.status, 200);
  assert.deepEqual(Buffer.from(await sccacheHit.arrayBuffer()), sccachePayload);

  // Turbo probes are skipped from counters; artifact miss -> put (with tag + query) -> hit
  assert.equal((await fetch(`${url}/turbo/v8/artifacts/status`, { headers: auth })).status, 200);
  assert.equal((await fetch(`${url}/turbo/v8/artifacts/events`, { method: "POST", headers: { ...auth, "content-type": "application/json" }, body: "[]" })).status, 200);
  assert.equal((await fetch(`${url}/turbo/v8/artifacts/deadbeef?teamId=rudder`, { headers: auth })).status, 404);
  assert.equal(seen.at(-1).url, "https://cloud.example/api/rudder/cache/turbo/v8/artifacts/deadbeef?teamId=rudder");

  const turboPayload = Buffer.from("turbo-tarball-payload");
  assert.equal(
    (await fetch(`${url}/turbo/v8/artifacts/deadbeef?teamId=rudder`, {
      method: "PUT",
      headers: { ...auth, "content-type": "application/octet-stream", "x-artifact-tag": "sig-42" },
      body: turboPayload,
    })).status,
    201,
  );
  assert.equal(seen.at(-1).headers["x-artifact-tag"], "sig-42");

  const turboHit = await fetch(`${url}/turbo/v8/artifacts/deadbeef?teamId=rudder`, { headers: auth });
  assert.equal(turboHit.status, 200);
  assert.equal(turboHit.headers.get("x-artifact-tag"), "sig-42");
  assert.deepEqual(Buffer.from(await turboHit.arrayBuffer()), turboPayload);
  // HEAD checks do not double-count as hits.
  assert.equal((await fetch(`${url}/turbo/v8/artifacts/deadbeef`, { method: "HEAD", headers: auth })).status, 200);

  const snap = proxy.snapshot();
  assert.equal(snap.upstreamConnected, true);
  assert.equal(snap.sccache.hits, 1);
  assert.equal(snap.sccache.misses, 1);
  assert.equal(snap.sccache.writes, 1);
  assert.equal(snap.sccache.errors, 0);
  assert.equal(snap.sccache.bytesRead, sccachePayload.length);
  assert.equal(snap.sccache.bytesWritten, sccachePayload.length);
  assert.equal(typeof snap.sccache.lastActivityAt, "number");

  assert.equal(snap.turbo.hits, 1);
  assert.equal(snap.turbo.misses, 1);
  assert.equal(snap.turbo.writes, 1);
  assert.equal(snap.turbo.errors, 0);
  assert.equal(snap.turbo.bytesRead, turboPayload.length);
  assert.equal(snap.turbo.bytesWritten, turboPayload.length);
  assert.equal(typeof snap.turbo.lastActivityAt, "number");
  assert.equal(snap.recent.length, 6);
  assert.equal(typeof snap.recent[0].at, "number");
  assert.deepEqual(
    snap.recent.map((e) => `${e.tool}:${e.op}:${e.key}`),
    [
      "sccache:miss:obj/hash1",
      "sccache:put:obj/hash1",
      "sccache:hit:obj/hash1",
      "turbo:miss:deadbeef",
      "turbo:put:deadbeef",
      "turbo:hit:deadbeef",
    ],
  );

  // Close flushes stats atomically; a restarted proxy loads prior cumulative counts.
  await proxy.close();
  assert.equal(proxy.snapshot().proxyActive, false);
  assert.ok(fs.existsSync(statsFile));

  const proxy2 = startCacheProxy({
    cloudUrl: "https://cloud.example",
    homeId: "home-xyz",
    workerToken: "secret-worker-token",
    statsFile,
    fetchImpl,
  });
  t.after(() => proxy2.close());
  await proxy2.ready;

  const restored = proxy2.snapshot();
  assert.equal(restored.sccache.hits, 1);
  assert.equal(restored.sccache.writes, 1);
  assert.equal(restored.sccache.bytesRead, sccachePayload.length);
  assert.equal(restored.turbo.hits, 1);
  assert.equal(restored.turbo.bytesWritten, turboPayload.length);
  assert.equal(restored.recent.length, 6);
});

test("cache-proxy returns 503 cleanly on upstream failure or timeout and caps recent ring buffer at 30", async (t) => {
  let mode = "fail";
  const proxy = startCacheProxy({
    cloudUrl: "https://cloud.example",
    homeId: "home-err",
    workerToken: "wt",
    timeoutMs: 80,
    fetchImpl: async (_url, init) => {
      if (mode === "fail") throw new Error("ECONNREFUSED");
      if (mode === "hang") {
        await new Promise((_resolve, reject) => {
          init.signal.addEventListener("abort", () => reject(init.signal.reason || new Error("aborted")));
        });
      }
      return new Response("miss", { status: 404 });
    },
  });
  t.after(() => proxy.close());
  const { url, token } = await proxy.ready;
  const auth = { authorization: `Bearer ${token}` };

  const errRes = await fetch(`${url}/sccache/k1`, { headers: auth });
  assert.equal(errRes.status, 503);
  assert.equal(proxy.snapshot().upstreamConnected, false);
  assert.equal(proxy.snapshot().sccache.errors, 1);

  mode = "hang";
  const timeoutRes = await fetch(`${url}/turbo/v8/artifacts/k2`, { headers: auth });
  assert.equal(timeoutRes.status, 503);
  assert.equal(proxy.snapshot().upstreamConnected, false);
  assert.equal(proxy.snapshot().turbo.errors, 1);

  mode = "ok";
  for (let i = 0; i < 35; i += 1) {
    const res = await fetch(`${url}/sccache/item-${i}`, { headers: auth });
    assert.equal(res.status, 404);
  }
  const snap = proxy.snapshot();
  assert.equal(snap.upstreamConnected, true);
  assert.equal(snap.sccache.misses, 35);
  assert.equal(snap.recent.length, 30);
  assert.equal(snap.recent[0].key, "item-5");
  assert.equal(snap.recent[29].key, "item-34");
});

test("cleanup pruneHomeToolCache evicts LRU files over limit and hourly cleanup reports toolCacheFreedBytes", async (t) => {
  const dir = tempDir(t, "rudder-cleanup-cache-");
  const cacheRoot = path.join(dir, "tool-cache");
  const npmDir = path.join(cacheRoot, "npm");
  fs.mkdirSync(npmDir, { recursive: true });
  const outside = path.join(dir, "outside.txt");
  fs.writeFileSync(outside, "keep-me");
  fs.symlinkSync(outside, path.join(npmDir, "link"));

  for (const [i, name] of ["oldest", "middle", "newest"].entries()) {
    const file = path.join(npmDir, name);
    fs.writeFileSync(file, Buffer.alloc(120));
    const when = new Date(Date.now() - (3 - i) * 60_000);
    fs.utimesSync(file, when, when);
  }

  const freed = pruneHomeToolCache({ RUDDER_TOOL_CACHE_DIR: cacheRoot }, { npm: 250 });
  assert.equal(freed, 120);
  assert.deepEqual(fs.readdirSync(npmDir).sort(), ["link", "middle", "newest"]);
  assert.equal(fs.readFileSync(outside, "utf8"), "keep-me");
  assert.equal(pruneHomeToolCache({ RUDDER_TOOL_CACHE_DIR: cacheRoot }, { npm: 250 }), 0);

  // Hourly cleanup run records toolCacheFreedBytes in last.json.
  const proc = path.join(dir, "proc");
  const home = path.join(dir, "home");
  const volume = path.join(dir, "workspace");
  fs.mkdirSync(path.join(proc, "pressure"), { recursive: true });
  fs.writeFileSync(path.join(proc, "uptime"), "1000.00 1.00\n");
  fs.writeFileSync(path.join(proc, "loadavg"), "0.10 0.10 0.10 1/10 100\n");
  fs.writeFileSync(path.join(proc, "meminfo"), "MemTotal: 16000000 kB\nMemAvailable: 8000000 kB\nSwapTotal: 0 kB\n");
  fs.mkdirSync(path.join(volume, "code"), { recursive: true });
  const paths = { home, volume, codeDir: path.join(volume, "code"), rudderHome: path.join(home, ".rudder"), procRoot: proc };
  const spawnImpl = () => {
    const child = new EventEmitter();
    child.stdout = new EventEmitter();
    child.stderr = new EventEmitter();
    child.kill = () => {};
    setImmediate(() => child.emit("close", 0));
    return child;
  };
  const runs = [];
  const job = createCleanupJob({ env: { HOME: home }, paths, spawnImpl, onRun: (r) => runs.push(r) });
  assert.equal(job.runNow("hourly"), true);
  await new Promise((resolve) => setImmediate(resolve));
  const last = JSON.parse(fs.readFileSync(path.join(paths.rudderHome, "cleanup", "last.json"), "utf8"));
  assert.equal(last.toolCacheFreedBytes, 0);
  assert.equal(runs[0].scratchFreedBytes, 0);
});

test("pruneWarmSnapshots removes stale unreferenced materialized lower dirs and broken symlinks while keeping active ones", async (t) => {
  const dir = tempDir(t, "rudder-warm-lowers-");
  const repoRoot = path.join(dir, "repo");
  fs.mkdirSync(repoRoot, { recursive: true });

  const warm = warmDir(repoRoot);
  const workspacesRoot = path.dirname(warm);
  const lowerLinks = path.join(warm, "lower-links");
  const legacyLowers = path.join(warm, "lowers");
  fs.mkdirSync(lowerLinks, { recursive: true });
  fs.mkdirSync(legacyLowers, { recursive: true });

  // A kept snapshot
  const snapDir = path.join(warm, "snap-keep");
  fs.mkdirSync(snapDir, { recursive: true });
  fs.writeFileSync(path.join(warm, "snap-keep.ready"), JSON.stringify({
    key: "snap-keep",
    commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    dir: snapDir,
    builtAt: new Date().toISOString(),
  }));

  const tenMinAgo = new Date(Date.now() - 10 * 60_000);
  const makeMaterialized = (root, name, ageDate) => {
    const target = path.join(root, name);
    fs.mkdirSync(path.join(target, "sub"), { recursive: true });
    fs.writeFileSync(path.join(target, "sub", "file.txt"), "data");
    if (ageDate) fs.utimesSync(target, ageDate, ageDate);
    return target;
  };

  // Unreferenced old materialized dirs (both .warm/lower-links and legacy .warm/lowers) -> must be pruned
  const staleNew = makeMaterialized(lowerLinks, "stale-tree-1", tenMinAgo);
  const staleLegacy = makeMaterialized(legacyLowers, "stale-legacy-1", tenMinAgo);
  // Fresh unreferenced materialized dir (within 5-min grace window) -> must stay on default grace
  const freshUnreferenced = makeMaterialized(lowerLinks, "fresh-tree-1", null);
  // Referenced materialized dirs -> must stay even though old
  const usedByWc = makeMaterialized(lowerLinks, "active-wc-tree", tenMinAgo);
  const usedByFork = makeMaterialized(lowerLinks, "active-fork-tree", tenMinAgo);

  // Set up two active forks referencing usedByWc and usedByFork
  const ws1 = path.join(workspacesRoot, "agent-1");
  fs.mkdirSync(path.join(ws1, ".jj", "working_copy"), { recursive: true });
  fs.mkdirSync(`${ws1}.fork`, { recursive: true });
  fs.writeFileSync(path.join(ws1, ".jj", "working_copy", "lower"), usedByWc);
  fs.writeFileSync(path.join(`${ws1}.fork`, "lower"), snapDir);

  const ws2 = path.join(workspacesRoot, "agent-2");
  fs.mkdirSync(`${ws2}.fork`, { recursive: true });
  fs.writeFileSync(path.join(`${ws2}.fork`, "lower"), usedByFork);

  // Valid symlink to kept snapshot -> stays; broken symlinks -> pruned
  const validLink = path.join(lowerLinks, "valid-link");
  const brokenLink = path.join(lowerLinks, "broken-link");
  const brokenLegacyLink = path.join(legacyLowers, "broken-legacy");
  fs.symlinkSync(snapDir, validLink);
  fs.symlinkSync(path.join(warm, "deleted-snap"), brokenLink);
  fs.symlinkSync(path.join(warm, "deleted-snap"), brokenLegacyLink);

  await pruneWarmSnapshots(repoRoot);

  assert.equal(fs.existsSync(staleNew), false, "old unreferenced materialized dir in lower-links is removed");
  assert.equal(fs.existsSync(staleLegacy), false, "old unreferenced materialized dir in legacy lowers is removed");
  assert.equal(fs.existsSync(freshUnreferenced), true, "fresh materialized dir inside grace window is preserved");
  assert.equal(fs.existsSync(usedByWc), true, "materialized dir referenced by working_copy/lower is preserved");
  assert.equal(fs.existsSync(usedByFork), true, "materialized dir referenced by .fork/lower is preserved");
  assert.equal(fs.lstatSync(validLink).isSymbolicLink(), true, "valid symlink is preserved");
  assert.throws(() => fs.lstatSync(brokenLink), { code: "ENOENT" });
  assert.throws(() => fs.lstatSync(brokenLegacyLink), { code: "ENOENT" });

  // With lowerGraceMs: 0, even freshUnreferenced is pruned while referenced ones stay.
  await pruneWarmSnapshots(repoRoot, 1, { lowerGraceMs: 0 });
  assert.equal(fs.existsSync(freshUnreferenced), false);
  assert.equal(fs.existsSync(usedByWc), true);
  assert.equal(fs.existsSync(usedByFork), true);
});
