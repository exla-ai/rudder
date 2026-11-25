import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createHash, randomUUID } from "node:crypto";
import { Readable } from "node:stream";
import {
  createRemoteBuildCacheHandler,
  MemoryRemoteCacheStorage,
  S3RemoteCacheStorage,
} from "../cloud/dist/remote-build-cache.js";
import { createFabricHttpHandler } from "../cloud/dist/fabric-http.js";
import { canonicalFabricMetadata, FabricVerificationCache } from "../cloud/dist/fabric-cas.js";

const execFileAsync = promisify(execFile);
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

function rawRequest(port, options) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        host: "127.0.0.1",
        port,
        method: options.method ?? "GET",
        path: options.path,
        headers: options.headers ?? {},
      },
      (res) => {
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => {
          resolve({
            status: res.statusCode,
            headers: res.headers,
            body: Buffer.concat(chunks),
          });
        });
      },
    );
    req.on("error", reject);
    if (options.body !== undefined) req.write(options.body);
    req.end();
  });
}

async function startCacheServer(t, options = {}) {
  const storage = options.storage ?? new MemoryRemoteCacheStorage();
  const tokens = new Map([
    ["token-a", "account-a"],
    ["token-b", "account-b"],
    ["test-token", "sccache-acct"],
  ]);
  let tick = options.startTime ?? 1_700_000_000_000;
  const now = options.now ?? (() => (tick += 5));
  const handler = createRemoteBuildCacheHandler({
    storage,
    bucket: options.bucket ?? "rudder-test-cache-bucket",
    maxObjectBytes: options.maxObjectBytes,
    now,
    authenticate: options.authenticate ?? ((req) => {
      const auth = req.headers.authorization;
      if (auth === "Bearer forbidden-token") {
        throw Object.assign(new Error("internal forbidden detail"), { status: 403 });
      }
      const bearer = typeof auth === "string" && auth.startsWith("Bearer ") ? auth.slice(7) : "";
      const acct = tokens.get(bearer);
      if (!acct) {
        throw Object.assign(new Error("internal auth detail"), { status: 401 });
      }
      return acct;
    }),
  });

  const server = http.createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    void handler(req, res, url);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  });
  const port = server.address().port;
  return { server, port, url: `http://127.0.0.1:${port}`, storage, handler };
}

test("Buck2 bazel-remote HTTP backend keeps verified blobs and action results account-scoped", async (t) => {
  const { port, storage, handler } = await startCacheServer(t, { maxObjectBytes: 16 });
  const blob = Buffer.from([0, 128, 255, 1]);
  const digest = sha256(blob);
  const actionResult = Buffer.from([0x20, 1]); // REAPI ActionResult with exit_code 1
  const cas = `cas/${digest}`;
  const ac = `ac/${digest}`;
  function request(method, key, token, body) {
    return rawRequest(port, {
      method, path: `/api/rudder/cache/buck2/${key}`,
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      body,
    });
  }

  assert.equal((await request("PUT", cas, null, blob)).status, 401);
  assert.equal((await request("GET", ac, "token-a")).status, 404);
  assert.equal((await request("HEAD", cas, "token-a")).status, 404);
  assert.equal((await request("PUT", cas, "token-a", blob)).status, 200);
  assert.equal((await request("PUT", ac, "token-a", actionResult)).status, 200);
  const head = await request("HEAD", cas, "token-a");
  assert.equal(head.status, 200);
  assert.equal(head.headers["content-length"], String(blob.length));
  assert.equal(head.body.length, 0);

  assert.equal((await request("PUT", cas, "token-a", actionResult)).status, 400);
  assert.equal((await request("PUT", ac, "token-a", Buffer.alloc(17))).status, 413);
  assert.equal((await request("DELETE", ac, "token-a")).status, 405);
  for (const key of [`cas/${digest.toUpperCase()}`, "cas/abcd", `cas.v2/${digest}`, `ac/../cas/${digest}`]) {
    assert.equal((await request("GET", key, "token-a")).status, 400);
  }
  for (const [key, expected] of [[cas, blob], [ac, actionResult]]) {
    assert.equal((await request("GET", key, "token-b")).status, 404);
    const read = await request("GET", key, "token-a");
    assert.equal(read.status, 200);
    assert.equal(read.headers["content-length"], String(expected.length));
    assert.deepEqual(read.body, expected);
  }
  const prefix = `cache/v1/accounts/${sha256("account-a")}/buck2/`;
  assert.deepEqual(storage.keys().sort(), [prefix + ac, prefix + cas]);
  const stats = handler.statsForAccount("account-a");
  assert.equal(stats.buck2.hits, 2);
  assert.equal(stats.buck2.misses, 1);
  assert.equal(stats.buck2.writes, 2);
  assert.equal(stats.buck2.bytesRead, blob.length + actionResult.length);
  assert.equal(stats.buck2.bytesWritten, blob.length + actionResult.length);
  assert.equal(stats.sccache.writes + stats.turbo.writes, 0);
  assert.ok(stats.recent.every((event) => event.tool === "buck2"));
  assert.equal(handler.stats().buck2.misses, 3);
  assert.equal(handler.statsForAccount("account-b").buck2.writes, 0);

  await storage.put(prefix + cas, actionResult);
  assert.equal((await request("GET", cas, "token-a")).status, 503);
  const empty = `cas/${sha256(Buffer.alloc(0))}`;
  assert.equal((await request("PUT", empty, "token-a", Buffer.alloc(0))).status, 200);
  const emptyRead = await request("GET", empty, "token-a");
  assert.equal(emptyRead.status, 200);
  assert.equal(emptyRead.headers["content-length"], "0");
});

test("sccache WebDAV protocol, account isolation, path validation, size limits and auth", async (t) => {
  const { port, url, storage } = await startCacheServer(t, { maxObjectBytes: 128 });
  const authA = { Authorization: "Bearer token-a" };
  const authB = { Authorization: "Bearer token-b" };

  // OPTIONS
  const opt = await rawRequest(port, { method: "OPTIONS", path: "/api/rudder/cache/sccache", headers: authA });
  assert.equal(opt.status, 200);
  assert.equal(opt.headers.dav, "1, 2");
  for (const method of ["OPTIONS", "GET", "HEAD", "PUT", "DELETE", "PROPFIND", "MKCOL"]) {
    assert.ok(opt.headers.allow?.includes(method), `Allow header missing ${method}`);
  }

  // MKCOL
  const mkcol = await rawRequest(port, { method: "MKCOL", path: "/api/rudder/cache/sccache/0/a", headers: authA });
  assert.equal(mkcol.status, 201);

  // PROPFIND on root and trailing-slash collections
  for (const colPath of ["/api/rudder/cache/sccache", "/api/rudder/cache/sccache/", "/api/rudder/cache/sccache/0/a/"]) {
    const prop = await rawRequest(port, { method: "PROPFIND", path: colPath, headers: authA });
    assert.equal(prop.status, 207);
    assert.match(prop.headers["content-type"] ?? "", /application\/xml/);
    const xml = prop.body.toString("utf8");
    assert.ok(xml.includes("<D:collection/>"));
    assert.ok(xml.includes("HTTP/1.1 200 OK"));
    assert.ok(xml.includes("<D:getlastmodified>"));
  }

  // PROPFIND / HEAD / GET on missing file
  const missingProp = await rawRequest(port, { method: "PROPFIND", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(missingProp.status, 404);
  const missingHead = await rawRequest(port, { method: "HEAD", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(missingHead.status, 404);
  const missingGet = await rawRequest(port, { method: "GET", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(missingGet.status, 404);

  // PUT -> HEAD -> PROPFIND -> GET
  const payload = Buffer.from("compiled-object-bytes-\x00\xff-v1");
  const putRes = await rawRequest(port, {
    method: "PUT",
    path: "/api/rudder/cache/sccache/0/a/deadbeef",
    headers: { ...authA, "Content-Length": String(payload.length) },
    body: payload,
  });
  assert.equal(putRes.status, 201);
  assert.ok(putRes.headers.etag);

  const expectedKey = `cache/v1/accounts/${sha256("account-a")}/sccache/0/a/deadbeef`;
  assert.deepEqual(storage.keys(), [expectedKey]);

  const headRes = await rawRequest(port, { method: "HEAD", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(headRes.status, 200);
  assert.equal(headRes.headers["content-length"], String(payload.length));
  assert.equal(headRes.headers.etag, putRes.headers.etag);
  assert.ok(headRes.headers["last-modified"]);

  const propFile = await rawRequest(port, { method: "PROPFIND", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(propFile.status, 207);
  const propXml = propFile.body.toString("utf8");
  assert.ok(propXml.includes(`<D:getcontentlength>${payload.length}</D:getcontentlength>`));
  assert.ok(propXml.includes("<D:resourcetype/>"));
  assert.ok(propXml.includes("<D:getetag>"));
  assert.ok(propXml.includes("HTTP/1.1 200 OK"));

  const getRes = await rawRequest(port, { method: "GET", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(getRes.status, 200);
  assert.equal(getRes.headers["content-type"], "application/octet-stream");
  assert.equal(getRes.headers["cache-control"], "private, immutable");
  assert.equal(getRes.headers.etag, putRes.headers.etag);
  assert.deepEqual(getRes.body, payload);

  // Account isolation: account B cannot read, stat, or delete account A's object
  assert.equal((await rawRequest(port, { method: "GET", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authB })).status, 404);
  assert.equal((await rawRequest(port, { method: "HEAD", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authB })).status, 404);
  assert.equal((await rawRequest(port, { method: "PROPFIND", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authB })).status, 404);
  assert.equal((await rawRequest(port, { method: "DELETE", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authB })).status, 204);
  assert.equal((await rawRequest(port, { method: "GET", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA })).status, 200);

  // DELETE by owner
  const delRes = await rawRequest(port, { method: "DELETE", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA });
  assert.equal(delRes.status, 204);
  assert.equal((await rawRequest(port, { method: "GET", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA })).status, 404);
  assert.equal((await rawRequest(port, { method: "DELETE", path: "/api/rudder/cache/sccache/0/a/deadbeef", headers: authA })).status, 204);

  // Path traversal, NUL, control chars, >512 chars rejected with 400
  const badPaths = [
    "/api/rudder/cache/sccache/a/../secret",
    "/api/rudder/cache/sccache/%2e%2e/secret",
    "/api/rudder/cache/sccache/a%00b",
    "/api/rudder/cache/sccache/a%0ab",
    "/api/rudder/cache/sccache/a%5cb",
    `/api/rudder/cache/sccache/${"a".repeat(520)}`,
  ];
  for (const badPath of badPaths) {
    const res = await rawRequest(port, { method: "GET", path: badPath, headers: authA });
    assert.equal(res.status, 400, `Expected 400 for ${badPath}, got ${res.status}`);
  }

  // Size limit 413 (both with Content-Length and chunked)
  const oversized = Buffer.alloc(129, 0x41);
  const put413 = await rawRequest(port, {
    method: "PUT",
    path: "/api/rudder/cache/sccache/0/a/too-big",
    headers: { ...authA, "Content-Length": String(oversized.length) },
    body: oversized,
  });
  assert.equal(put413.status, 413);

  const chunked413 = await rawRequest(port, {
    method: "PUT",
    path: "/api/rudder/cache/sccache/0/a/too-big-chunked",
    headers: { ...authA, "Transfer-Encoding": "chunked" },
    body: oversized,
  });
  assert.equal(chunked413.status, 413);

  // Auth 401 and 403 do not leak private auth errors
  const unauth = await fetch(`${url}/api/rudder/cache/sccache/0/a/x`);
  assert.equal(unauth.status, 401);
  const unauthText = await unauth.text();
  assert.ok(!unauthText.includes("internal auth detail"));

  const forbidden = await fetch(`${url}/api/rudder/cache/sccache/0/a/x`, {
    headers: { Authorization: "Bearer forbidden-token" },
  });
  assert.equal(forbidden.status, 403);
  const forbiddenText = await forbidden.text();
  assert.ok(!forbiddenText.includes("internal forbidden detail"));
});

test("Turborepo v8 remote cache API preserves artifact tags and enforces isolation", async (t) => {
  const { url, storage } = await startCacheServer(t);
  const authA = { Authorization: "Bearer token-a" };
  const authB = { Authorization: "Bearer token-b" };
  const hash = "a1b2c3d4e5f60718";

  // Status
  const statusRes = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/status?teamId=team_1`, { headers: authA });
  assert.equal(statusRes.status, 200);
  assert.deepEqual(await statusRes.json(), { status: "enabled" });

  // Events
  const eventsRes = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/events`, {
    method: "POST",
    headers: { ...authA, "Content-Type": "application/json" },
    body: JSON.stringify([{ sessionId: "s1", source: "REMOTE", event: "HIT", hash, duration: 12 }]),
  });
  assert.equal(eventsRes.status, 200);
  assert.deepEqual(await eventsRes.json(), {});

  // Oversized events > 256 KiB -> 413
  const bigEvents = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/events`, {
    method: "POST",
    headers: { ...authA, "Content-Type": "application/json" },
    body: Buffer.alloc(256 * 1024 + 10, 0x20),
  });
  assert.equal(bigEvents.status, 413);

  // HEAD / GET miss before PUT
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, { method: "HEAD", headers: authA })).status, 404);
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, { method: "GET", headers: authA })).status, 404);

  // PUT with x-artifact-tag
  const artifact = Buffer.from("tar-gzip-turbo-artifact-payload");
  const tag = "sig-v1:abcdef0123456789";
  const putRes = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}?teamId=team_1`, {
    method: "PUT",
    headers: { ...authA, "Content-Type": "application/octet-stream", "x-artifact-tag": tag },
    body: artifact,
  });
  assert.equal(putRes.status, 200);
  assert.deepEqual(await putRes.json(), { urls: [`/api/rudder/cache/turbo/v8/artifacts/${hash}`] });

  const expectedKey = `cache/v1/accounts/${sha256("account-a")}/turbo/${hash}`;
  assert.deepEqual(storage.keys(), [expectedKey]);

  // HEAD hit preserves x-artifact-tag and Content-Length
  const headRes = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, { method: "HEAD", headers: authA });
  assert.equal(headRes.status, 200);
  assert.equal(headRes.headers.get("content-length"), String(artifact.length));
  assert.equal(headRes.headers.get("x-artifact-tag"), tag);

  // GET hit returns exact bytes and x-artifact-tag
  const getRes = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, { headers: authA });
  assert.equal(getRes.status, 200);
  assert.equal(getRes.headers.get("content-type"), "application/octet-stream");
  assert.equal(getRes.headers.get("x-artifact-tag"), tag);
  assert.deepEqual(Buffer.from(await getRes.arrayBuffer()), artifact);

  // Account B cannot access Account A's artifact
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, { headers: authB })).status, 404);

  // Invalid hash and invalid tag rejected with 400
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/short`, { headers: authA })).status, 400);
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/........`, { headers: authA })).status, 400);
  const badTag = await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/${hash}`, {
    method: "PUT",
    headers: { ...authA, "x-artifact-tag": "x".repeat(513) },
    body: artifact,
  });
  assert.equal(badTag.status, 400);
});

test("live stats & recent events exclude .sccache_check probes and respect ring buffer caps", async (t) => {
  const { url, handler } = await startCacheServer(t, { bucket: "prod-cache-bucket" });
  const authA = { Authorization: "Bearer token-a" };
  const authB = { Authorization: "Bearer token-b" };

  // .sccache_check probes should not affect hit/miss/write stats
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/.sccache_check`, { headers: authA })).status, 404);
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/.sccache_check`, { method: "PUT", headers: authA, body: Buffer.from("ok") })).status, 201);
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/.sccache_check`, { headers: authA })).status, 200);

  const afterProbe = handler.statsForAccount("account-a");
  assert.equal(afterProbe.enabled, true);
  assert.equal(afterProbe.bucket, "prod-cache-bucket");
  assert.equal(afterProbe.sccache.hits, 0);
  assert.equal(afterProbe.sccache.misses, 0);
  assert.equal(afterProbe.sccache.writes, 0);
  assert.equal(afterProbe.recent.length, 0);

  // Real sccache + turbo activity for account A
  const data = Buffer.from("hello-cache-1234");
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/0/1/obj1`, { headers: authA })).status, 404);
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/0/1/obj1`, { method: "PUT", headers: authA, body: data })).status, 201);
  assert.equal((await fetch(`${url}/api/rudder/cache/sccache/0/1/obj1`, { headers: authA })).status, 200);

  const turboData = Buffer.from("turbo-bin-xyz");
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/hash12345678`, { headers: authA })).status, 404);
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/hash12345678`, { method: "PUT", headers: authA, body: turboData })).status, 200);
  assert.equal((await fetch(`${url}/api/rudder/cache/turbo/v8/artifacts/hash12345678`, { headers: authA })).status, 200);

  const statsA = handler.statsForAccount("account-a");
  assert.equal(statsA.sccache.misses, 1);
  assert.equal(statsA.sccache.writes, 1);
  assert.equal(statsA.sccache.hits, 1);
  assert.equal(statsA.sccache.bytesWritten, data.length);
  assert.equal(statsA.sccache.bytesRead, data.length);
  assert.ok(statsA.sccache.lastActivityAt !== null);

  assert.equal(statsA.turbo.misses, 1);
  assert.equal(statsA.turbo.writes, 1);
  assert.equal(statsA.turbo.hits, 1);
  assert.equal(statsA.turbo.bytesWritten, turboData.length);
  assert.equal(statsA.turbo.bytesRead, turboData.length);
  assert.equal(statsA.recent.length, 6);

  // Unseen account returns zeroed stats without inflating accounts count
  const unseen = handler.statsForAccount("never-seen");
  assert.equal(unseen.sccache.hits, 0);
  assert.equal(unseen.turbo.hits, 0);
  assert.equal(handler.stats().accounts, 1);

  // Fill ring buffers beyond 30 per account and 50 globally
  for (let i = 0; i < 35; i++) {
    await fetch(`${url}/api/rudder/cache/sccache/a/miss-${i}`, { headers: authA });
  }
  for (let i = 0; i < 25; i++) {
    await fetch(`${url}/api/rudder/cache/sccache/b/miss-${i}`, { headers: authB });
  }
  assert.equal(handler.statsForAccount("account-a").recent.length, 30);
  assert.equal(handler.statsForAccount("account-b").recent.length, 25);
  const globalStats = handler.stats();
  assert.equal(globalStats.accounts, 2);
  assert.equal(globalStats.recent.length, 50);
  assert.equal(globalStats.sccache.misses, 1 + 35 + 25);
});

test("S3RemoteCacheStorage maps 404/NoSuchKey/NotFound and passes AbortSignal", async () => {
  const sent = [];
  const objects = new Map();
  const fakeClient = {
    async send(command, options) {
      sent.push({ name: command.constructor.name, input: command.input, hasSignal: options?.abortSignal instanceof AbortSignal });
      const key = command.input.Key;
      if (command.constructor.name === "PutObjectCommand") {
        objects.set(key, {
          body: Buffer.from(command.input.Body),
          contentType: command.input.ContentType,
          metadata: command.input.Metadata,
          etag: `"etag-${key}"`,
          lastModified: new Date("2026-11-24T00:00:00Z"),
        });
        return { ETag: `"etag-${key}"` };
      }
      if (command.constructor.name === "HeadObjectCommand") {
        const item = objects.get(key);
        if (!item) throw Object.assign(new Error("NotFound"), { name: "NotFound", $metadata: { httpStatusCode: 404 } });
        return { ContentLength: item.body.length, ContentType: item.contentType, ETag: item.etag, LastModified: item.lastModified, Metadata: item.metadata };
      }
      if (command.constructor.name === "GetObjectCommand") {
        const item = objects.get(key);
        if (!item) throw Object.assign(new Error("NoSuchKey"), { name: "NoSuchKey" });
        return {
          Body: Readable.from([item.body]),
          ContentLength: item.body.length,
          ContentType: item.contentType,
          ETag: item.etag,
          LastModified: item.lastModified,
          Metadata: item.metadata,
        };
      }
      if (command.constructor.name === "DeleteObjectCommand") {
        if (!objects.has(key)) throw Object.assign(new Error("NoSuchKey"), { name: "NoSuchKey", $metadata: { httpStatusCode: 404 } });
        objects.delete(key);
        return {};
      }
      throw new Error(`Unexpected command ${command.constructor.name}`);
    },
  };

  const s3Storage = new S3RemoteCacheStorage({ client: fakeClient, bucket: "test-bucket", timeoutMs: 5_000 });
  assert.equal(await s3Storage.get("missing", 1024), null);
  assert.equal(await s3Storage.head("missing"), null);
  await s3Storage.delete("missing");

  const put = await s3Storage.put("k1", Buffer.from("abc"), { contentType: "application/octet-stream", metadata: { "artifact-tag": "t1" } });
  assert.equal(put.etag, '"etag-k1"');

  const head = await s3Storage.head("k1");
  assert.equal(head?.size, 3);
  assert.equal(head?.metadata?.["artifact-tag"], "t1");

  const got = await s3Storage.get("k1", 1024);
  assert.deepEqual(got?.body, Buffer.from("abc"));
  assert.equal(got?.metadata?.["artifact-tag"], "t1");

  await s3Storage.delete("k1");
  assert.equal(await s3Storage.get("k1", 1024), null);
  assert.ok(sent.every((entry) => entry.hasSignal));
});

test("fabric-http statsForAccount and stats track gets, puts, rootsPublished, bytes and errors", async (t) => {
  const store = new Map();
  const storage = {
    async get(key) {
      const entry = store.get(key);
      return entry ? { body: Buffer.from(entry.body), etag: entry.etag } : null;
    },
    async putIfAbsent(key, bytes) {
      if (store.has(key)) return false;
      store.set(key, { body: Buffer.from(bytes), etag: sha256(bytes) });
      return true;
    },
    async replaceIfMatch(key, etag, bytes) {
      const entry = store.get(key);
      if (!entry || entry.etag !== etag) return false;
      store.set(key, { body: Buffer.from(bytes), etag: sha256(bytes) });
      return true;
    },
    async delete(key) {
      store.delete(key);
    },
  };

  const verificationCache = new FabricVerificationCache(1024 * 1024);
  const handler = createFabricHttpHandler({
    storage,
    verificationCache,
    authenticate: (req) => {
      if (req.headers.authorization === "Bearer acct-a") return "acct-a";
      if (req.headers.authorization === "Bearer acct-b") return "acct-b";
      throw Object.assign(new Error("nope"), { status: 401 });
    },
  });

  const server = http.createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://127.0.0.1");
    void handler(req, res, url);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  });
  const base = `http://127.0.0.1:${server.address().port}/api/rudder/fabric`;
  const authA = { Authorization: "Bearer acct-a" };

  // Begin staging
  const stageRes = await fetch(`${base}/staging`, {
    method: "POST",
    headers: { ...authA, "Content-Type": "application/json" },
    body: JSON.stringify({ format_version: 1, ttl_ms: 60_000 }),
  });
  assert.equal(stageRes.status, 201);
  const { lease_id } = await stageRes.json();

  // Put a blob + directory referencing it, then publish a root
  const blobBytes = Buffer.from("fabric-blob-content");
  const blobRef = { kind: "blob", algorithm: "sha256", digest: sha256(blobBytes), size_bytes: blobBytes.length };
  const dirBytes = canonicalFabricMetadata({
    format_version: 1,
    mode: 0o755,
    entries: [{ name_hex: Buffer.from("f.txt").toString("hex"), node: { type: "file", object: blobRef, mode: 0o644 } }],
  });
  const dirRef = { kind: "directory", algorithm: "sha256", digest: sha256(dirBytes), size_bytes: dirBytes.length };

  for (const [refObj, bytes] of [[blobRef, blobBytes], [dirRef, dirBytes]]) {
    const put = await fetch(`${base}/objects/${refObj.kind}/${refObj.digest}/${refObj.size_bytes}`, {
      method: "PUT",
      headers: { ...authA, "Content-Length": String(bytes.length), "X-Rudder-Staging-Lease": lease_id },
      body: bytes,
    });
    assert.equal(put.status, 200);
  }

  const pub = await fetch(`${base}/roots/test-root`, {
    method: "POST",
    headers: { ...authA, "Content-Type": "application/json" },
    body: JSON.stringify({ format_version: 1, lease_id, root: dirRef }),
  });
  assert.equal(pub.status, 200);

  // Get both objects back
  for (const [refObj, bytes] of [[blobRef, blobBytes], [dirRef, dirBytes]]) {
    const get = await fetch(`${base}/objects/${refObj.kind}/${refObj.digest}/${refObj.size_bytes}`, { headers: authA });
    assert.equal(get.status, 200);
    assert.deepEqual(Buffer.from(await get.arrayBuffer()), bytes);
  }

  // Trigger an authenticated error (404 missing root) and an unauthenticated error (401)
  assert.equal((await fetch(`${base}/roots/does-not-exist`, { headers: authA })).status, 404);
  assert.equal((await fetch(`${base}/roots/test-root`)).status, 401);

  const statsA = handler.statsForAccount("acct-a");
  assert.equal(statsA.puts, 2);
  assert.equal(statsA.gets, 2);
  assert.equal(statsA.rootsPublished, 1);
  assert.equal(statsA.bytesWritten, blobBytes.length + dirBytes.length);
  assert.equal(statsA.bytesRead, blobBytes.length + dirBytes.length);
  assert.equal(statsA.errors, 1);
  assert.equal(statsA.requests, 7);
  assert.ok(statsA.lastActivityAt !== null);
  assert.ok(statsA.verificationCache && statsA.verificationCache.entries >= 1);

  const statsB = handler.statsForAccount("acct-b");
  assert.equal(statsB.requests, 0);
  assert.equal(statsB.puts, 0);

  const globalStats = handler.stats();
  assert.equal(globalStats.accounts, 1);
  assert.equal(globalStats.puts, 2);
  assert.equal(globalStats.gets, 2);
  assert.equal(globalStats.rootsPublished, 1);
  assert.equal(globalStats.errors, 2);
  assert.equal(globalStats.requests, 8);
  assert.ok(globalStats.verificationCache && globalStats.verificationCache.entries >= 1);
});

test("remote build cache enforces concurrency limits (429) and MemoryRemoteCacheStorage maxBytes eviction", async (t) => {
  // MemoryRemoteCacheStorage maxBytes cap and LRU eviction
  const mem = new MemoryRemoteCacheStorage({ maxBytes: 10 });
  await mem.put("k1", Buffer.from("12345"));
  await mem.put("k2", Buffer.from("67890"));
  assert.ok(await mem.get("k1", 10)); // touch k1 so k2 is oldest
  await mem.put("k3", Buffer.from("abcde")); // evicts k2
  assert.ok(await mem.head("k1"));
  assert.equal(await mem.head("k2"), null);
  assert.ok(await mem.head("k3"));
  await assert.rejects(() => mem.put("too-large", Buffer.alloc(11)), (err) => err.status === 413);

  // Per-account concurrency limit (16 -> 17th returns 429)
  let releaseAll;
  const gate = new Promise((resolve) => { releaseAll = resolve; });
  let inFlightHeads = 0;
  let allBlocked;
  const reached16 = new Promise((resolve) => { allBlocked = resolve; });
  const blockingStorage = {
    async get() { return null; },
    async head() {
      inFlightHeads++;
      if (inFlightHeads === 16) allBlocked();
      await gate;
      return null;
    },
    async put() { return {}; },
    async delete() {},
  };

  const { url } = await startCacheServer(t, { storage: blockingStorage });
  const authA = { Authorization: "Bearer token-a" };
  const pending = [];
  for (let i = 0; i < 16; i++) {
    pending.push(fetch(`${url}/api/rudder/cache/sccache/0/item-${i}`, { method: "HEAD", headers: authA }));
  }
  await reached16;
  const res429 = await fetch(`${url}/api/rudder/cache/sccache/0/overflow`, { method: "HEAD", headers: authA });
  assert.equal(res429.status, 429);
  releaseAll();
  const settled = await Promise.all(pending);
  assert.ok(settled.every((res) => res.status === 404));
});

test("real sccache 0.18.0 multilevel (disk,webdav) populates and hits our remote WebDAV cache", { timeout: 60_000 }, async (t) => {
  try {
    await execFileAsync("sccache", ["--version"]);
    await execFileAsync("rustc", ["--version"]);
  } catch {
    t.skip("sccache and rustc are required for end-to-end multilevel cache test");
    return;
  }

  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "sc-"));
  t.after(() => fs.rm(tmpDir, { recursive: true, force: true }));

  const { port, handler } = await startCacheServer(t, { bucket: "e2e-webdav-bucket" });
  const srcFile = path.join(tmpDir, "lib.rs");
  const outDir = path.join(tmpDir, "out");
  const outFile = path.join(outDir, "librudder_cache_probe.rlib");
  await fs.mkdir(outDir, { recursive: true });
  await fs.writeFile(srcFile, "pub fn rudder_add(a: i32, b: i32) -> i32 { a + b }\n", "utf8");

  const makeSccacheEnv = (diskDir, udsName) => {
    const env = { ...process.env };
    for (const key of Object.keys(env)) {
      if (key.startsWith("SCCACHE_")) delete env[key];
    }
    return {
      ...env,
      SCCACHE_MULTILEVEL_CHAIN: "disk,webdav",
      SCCACHE_DIR: diskDir,
      SCCACHE_WEBDAV_ENDPOINT: `http://127.0.0.1:${port}/api/rudder/cache/sccache`,
      SCCACHE_WEBDAV_TOKEN: "test-token",
      SCCACHE_WEBDAV_RW_MODE: "READ_WRITE",
      SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY: "ignore",
      SCCACHE_SERVER_UDS: path.join(tmpDir, udsName),
    };
  };

  const stopServer = async (env) => {
    await execFileAsync("sccache", ["--stop-server"], { env }).catch(() => {});
  };

  const rustcArgs = [
    "rustc",
    "--crate-type",
    "lib",
    "--crate-name",
    "rudder_cache_probe",
    "--emit=link",
    "--out-dir",
    outDir,
    srcFile,
  ];

  // Phase 1: cold compile populates local disk-1 and remote WebDAV cache
  const disk1 = path.join(tmpDir, "d1");
  const env1 = makeSccacheEnv(disk1, `1-${randomUUID().slice(0, 6)}.sock`);
  t.after(() => stopServer(env1));

  await execFileAsync("sccache", rustcArgs, { env: env1 });
  await stopServer(env1);

  const afterFirst = handler.statsForAccount("sccache-acct");
  assert.ok(afterFirst.sccache.writes >= 1, `Expected remote WebDAV write >= 1, got ${JSON.stringify(afterFirst.sccache)}`);
  assert.ok(afterFirst.sccache.bytesWritten > 0);
  assert.equal(afterFirst.sccache.errors, 0);

  // Phase 2: wipe output + disk-1, use brand-new empty disk-2 and fresh sccache server UDS
  await fs.rm(outDir, { recursive: true, force: true });
  await fs.mkdir(outDir, { recursive: true });
  await fs.rm(disk1, { recursive: true, force: true });

  const disk2 = path.join(tmpDir, "d2");
  const env2 = makeSccacheEnv(disk2, `2-${randomUUID().slice(0, 6)}.sock`);
  t.after(() => stopServer(env2));

  await execFileAsync("sccache", rustcArgs, { env: env2 });
  const { stdout: statsOut } = await execFileAsync("sccache", ["--show-stats"], { env: env2 });
  await stopServer(env2);

  const stat = await fs.stat(outFile);
  assert.ok(stat.size > 0, "Expected compiled artifact from remote WebDAV cache hit");
  assert.match(statsOut, /L1 \(webdav\) hits\s+1\b/);

  const afterSecond = handler.statsForAccount("sccache-acct");
  assert.ok(
    afterSecond.sccache.hits >= 1,
    `Expected remote WebDAV hit >= 1, got ${JSON.stringify(afterSecond.sccache)}\nsccache stats:\n${statsOut}`,
  );
  assert.ok(afterSecond.sccache.bytesRead > 0);
  assert.equal(afterSecond.sccache.errors, 0);
});
