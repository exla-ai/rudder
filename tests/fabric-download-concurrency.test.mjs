import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { createHash, randomUUID } from "node:crypto";
import { FabricRemoteClient } from "../dist/fabric-remote.js";
import { createFabricHttpHandler } from "../cloud/dist/fabric-http.js";
import { FabricCas, MemoryFabricObjectStorage, canonicalFabricMetadata, fabricReferences } from "../cloud/dist/fabric-cas.js";

const reference = (kind, bytes) => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
function deferred() { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; }
async function deadline(promise, message) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), 2_000); })]); }
  finally { clearTimeout(timer); }
}
function directory(nodes) {
  const bytes = canonicalFabricMetadata({ format_version: 1, mode: 0o755, entries: nodes.map((object, index) => ({ name_hex: Buffer.from(`entry-${index}`).toString("hex"), node: object.kind === "directory" ? { type: "directory", object } : { type: "file", object, mode: 0o644 } })) });
  return { object: reference("directory", bytes), bytes };
}
function objects() {
  const blobs = ["first child", "second child"].map((text) => { const bytes = Buffer.from(text); return { object: reference("blob", bytes), bytes }; });
  const nested = directory(blobs.map(({ object }) => object));
  const root = directory([nested.object]);
  return { root, all: [...blobs, nested, root] };
}
function target(onReceive = async () => {}) {
  const events = []; const received = new Map(); let active = 0;
  const staging = { format_version: 1, lease_id: randomUUID(), expires_at_ms: Date.now() + 300_000 };
  return {
    events, received,
    async beginStaging() { events.push("begin"); return staging; },
    async renewStaging() { return staging; },
    async receive(id, object, bytes) {
      assert.equal(id, staging.lease_id); active++;
      try { const children = fabricReferences(object, Buffer.from(bytes)); await onReceive(object); received.set(object.digest, bytes); return children; }
      finally { active--; }
    },
    async publish(id, pin, root) { assert.equal(id, staging.lease_id); assert.equal(active, 0); events.push("publish"); return { format_version: 1, root, pin, generation: 1, durability: "local" }; },
    async abandonStaging(id) { assert.equal(id, staging.lease_id); assert.equal(active, 0, "every receive must settle before staging is abandoned"); events.push("abandon"); },
  };
}
async function fixture(t, beforeGet = async () => {}) {
  const storage = new MemoryFabricObjectStorage();
  const { root, all } = objects();
  const cas = new FabricCas({ storage, accountId: "download-fixture" });
  const staging = await cas.beginStaging();
  for (const { object, bytes } of all) await cas.putObject(staging.lease_id, object, bytes);
  await cas.publishRoot("nested", staging.lease_id, root.object);
  const get = storage.get.bind(storage);
  storage.get = async (key, maxBytes) => { await beforeGet(key); return get(key, maxBytes); };
  const handler = createFabricHttpHandler({ storage, authenticate: (request) => { assert.equal(request.headers.authorization, "Bearer fixture-only"); return "download-fixture"; } });
  const server = http.createServer((request, response) => { void handler(request, response, new URL(request.url, "http://127.0.0.1")); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); });
  const client = new FabricRemoteClient({ baseUrl: `http://127.0.0.1:${server.address().port}/api/rudder/fabric`, authHeaders: () => ({ Authorization: "Bearer fixture-only" }), concurrency: 4 });
  return { client, root, all };
}

test("concurrency four keeps workers available through nested metadata discovery", async (t) => {
  const entered = deferred(); const release = deferred(); let active = 0, maximum = 0;
  const blobs = new Set(objects().all.filter(({ object }) => object.kind === "blob").map(({ object }) => object.digest));
  const { client, all } = await fixture(t, async (key) => {
    if (!blobs.has(key.split("/").at(-1))) return;
    active++; maximum = Math.max(maximum, active);
    if (active === 2) entered.resolve();
    try { await release.promise; } finally { active--; }
  });
  const local = target(); const pending = client.downloadRoot("nested", local, "download");
  try { await deadline(entered.promise, "discovered child downloads did not overlap"); }
  finally { release.resolve(); await pending.catch(() => {}); }
  const result = await pending;
  assert.equal(maximum, 2, "both HTTP child downloads overlap after two metadata levels");
  assert.equal(result.downloaded_objects, all.length); assert.equal(local.received.size, all.length);
  assert.deepEqual(local.events, ["begin", "publish"]);
});

test("a failed child waits for every in-flight receive before abandoning staging", async (t) => {
  const entered = deferred(); const fail = deferred(); const finish = deferred();
  let children = 0;
  const local = target(async (object) => {
    if (object.kind !== "blob") return;
    const child = ++children; if (children === 2) entered.resolve();
    if (child === 1) { await fail.promise; throw new Error("private target error"); }
    await finish.promise;
  });
  const { client } = await fixture(t); let settled = false;
  const pending = client.downloadRoot("nested", local, "download").finally(() => { settled = true; });
  const rejected = assert.rejects(pending, (error) => error.code === "target_failed" && !error.message.includes("private"));
  try {
    await deadline(entered.promise, "both discovered children did not reach the target");
    fail.resolve(); await new Promise((resolve) => setImmediate(resolve));
    assert.equal(settled, false); assert.deepEqual(local.events, ["begin"]);
  } finally { fail.resolve(); finish.resolve(); await rejected; }
  assert.deepEqual(local.events, ["begin", "abandon"]);
});

test("failure wakes byte-budget waiters without starting another object GET", async () => {
  const large = ["a", "b"].map((digit) => ({ kind: "blob", algorithm: "sha256", digest: digit.repeat(64), size_bytes: 160 * 1024 * 1024 }));
  const root = directory(large); const entered = deferred(); const fail = deferred(); let gets = 0;
  const client = new FabricRemoteClient({
    baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => ({}), concurrency: 4,
    fetch: async (url) => {
      if (url.pathname.includes("/roots/")) return Response.json({ format_version: 1, root_id: "large", root: root.object, durability: "replicated", published_at_ms: Date.now(), object_count: 3, total_bytes: root.bytes.length + 320 * 1024 * 1024 });
      if (url.pathname.includes(root.object.digest)) return new Response(root.bytes);
      gets++; entered.resolve(); await fail.promise; return new Response("", { status: 503 });
    },
  });
  const local = target(); const pending = client.downloadRoot("large", local, "download");
  const rejected = assert.rejects(pending, { code: "remote_rejected", status: 503 });
  try { await deadline(entered.promise, "first large object GET did not start"); assert.equal(gets, 1); }
  finally { fail.resolve(); await rejected; }
  assert.equal(gets, 1, "the 256 MiB budget waiter must not start after failure");
  assert.deepEqual(local.events, ["begin", "abandon"]);
});
