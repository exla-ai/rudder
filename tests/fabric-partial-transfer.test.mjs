import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { createHash, randomUUID } from "node:crypto";
import { FabricRemoteClient } from "../dist/fabric-remote.js";
import { FabricCas, MemoryFabricObjectStorage, canonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";
import { createFabricHttpHandler } from "../cloud/dist/fabric-http.js";

const identity = (object) => `${object.kind}:${object.digest}:${object.size_bytes}`;
const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
const blob = (text) => { const bytes = Buffer.from(text); return { bytes, object: ref("blob", bytes) }; };
const meta = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { bytes, object: ref(kind, bytes) }; };
const directory = (files) => meta("directory", { format_version: 1, mode: 493, entries: files.map(([name, file]) => ({ name_hex: Buffer.from(name).toString("hex"), node: { type: "file", mode: 420, object: file.object } })) });

async function fixture(t, options = {}) {
  const storage = new MemoryFabricObjectStorage();
  const cas = new FabricCas({ storage, accountId: "account-a" });
  const old = blob("unchanged inherited data"), changed = blob("new local data");
  const before = directory([["old", old]]), after = directory([["changed", changed], ["old", old]]);
  const policy = blob('{"source":"all"}'), profile = blob('{"profile":"test"}'), hardlinks = blob('{"format_version":1,"groups":[]}');
  const source = meta("source_root", { format_version: 2, consistency: "quiesced", directory: before.object, projection_policy: policy.object });
  const environment = meta("environment_root", { format_version: 2, directory: before.object, platform: { os: "linux", arch: "x86_64" }, installer_inputs: [policy.object], provenance: {} });
  const base = meta("runnable_root", { format_version: 1, consistency: "quiesced", source: source.object, environment: environment.object, view_root: before.object, profile: profile.object, hardlinks: hardlinks.object, knowledge_cursor: 1 });
  const seed = await cas.beginStaging();
  for (const item of [old, before, policy, profile, hardlinks, source, environment, base]) await cas.putObject(seed.lease_id, item.object, item.bytes);
  await cas.publishRoot("base", seed.lease_id, base.object);
  // Rust tests establish provenance and retention. This source fixture exposes
  // the resulting trusted transfer inventory, never an arbitrary closure walk.
  const plan = blob("opaque native-validated partial plan");
  const localObjects = new Map([changed, after, plan].map((item) => [identity(item.object), item]));
  const binding = { scope: { provider_id: "fly", tenant_id: "account-a" }, root_id: "base", root: base.object };
  const requests = [], pins = new Set(), reads = [], receivedReceipts = [];
  const sourceAdapter = {
    async pinPartial(receiptJson) {
      receivedReceipts.push(receiptJson);
      const owned_pin = `transfer:partial:rpc:${randomUUID()}`; pins.add(owned_pin);
      return { format_version: 1, owned_pin, plan: plan.object, directory: after.object, hardlinks: hardlinks.object, base: structuredClone(binding), objects: [...localObjects.values()].map((item) => item.object), ...options.inventory };
    },
    async unpin(pin) { assert(pins.delete(pin), "only an owned transfer pin may be removed"); },
    async read(object) {
      reads.push(object); const item = localObjects.get(identity(object));
      assert(item, "unchanged inherited data must never be read locally");
      return options.corrupt && object.digest === changed.object.digest ? Buffer.from("corrupted") : item.bytes;
    },
  };
  const handler = createFabricHttpHandler({ storage, authenticate: (request) => {
    if (request.headers.authorization !== "Bearer fixture") throw Object.assign(new Error("private auth"), { status: 401 });
    return "account-a";
  } });
  const server = http.createServer((request, response) => { requests.push({ method: request.method, path: request.url }); void handler(request, response, new URL(request.url, "http://127.0.0.1")); });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); });
  const clientOptions = { baseUrl: `http://127.0.0.1:${server.address().port}/api/rudder/fabric`, authHeaders: () => ({ Authorization: "Bearer fixture" }), scope: binding.scope, concurrency: 2, ...options.client };
  return { cas, storage, binding, old, changed, after, base, plan, requests, pins, reads, receivedReceipts, source: sourceAdapter, remote: new FabricRemoteClient(clientOptions), clientOptions };
}

test("partial upload sends only new retained units and verifies the complete remote directory", async (t) => {
  const f = await fixture(t);
  const opaque = '{"generation":18446744073709551615}';
  const result = await f.remote.uploadPartialTree("changed-tree", opaque, f.source);
  assert.equal(result.receipt.durability, "replicated");
  assert.deepEqual(result.receipt.root, f.after.object);
  assert.equal(result.uploaded_objects, 3);
  assert.equal(result.receipt.object_count, 3, "directory, inherited bytes and new bytes are verified");
  assert.deepEqual(f.receivedReceipts, [opaque], "the host must not round native u64 receipt fields");
  assert.equal(f.pins.size, 0);
  assert.equal(f.reads.some((object) => object.digest === f.old.object.digest), false);
  assert.equal([...f.storage.objects.keys()].some((key) => key.includes("/staging/")), false);
  const retry = await f.remote.uploadPartialTree("changed-tree", opaque, f.source);
  assert.equal(retry.uploaded_objects, 0); assert.deepEqual(retry.receipt, result.receipt);
});

test("partial transfer rejects another configured provider or tenant before network work", async (t) => {
  const f = await fixture(t, { client: { scope: { provider_id: "fly", tenant_id: "account-b" } } });
  await assert.rejects(f.remote.uploadPartialTree("wrong-scope", "{}", f.source), { code: "scope_mismatch" });
  assert.equal(f.requests.length, 0); assert.equal(f.pins.size, 0); assert.equal(f.reads.length, 0);
});

test("partial transfer requires trusted scope and exact immutable base identity", async (t) => {
  const f = await fixture(t);
  const unscoped = new FabricRemoteClient({ ...f.clientOptions, scope: undefined });
  await assert.rejects(unscoped.uploadPartialTree("unscoped", "{}", f.source), { code: "scope_required" });
  assert.equal(f.receivedReceipts.length, 0);
  f.binding.root = { ...f.base.object, digest: "f".repeat(64) };
  await assert.rejects(f.remote.uploadPartialTree("changed-base", "{}", f.source), { code: "base_mismatch" });
  assert.equal(f.pins.size, 0); assert.equal(f.reads.length, 0);
  assert(f.requests.every((request) => request.method === "GET"));
});

test("corrupt new bytes never publish and all admitted readers settle before retention cleanup", async (t) => {
  const f = await fixture(t, { corrupt: true }); let active = 0;
  const read = f.source.read;
  f.source.read = async (object) => { active++; try { await new Promise((resolve) => setTimeout(resolve, 15)); return await read(object); } finally { active--; } };
  const unpin = f.source.unpin;
  f.source.unpin = async (pin) => { assert.equal(active, 0); return unpin(pin); };
  await assert.rejects(f.remote.uploadPartialTree("corrupt", "{}", f.source), { code: "checksum_mismatch" });
  assert.equal(await f.cas.getRoot("corrupt"), null); assert.equal(f.pins.size, 0);
  assert.equal([...f.storage.objects.keys()].some((key) => key.includes("/staging/")), false);
});

test("partial upload cancels buffer waiters and keeps timed-out transport bytes charged until settlement", async (t) => {
  const f = await fixture(t); let releasePut, stalled = true, activeBytes = 0, maximumBytes = 0, putCalls = 0;
  const remote = new FabricRemoteClient({ ...f.clientOptions, requestTimeoutMs: 200, fetch: async (url, init) => {
    const response = await fetch(url, init);
    if (url.pathname.endsWith("/missing")) {
      const reply = await response.json(); reply.missing.sort((a, b) => b.size_bytes - a.size_bytes);
      return Response.json(reply);
    }
    if (init.method === "PUT") {
      putCalls++; activeBytes += init.body.byteLength; maximumBytes = Math.max(maximumBytes, activeBytes);
      try { if (stalled) await new Promise((resolve) => { releasePut = resolve; }); }
      finally { activeBytes -= init.body.byteLength; }
    }
    return response;
  } });
  // Lower only the fixture's private capacity so the real queue/ownership path
  // is exercised without allocating a 256 MiB payload in the parallel suite.
  const capacity = f.after.bytes.length; remote.buffered.available = capacity;
  t.after(() => releasePut?.());
  await assert.rejects(remote.uploadPartialTree("bounded", "{}", f.source), { code: "request_timeout" });
  assert.equal(putCalls, 1); assert.equal(activeBytes, capacity); assert.equal(remote.buffered.available, 0);
  assert.equal(remote.buffered.queue.length, 0); assert.equal(f.pins.size, 0); assert.equal(f.reads.length, 1);
  for (let count = 0; count < 2; count++) {
    await assert.rejects(remote.uploadPartialTree("bounded", "{}", f.source), { code: "request_timeout" });
    assert.equal(remote.buffered.queue.length, 0); assert.equal(f.pins.size, 0);
  }
  assert.equal(f.reads.length, 1, "cancelled waiters must never start another source read");
  assert.equal(putCalls, 1); assert.equal(maximumBytes, capacity);
  stalled = false; releasePut();
  for (let count = 0; count < 100 && remote.inFlight; count++) await new Promise((resolve) => setTimeout(resolve, 1));
  assert.equal(remote.buffered.available, capacity);
  const result = await remote.uploadPartialTree("bounded", "{}", f.source);
  assert.deepEqual(result.receipt.root, f.after.object); assert.equal(f.pins.size, 0);
  assert.equal(activeBytes, 0); assert(maximumBytes <= capacity);
});

test("a lost inherited object prevents a replicated receipt despite successful delta upload", async (t) => {
  const f = await fixture(t);
  const key = [...f.storage.objects.keys()].find((value) => value.endsWith(f.old.object.digest));
  await f.storage.delete(key);
  await assert.rejects(f.remote.uploadPartialTree("missing-base-object", "{}", f.source), { code: "remote_rejected" });
  assert.equal(await f.cas.getRoot("missing-base-object"), null); assert.equal(f.pins.size, 0);
  assert.equal([...f.storage.objects.keys()].some((value) => value.includes("/staging/")), false);
});

test("lost publication reply remains uncertain and retry reconciles the committed root", async (t) => {
  const f = await fixture(t); let lost = false;
  const remote = new FabricRemoteClient({ ...f.clientOptions, fetch: async (url, init) => {
    const response = await fetch(url, init);
    if (!lost && init.method === "POST" && url.pathname.endsWith("/uncertain")) { lost = true; await response.body?.cancel(); throw new Error("lost reply"); }
    return response;
  } });
  await assert.rejects(remote.uploadPartialTree("uncertain", "{}", f.source), { code: "request_failed" });
  assert.equal(f.pins.size, 0); const committed = await f.cas.getRoot("uncertain"); assert(committed);
  const retry = await remote.uploadPartialTree("uncertain", "{}", f.source);
  assert.equal(retry.uploaded_objects, 0); assert.deepEqual(retry.receipt, committed);
});

test("invalid partial inventory releases its allocated pin before any network work", async (t) => {
  const f = await fixture(t, { inventory: { objects: [] } });
  await assert.rejects(f.remote.uploadPartialTree("invalid", "{}", f.source), { code: "invalid_source" });
  assert.equal(f.pins.size, 0); assert.equal(f.requests.length, 0);
});

test("remote staging cleanup is idempotent and cannot delete another account's lease", async (t) => {
  const f = await fixture(t);
  const own = await f.remote.beginStaging();
  const other = new FabricCas({ storage: f.storage, accountId: "account-b" });
  const foreign = await other.beginStaging();
  await f.remote.abandonStaging(foreign.lease_id);
  assert.equal((await other.renewStaging(foreign.lease_id)).lease_id, foreign.lease_id);
  await f.remote.abandonStaging(own.lease_id);
  await f.remote.abandonStaging(own.lease_id);
  await assert.rejects(f.remote.renewStaging(own.lease_id), { code: "remote_rejected" });
  assert.deepEqual((await f.remote.getRoot("base")).root, f.base.object);
});
