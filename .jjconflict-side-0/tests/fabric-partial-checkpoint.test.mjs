import test from "node:test";
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { FabricRemoteClient, validateFabricPartialCheckpointInventory } from "../dist/fabric-remote.js";
import { FabricLocalClient } from "../dist/fabric-local.js";

const identity = (ref) => `${ref.kind}:${ref.digest}:${ref.size_bytes}`;
const deferred = () => { let resolve; const promise = new Promise((done) => { resolve = done; }); return { promise, resolve }; };
const item = (kind, text) => {
  const bytes = Buffer.from(text);
  return { bytes, object: { kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length } };
};

// This fixture exercises the transfer/retention boundary. Native owns receipt
// validation; separate server tests own canonical metadata and graph semantics.
function fixture(options = {}) {
  const q = item("blob", "checkpoint audit"), plan = item("blob", "capture plan");
  const freeze = item("blob", "freeze audit"), drain = item("blob", "drain audit");
  const source = item("source_root", "new source"), runnable = item("runnable_root", "new runnable");
  const publication = item("action_result", "complete checkpoint data envelope");
  const inherited = item("blob", "inherited payload must stay remote");
  const all = [q, plan, freeze, drain, source, runnable, publication];
  const local = new Map(all.map((value) => [identity(value.object), value.bytes]));
  const base = { scope: { provider_id: "fly", tenant_id: "fixture" }, root_id: "base", root: item("runnable_root", "base runnable").object };
  const publicationId = `pcd1-${"a".repeat(64)}`;
  const pins = new Set(), reads = [], receipts = [], requests = [], stored = new Map();
  const rootReceipt = (id, root) => ({ format_version: 1, root_id: id, root, durability: "replicated", published_at_ms: 1, object_count: 7, total_bytes: 1000 });
  const roots = new Map([["base", rootReceipt("base", base.root)]]);
  const inventory = { format_version: 1, owned_pin: "", publication_id: publicationId, publication: publication.object,
    q: q.object, source: source.object, runnable: runnable.object, plan: plan.object,
    freeze_intent: freeze.object, drain_record: drain.object, base, objects: all.map((value) => value.object) };
  const sourceAdapter = {
    async pinPartialCheckpoint(receipt) {
      receipts.push(receipt); const pin = `transfer:partial-checkpoint:rpc:${randomUUID()}`; pins.add(pin);
      return { ...structuredClone(inventory), owned_pin: pin, ...options.inventory };
    },
    async unpin(pin) { assert(pins.delete(pin), "release only this transfer's clone"); },
    async read(reference) {
      assert.equal(pins.size, 1); reads.push(reference);
      assert.notEqual(reference.digest, inherited.object.digest);
      const bytes = local.get(identity(reference)); assert(bytes, "do not walk missing inherited closure"); return bytes;
    },
  };
  const fetch = async (url, init) => {
    const path = url.pathname.replace("/api/rudder/fabric/", ""); const method = init.method;
    requests.push({ method, path });
    const body = init.headers["Content-Type"] === "application/json" ? JSON.parse(init.body) : null;
    if (path.startsWith("roots/") && method === "GET") {
      const receipt = roots.get(path.slice(6)); return receipt ? Response.json(receipt) : new Response(null, { status: 404 });
    }
    if (path === "staging" || path.endsWith("/renew")) return Response.json({ format_version: 1, lease_id: path.endsWith("/renew") ? path.split("/")[1] : randomUUID(), expires_at_ms: Date.now() + 300_000 });
    if (path.startsWith("staging/") && method === "DELETE") return new Response(null, { status: 204 });
    if (path === "missing") return Response.json({ format_version: 1, missing: body.objects.filter((ref) => !stored.has(identity(ref))) });
    if (path.startsWith("objects/") && method === "PUT") {
      const [, kind, digest, size] = path.split("/"); stored.set(`${kind}:${digest}:${size}`, Buffer.from(init.body)); return Response.json({ stored: true });
    }
    if (path.startsWith("roots/") && method === "POST") {
      const id = path.slice(6), receipt = rootReceipt(id, body.root); roots.set(id, receipt);
      if (options.loseReply && !options.lost) { options.lost = true; throw new Error("lost publication reply"); }
      return Response.json(receipt);
    }
    throw new Error("unexpected fixture request");
  };
  const client = new FabricRemoteClient({ baseUrl: "http://127.0.0.1/api/rudder/fabric", authHeaders: () => ({}), fetch, concurrency: 2, scope: options.scope ?? base.scope });
  return { q, plan, freeze, drain, source, runnable, publication, inherited, inventory, publicationId, sourceAdapter, pins, reads, receipts, requests, roots, client };
}

test("checkpoint data transfer preserves opaque u64 receipts and returns a distinct proof-bundle receipt", async () => {
  const f = fixture(), opaque = '{"generation":18446744073709551615}';
  const result = await f.client.uploadPartialCheckpoint(f.publicationId, opaque, f.sourceAdapter);
  assert.deepEqual(f.receipts, [opaque]); assert.equal(f.pins.size, 0);
  assert.equal(result.kind, "partial_checkpoint_data"); assert.equal(result.source_role, "quiesced_source_complement_data");
  assert.deepEqual(result.bundle.root, f.publication.object); assert.equal(result.bundle.root.kind, "action_result");
  assert.equal(Object.hasOwn(result, "receipt"), false, "do not masquerade as the existing runnable transfer result");
  assert.deepEqual(result.runnable, f.runnable.object); assert.equal(result.uploaded_objects, 7);
  assert.deepEqual(new Set(f.reads.map(identity)), new Set(f.inventory.objects.map(identity)));
});

test("checkpoint proof omissions, duplicate objects and foreign identities fail before network writes", async () => {
  for (const field of ["publication", "q", "source", "runnable", "plan", "freeze_intent", "drain_record"]) {
    const f = fixture(); f.inventory.objects = f.inventory.objects.filter((ref) => identity(ref) !== identity(f.inventory[field]));
    await assert.rejects(f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter), { code: "invalid_source" });
    assert.equal(f.requests.length, 0); assert.equal(f.pins.size, 0);
  }
  const duplicate = fixture(); duplicate.inventory.objects.push(duplicate.inventory.objects[0]);
  await assert.rejects(duplicate.client.uploadPartialCheckpoint(duplicate.publicationId, "{}", duplicate.sourceAdapter), { code: "invalid_source" });
  const foreign = fixture({ scope: { provider_id: "fly", tenant_id: "other" } });
  await assert.rejects(foreign.client.uploadPartialCheckpoint(foreign.publicationId, "{}", foreign.sourceAdapter), { code: "scope_mismatch" });
  assert.equal(foreign.requests.length, 0); assert.equal(foreign.pins.size, 0);
  const identityMismatch = fixture();
  await assert.rejects(identityMismatch.client.uploadPartialCheckpoint(`pcd1-${"b".repeat(64)}`, "{}", identityMismatch.sourceAdapter), { code: "publication_mismatch" });
  assert.equal(identityMismatch.requests.length, 0); assert.equal(identityMismatch.pins.size, 0);
});

test("lost checkpoint publication reply reconciles the exact committed ID without another POST or source read", async () => {
  const f = fixture({ loseReply: true });
  await assert.rejects(f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter), { code: "request_failed" });
  assert.equal(f.pins.size, 0); const reads = f.reads.length, posts = f.requests.filter((r) => r.method === "POST").length;
  const result = await f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter);
  assert.deepEqual(result.bundle, f.roots.get(f.publicationId)); assert.equal(result.uploaded_objects, 0);
  assert.equal(f.requests.filter((r) => r.method === "POST").length, posts); assert.equal(f.reads.length, reads);
  f.roots.set(f.publicationId, { ...result.bundle, root: f.runnable.object });
  await assert.rejects(f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter), { code: "publication_mismatch" });
  assert.equal(f.pins.size, 0);
});

test("failed checkpoint upload holds its clone until every admitted source reader actually settles", async () => {
  const f = fixture(), first = deferred(), failed = deferred(), release = deferred(); let calls = 0, active = 0;
  const read = f.sourceAdapter.read, unpin = f.sourceAdapter.unpin;
  f.sourceAdapter.read = async (ref) => {
    const number = ++calls; active++;
    try {
      if (number === 1) { first.resolve(); await release.promise; return await read(ref); }
      await first.promise; failed.resolve(); throw new Error("synthetic source failure");
    } finally { active--; }
  };
  f.sourceAdapter.unpin = async (pin) => { assert.equal(active, 0); await unpin(pin); };
  const result = f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter);
  await failed.promise; assert.equal(f.pins.size, 1); assert.equal(active, 1);
  release.resolve(); await assert.rejects(result, { code: "source_failed" });
  assert.equal(f.pins.size, 0); assert.equal(f.roots.has(f.publicationId), false);
});

test("checkpoint inventory validation preserves the declared data boundary and exact supported fields", () => {
  const f = fixture(); const inventory = { ...f.inventory, owned_pin: `transfer:partial-checkpoint:rpc:${randomUUID()}` };
  assert.deepEqual(validateFabricPartialCheckpointInventory(inventory), inventory);
  for (const patch of [{ extra: true }, { publication: f.runnable.object }, { owned_pin: `transfer:partial:rpc:${randomUUID()}` }, { publication_id: "caller-selected" }]) {
    assert.throws(() => validateFabricPartialCheckpointInventory({ ...inventory, ...patch }));
  }
});

test("checkpoint cleanup keeps the issued pin identity when an adapter mutates its returned inventory", async () => {
  const f = fixture(), other = `transfer:partial-checkpoint:rpc:${randomUUID()}`;
  f.pins.add(other); let admitted, issued; const released = [];
  const mint = f.sourceAdapter.pinPartialCheckpoint, unpin = f.sourceAdapter.unpin, get = f.client.getRoot.bind(f.client);
  f.sourceAdapter.pinPartialCheckpoint = async (receipt) => { admitted = await mint(receipt); issued = admitted.owned_pin; return admitted; };
  f.sourceAdapter.unpin = async (pin) => { released.push(pin); await unpin(pin); };
  f.client.getRoot = async (id) => { admitted.owned_pin = other; return get(id); };
  f.roots.set(f.publicationId, { format_version: 1, root_id: f.publicationId, root: f.publication.object,
    durability: "replicated", published_at_ms: 1, object_count: 7, total_bytes: 1000 });
  const result = await f.client.uploadPartialCheckpoint(f.publicationId, "{}", f.sourceAdapter);
  assert.equal(result.uploaded_objects, 0); assert.deepEqual(released, [issued]);
  assert.deepEqual([...f.pins], [other], "another held clone must survive cleanup");
});

async function malformedNative(t, { cleanupFails = false, invalidPrefix = false, legacy = false } = {}) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-pcd-pin-"));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const f = fixture(), issued = `${invalidPrefix ? "transfer:other:rpc:" : legacy ? "transfer:partial:rpc:" : "transfer:partial-checkpoint:rpc:"}${randomUUID()}`;
  const inventory = legacy ? { format_version: 1, owned_pin: issued, plan: f.plan.object, directory: item("directory", "fixture").object, hardlinks: f.plan.object, base: f.inventory.base, objects: [] } : { ...f.inventory, owned_pin: issued, objects: [] };
  const binary = path.join(directory, "fake-native.mjs");
  await fs.writeFile(binary, `#!${process.execPath}
import {createInterface} from 'node:readline';
const inventory=${JSON.stringify(inventory)};let held=false;const releases=[];
for await(const line of createInterface({input:process.stdin,crlfDelay:Infinity})){
 const f=JSON.parse(line),r=f.request;let result,ok=true;
 if(r.operation==='store'&&r.request.operation==='capabilities')result={format_version:1};
 else if(r.operation===${JSON.stringify(legacy ? "pin_partial_source" : "pin_partial_checkpoint")}){held=true;result=inventory;}
 else if(r.operation==='store'&&r.request.operation==='retention')result={held,releases};
 else if(r.operation==='unpin_source'){releases.push(r.pin);if(${cleanupFails}){ok=false;result={};}else{held=false;result={released:true};}}
 else throw Error('unexpected fixture frame');
 process.stdout.write(JSON.stringify({format_version:1,id:f.id,ok,result,...(!ok?{error:{code:'synthetic_cleanup_failure'}}:{}),binary_bytes:0})+'\\n');
}
`, { mode: 0o700 });
  const local = await FabricLocalClient.open({ nativeBinary: binary, env: { PATH: path.dirname(process.execPath) + path.delimiter + "/usr/bin:/bin" }, requestTimeoutMs: 2000 });
  t.after(() => local.close());
  return { local, issued };
}

test("malformed native checkpoint inventory releases its fresh admitted clone before returning failure", async (t) => {
  const f = await malformedNative(t);
  await assert.rejects(f.local.pinPartialCheckpoint('{"generation":18446744073709551615}'), { code: "invalid_source" });
  assert.deepEqual(await f.local.request({ operation: "retention" }), { held: false, releases: [f.issued] });
  await assert.rejects(f.local.unpin(f.issued), { code: "invalid_pin" });
});

test("uncertain native cleanup stays visible and invalid pin namespaces are never released", async (t) => {
  const pending = await malformedNative(t, { cleanupFails: true });
  await assert.rejects(pending.local.pinPartialCheckpoint("{}"), { code: "source_cleanup_pending" });
  assert.deepEqual(await pending.local.request({ operation: "retention" }), { held: true, releases: [pending.issued] });
  const invalid = await malformedNative(t, { invalidPrefix: true });
  await assert.rejects(invalid.local.pinPartialCheckpoint("{}"), { code: "invalid_response" });
  assert.deepEqual(await invalid.local.request({ operation: "retention" }), { held: true, releases: [] });
});

test("legacy partial inventory uses the same exact pin custody on malformed replies", async (t) => {
  const f = await malformedNative(t, { legacy: true });
  await assert.rejects(f.local.pinPartial("{}"), { code: "invalid_source" });
  assert.deepEqual(await f.local.request({ operation: "retention" }), { held: false, releases: [f.issued] });
});
