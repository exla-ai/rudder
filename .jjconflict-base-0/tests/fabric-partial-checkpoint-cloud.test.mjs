import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { ContentFormatError, validateContentReference } from "../cloud/dist/fabric-content.js";
import { partialCheckpointDataReferences, validatePartialCheckpointDataChildren } from "../cloud/dist/fabric-partial-checkpoint.js";
import { canonicalFabricMetadata, parseCanonicalFabricMetadata, fabricReferences, FabricCas, FabricVerificationCache } from "../cloud/dist/fabric-cas.js";

const fixtureBytes = await readFile(new URL("./fixtures/partial-checkpoint-v1.json", import.meta.url));
const fixture = JSON.parse(fixtureBytes.toString());
const codec = { parseCanonical: parseCanonicalFabricMetadata, encodeCanonical: canonicalFabricMetadata };
const hash = bytes => createHash("sha256").update(bytes).digest("hex");
const clone = value => codec.parseCanonical(codec.encodeCanonical(value));
const key = r => `${r.kind}:${r.digest}:${r.size_bytes}`;
const kinds = ["blob", "file", "directory", "directory_shard", "source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result"];
const compare = (a, b) => kinds.indexOf(a.kind) - kinds.indexOf(b.kind) || (a.digest < b.digest ? -1 : a.digest > b.digest ? 1 : a.size_bytes < b.size_bytes ? -1 : a.size_bytes > b.size_bytes ? 1 : 0);
const ordered = refs => [...refs].sort(compare).filter((r, i, all) => !i || key(r) !== key(all[i - 1]));
const wire = r => ({ ...r, size_bytes: Number(r.size_bytes) });
function make() {
  const graph = new Map([...fixture.base_objects, ...fixture.objects].map(o => [key(o.reference), Buffer.from(o.hex, "hex")]));
  const s = { graph, envelope: codec.parseCanonical(Buffer.from(fixture.envelope_json)), q: codec.parseCanonical(Buffer.from(fixture.q_json)), plan: codec.parseCanonical(Buffer.from(fixture.plan_json)), freeze: codec.parseCanonical(Buffer.from(fixture.freeze_json)), drain: codec.parseCanonical(Buffer.from(fixture.drain_json)), source: codec.parseCanonical(Buffer.from(fixture.source_json)), runnable: codec.parseCanonical(Buffer.from(fixture.runnable_json)) };
  s.template = s.envelope.payload.template;
  return s;
}
function put(s, kind, value) {
  const bytes = Buffer.isBuffer(value) ? value : codec.encodeCanonical(value);
  const ref = { kind, algorithm: "sha256", digest: hash(bytes), size_bytes: BigInt(bytes.length) };
  s.graph.set(key(ref), bytes); return ref;
}
function setReceipt(receipt, refs) {
  const set = ordered(refs);
  receipt.object_count = BigInt(set.length); receipt.total_bytes = set.reduce((sum, r) => sum + r.size_bytes, 0n); receipt.set_digest = hash(codec.encodeCanonical(set));
}
function seal(s) {
  const p = s.envelope.payload;
  s.envelope.references = ordered([p.q, p.plan, p.freeze_intent, p.drain_record, p.source, p.runnable]);
  const { publication_id: _, ...identity } = p;
  p.publication_id = `pcd1-${hash(codec.encodeCanonical(identity))}`;
  return s;
}
/** Rehash every changed DATA wrapper, so negative cases cannot pass merely
 * because an unrelated parent SHA/receipt became stale. */
function rebuild(s) {
  const selection = s.template.selection;
  s.freeze.launch_digest = hash(codec.encodeCanonical(selection));
  s.drain.launch_request_digest = s.freeze.launch_digest;
  s.drain.partial_binding_digest = hash(codec.encodeCanonical({ protocol: "rudder_partial_workspace_v1", format_version: 1, launch_request_digest: s.freeze.launch_digest, root: selection.root, attempt: selection.attempt, observation: selection.observation }));
  const p = s.envelope.payload;
  p.plan = put(s, "blob", s.plan); s.q.capture.retention.plan = p.plan;
  setReceipt(s.q.capture.retention.local, [...s.plan.local_objects, p.plan]);
  p.freeze_intent = put(s, "blob", s.freeze); p.drain_record = put(s, "blob", s.drain);
  s.q.freeze_intent = p.freeze_intent; s.q.drain_record = p.drain_record;
  setReceipt(s.q.evidence_retention, [p.freeze_intent, p.drain_record]);
  p.q = put(s, "blob", s.q); p.source = put(s, "source_root", s.source);
  s.runnable.source = p.source; p.runnable = put(s, "runnable_root", s.runnable);
  return seal(s);
}
function reader(s, calls = [], deny = new Set()) {
  return async (ref, maxBytes) => {
    assert.ok(ref.size_bytes <= BigInt(maxBytes), "admission precedes I/O");
    calls.push(key(ref)); if (deny.has(key(ref))) throw new ContentFormatError("fixture metadata unavailable");
    const bytes = s.graph.get(key(ref)); if (!bytes) throw new ContentFormatError("fixture metadata unavailable"); return Buffer.from(bytes);
  };
}
const resolve = s => async id => { assert.equal(id, s.template.selection.root.root_id); return validateContentReference(s.template.selection.root.root); };
const validate = (s, read = reader(s), base = resolve(s)) => validatePartialCheckpointDataChildren(s.envelope, codec, read, base);
function rebase(s, directory) {
  s.template.base_source.directory = directory;
  s.template.base_runnable.source = put(s, "source_root", s.template.base_source);
  s.template.base_runnable.view_root = directory;
  s.template.selection.accepted_source = s.template.base_runnable.source;
  s.template.selection.root.root = put(s, "runnable_root", s.template.base_runnable);
  s.q.capture.base = clone(s.template.selection.root); s.plan.base = clone(s.template.selection.root);
}
function pagedDirectory(s, entries) {
  const names = entries.map(entry => Buffer.from(entry.name_hex, "hex"));
  let prefix = 0;
  while (names.length && prefix < names[0].length && names.every(name => name[prefix] === names[0][prefix])) prefix++;
  const summary = { entry_count: BigInt(entries.length), subdirectory_count: BigInt(entries.filter(entry => entry.node.type === "directory").length), entries_bytes: entries.reduce((sum, entry) => sum + BigInt(codec.encodeCanonical(entry).length), 0n) };
  const shard = put(s, "directory_shard", { format_version: 1, prefix_hex: names.length ? names[0].subarray(0, prefix).toString("hex") : "", summary, node: { type: "leaf", entries } });
  return { shard, root: put(s, "directory", { format_version: 2, mode: 493, entry_count: summary.entry_count, subdirectory_count: summary.subdirectory_count, directory_shard: shard }) };
}

test("Rust golden publication has six sorted exact DATA edges and lossless audit integers", async () => {
  assert.equal(hash(fixtureBytes), "5a47a7321952d5064734d9667bf3ddd2ed66f96ada4f871cfb7e0e47530ae76f");
  const s = make();
  assert.deepEqual(codec.encodeCanonical(partialCheckpointDataReferences(s.envelope, codec)), codec.encodeCanonical(s.envelope.references));
  assert.equal(s.envelope.payload.publication_id, fixture.publication_id);
  assert.equal(s.template.base_runnable.knowledge_cursor, 9007199254740999n);
  assert.equal(s.q.capture.begin.generation, 9007199254740997n);
  assert.equal(s.drain.network.network.inode, (1n << 64n) - 2n);
  await validate(s);
});

test("other ActionResult purposes remain unchanged and do not resolve or read children", async () => {
  for (const payload of [{ kind: "ordinary-action" }, null, "opaque old payload", { kind: "partial-checkpoint-data-v2" }]) {
    const value = { format_version: 1, references: [], payload };
    assert.equal(partialCheckpointDataReferences(value, codec), null);
    await validatePartialCheckpointDataChildren(value, codec, async () => assert.fail("other purpose read"), async () => assert.fail("other purpose resolved"));
  }
});

test("inline DATA schema, identity and Rust enum edge ordering fail closed before reads", () => {
  for (const mutate of [
    s => { s.envelope.hidden = true; }, s => { s.envelope.payload.hidden = true; seal(s); },
    s => { s.envelope.payload.source_role = "accepted_source"; seal(s); },
    s => { s.envelope.payload.template.selection.hidden = true; seal(s); },
    s => { s.envelope.payload.template.profile.prepared = [[[97n]]]; seal(s); },
    s => { s.envelope.payload.template.base_runnable.environment = s.envelope.payload.source; seal(s); },
    s => { s.envelope.payload.publication_id = `pcd1-${"0".repeat(64)}`; },
    s => { s.envelope.references.reverse(); }, s => { s.envelope.references[1] = s.envelope.references[0]; },
    s => { s.envelope.payload.q.size_bytes = 32769n; seal(s); }, s => { s.envelope.payload.plan.size_bytes = 4194305n; seal(s); },
    s => { s.envelope.payload.drain_record.size_bytes = 32769n; seal(s); },
    s => { s.template.selection.attempt.owner.epoch = Number.MAX_SAFE_INTEGER + 2; },
  ]) { const s = make(); mutate(s); assert.throws(() => partialCheckpointDataReferences(s.envelope, codec), ContentFormatError); }
  const s = make(); assert.throws(() => partialCheckpointDataReferences(JSON.parse(fixture.envelope_json), codec), ContentFormatError);
  s.envelope.payload.template.selection.root.root_id = "x".repeat(64 * 1024); assert.throws(() => partialCheckpointDataReferences(s.envelope, codec), ContentFormatError);
});

test("inline admission rejects excess bytes, sparse arrays, cycles and deep values before encoding", () => {
  for (const extra of ["x".repeat(65537), [, true], 1n << 64n, Object.assign(new Date(), { value: 1 })]) {
    const s = make(); s.envelope.payload.extra = extra; let encoded = 0;
    assert.throws(() => partialCheckpointDataReferences(s.envelope, { ...codec, encodeCanonical: value => { encoded++; return codec.encodeCanonical(value); } }), ContentFormatError);
    assert.equal(encoded, 0, "untrusted tree admission precedes whole-value encoding");
  }
  for (const mode of ["cycle", "depth"]) {
    const s = make(); let extra = {};
    if (mode === "cycle") extra.next = extra; else for (let i = 0; i < 130; i++) extra = { next: extra };
    s.envelope.payload.extra = extra; let encoded = 0;
    assert.throws(() => partialCheckpointDataReferences(s.envelope, { ...codec, encodeCanonical: value => { encoded++; return codec.encodeCanonical(value); } }), ContentFormatError);
    assert.equal(encoded, 0);
  }
});

test("unknown or forged Q, plan, freeze and drain fields remain invalid after all hashes are recomputed", async () => {
  const cases = [
    s => { s.q.extra = "not allowed"; }, s => { s.q.capture.begin.extra = true; },
    s => { s.q.capture.attempt.run_epoch++; }, s => { s.q.capture.end.generation++; },
    s => { s.q.capture.consistency = "quiesced"; }, s => { s.q.stop.owner.epoch++; },
    s => { s.q.capture.retention.local.generation = 0n; }, s => { s.q.evidence_retention.staging_id = "not-a-uuid"; },
    s => { s.plan.extra = true; }, s => { s.plan.capture_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"; },
    s => { s.freeze.request.extra = true; }, s => { s.freeze.request.intent_id = s.q.stop.request_id; },
    s => { s.freeze.request.capture.expected_begin.generation++; }, s => { s.freeze.request.capture.expected_head = "another-head"; },
    s => { s.drain.network.hidden = true; }, s => { s.drain.network.mode = "connected_v1"; },
    s => { s.drain.kernel_drained = false; }, s => { s.drain.owner.epoch++; },
    s => { s.drain.network.user.inode = 1n << 64n; }, s => { s.drain.agent_exit = 2147483648n; },
  ];
  for (const mutate of cases) {
    const s = make(); mutate(s); rebuild(s);
    assert.equal(partialCheckpointDataReferences(s.envelope, codec).length, 6, "inline hashes are recomputed");
    await assert.rejects(validate(s), ContentFormatError);
  }
});

test("plan provenance is sorted unique content-only, scoped and backed by actual base metadata", async () => {
  for (const mutate of [
    s => s.plan.local_objects.reverse(), s => s.plan.local_objects.push(s.plan.local_objects[0]),
    s => s.plan.remote_boundaries.reverse(), s => s.plan.remote_boundaries.push(s.plan.remote_boundaries[0]),
    s => { s.plan.remote_boundaries[1].edges = [1n, 99n]; },
    s => { s.plan.remote_boundaries[1].edges = Array(1025).fill(0n); },
    s => { s.plan.remote_boundaries[1].reference = { ...s.plan.remote_boundaries[1].reference, digest: "f".repeat(64) }; },
    s => { s.plan.local_objects.push(s.plan.remote_boundaries[0].reference); s.plan.local_objects.sort(compare); },
    s => { s.plan.local_objects.push(s.envelope.payload.source); s.plan.local_objects.sort(compare); },
    s => { s.plan.local_objects.push({ kind: "blob", algorithm: "sha256", digest: "0".repeat(64), size_bytes: 1n }); s.plan.local_objects.sort(compare); },
  ]) { const s = make(); mutate(s); rebuild(s); await assert.rejects(validate(s), ContentFormatError); }
  const s = make(), calls = [];
  await assert.rejects(validate(s, reader(s, calls), async () => ({ ...s.template.selection.root.root, digest: "0".repeat(64) })), ContentFormatError);
  assert.equal(calls.length, 0, "account-scoped immutable base refusal precedes child I/O");
  const missing = s.template.base_runnable.view_root;
  await assert.rejects(validate(s, reader(s, [], new Set([key(missing)]))), ContentFormatError);
  await assert.rejects(validate(s, reader(s, [], new Set([key(s.template.base_runnable.source)]))), ContentFormatError);
});

test("u64 MAX cursor and historical seed identity stay exact without fetching seed or inherited payload", async () => {
  const s = make(); s.template.base_runnable.knowledge_cursor = (1n << 64n) - 1n; s.runnable.knowledge_cursor = (1n << 64n) - 1n;
  s.template.selection.root.root = put(s, "runnable_root", s.template.base_runnable);
  s.q.capture.base = clone(s.template.selection.root); s.plan.base = clone(s.template.selection.root);
  s.template.selection.accepted_seed.size_bytes = (1n << 64n) - 1n;
  rebuild(s); const calls = [];
  await validate(s, reader(s, calls));
  for (const ref of [s.template.selection.accepted_seed, ...s.plan.remote_boundaries.map(p => p.reference), s.template.base_runnable.profile, s.template.base_source.projection_policy]) assert.ok(!calls.includes(key(ref)), "audit/final inherited payload is not fetched");
});

test("paged Directory/File provenance follows Rust edge order and checks actual intermediate child semantics", async () => {
  function build(wrongBaseLength = false, wrongNewLength = false) {
    const s = make(), inherited = s.plan.remote_boundaries.find(path => path.edges.length > 1).reference;
    const changed = s.plan.local_objects.find(ref => ref.kind === "blob");
    const file = child => put(s, "file", { format_version: 1, layout: "fixed_4mib_fanout256_v1", level: 0, logical_size: child.size_bytes, children: [child] });
    const inheritedFile = file(inherited), changedFile = file(changed);
    const entry = (name, object, length) => ({ name_hex: Buffer.from(name).toString("hex"), node: { type: "file", object, logical_size: length, mode: 420 } });
    const base = pagedDirectory(s, [entry("a", inheritedFile, inherited.size_bytes + BigInt(wrongBaseLength))]);
    rebase(s, base.root);
    const current = pagedDirectory(s, [entry("a", inheritedFile, inherited.size_bytes + BigInt(wrongNewLength)), entry("b", changedFile, changed.size_bytes)]);
    s.plan.directory = current.root; s.source.directory = current.root; s.runnable.view_root = current.root;
    s.plan.local_objects = ordered([changed, inheritedFile, changedFile, current.root, current.shard]);
    s.plan.remote_boundaries = [{ reference: s.template.base_runnable.hardlinks, edges: [3n] }, { reference: inherited, edges: [1n, 0n, 0n, 0n] }].sort((a, b) => compare(a.reference, b.reference));
    rebuild(s); return { s, inherited, inheritedFile, base };
  }
  const { s, inherited, inheritedFile, base } = build(), calls = [];
  await validate(s, reader(s, calls));
  for (const ref of [base.root, base.shard, inheritedFile]) assert.ok(calls.includes(key(ref)), "actual metadata on the provenance path is read");
  assert.ok(!calls.includes(key(inherited)), "the final inherited payload is not fetched by provenance replay");
  for (const bad of [build(true), build(false, true)]) await assert.rejects(validate(bad.s), /logical size/i);
});

test("metadata replay independently stops at its byte budget before another base read", async () => {
  const s = make(), inherited = s.plan.remote_boundaries.find(path => path.edges.length > 1);
  // Schema1 generic CAS directories permit this long name. This intentionally
  // exercises the historical DATA reader, not strict-launch filesystem support.
  const name = "61".repeat(1024 * 1024); let directory = s.template.base_source.directory;
  for (let i = 0; i < 34; i++) directory = put(s, "directory", { format_version: 1, mode: 493, entries: [{ name_hex: name, node: { type: "directory", object: directory } }] });
  rebase(s, directory); inherited.edges = [1n, ...Array(35).fill(0n)]; rebuild(s);
  let bytes = 0, calls = 0;
  await assert.rejects(validate(s, async (ref, max) => {
    bytes += Number(ref.size_bytes); calls++;
    assert.ok(bytes <= 64 * 1024 * 1024, "budget admission precedes I/O");
    return reader(s)(ref, max);
  }), /proof inventory exhausted/);
  assert.ok(calls > 20, "multiple independent base objects were replayed");
});

test("bound child bytes/canonical JSON and repeated validation cannot be bypassed by cached refs", async () => {
  const s = make(), calls = [], memo = new Map();
  const read = async (ref, max) => { if (!memo.has(key(ref))) memo.set(key(ref), await reader(s, calls)(ref, max)); return memo.get(key(ref)); };
  await validate(s, read); assert.ok(memo.size > 0);
  await assert.rejects(validate(s, read, async () => null), ContentFormatError);
  await assert.rejects(validate(s, async (ref, max) => key(ref) === key(s.envelope.payload.q) ? Buffer.from("{}") : read(ref, max)), ContentFormatError);
  const raw = codec.encodeCanonical(s.q).toString().replace('"format_version":1', '"format_version":1,"format_version":1');
  s.envelope.payload.q = put(s, "blob", Buffer.from(raw)); seal(s);
  await assert.rejects(validate(s), ContentFormatError);
});

class Storage {
  objects = new Map(); next = 0;
  async get(key, max) { const object = this.objects.get(key); if (!object) return null; assert.ok(object.body.length <= max); return { body: Buffer.from(object.body), etag: object.etag }; }
  async putIfAbsent(key, bytes) { if (this.objects.has(key)) return false; this.objects.set(key, { body: Buffer.from(bytes), etag: String(++this.next) }); return true; }
  async replaceIfMatch(key, etag, bytes) { if (this.objects.get(key)?.etag !== etag) return false; this.objects.set(key, { body: Buffer.from(bytes), etag: String(++this.next) }); return true; }
  async delete(key) { this.objects.delete(key); }
}
async function populate(s, cache = undefined) {
  const storage = new Storage(), cas = new FabricCas({ storage, accountId: "partial-checkpoint-fixture", verificationCache: cache });
  let lease = await cas.beginStaging();
  for (const o of fixture.base_objects) await cas.putObject(lease.lease_id, o.reference, Buffer.from(o.hex, "hex"));
  await cas.publishRoot(s.template.selection.root.root_id, lease.lease_id, wire(s.template.selection.root.root));
  lease = await cas.beginStaging();
  for (const [id, bytes] of s.graph) {
    const [kind, digest, size] = id.split(":"); await cas.putObject(lease.lease_id, { kind, digest, size_bytes: Number(size), algorithm: "sha256" }, bytes);
  }
  const publication = put(s, "action_result", s.envelope); await cas.putObject(lease.lease_id, wire(publication), codec.encodeCanonical(s.envelope));
  return { storage, cas, lease, publication };
}

test("CAS cached closure still performs DATA child semantics, and ordinary closure refuses absent inherited bytes", async () => {
  const bad = make(); bad.drain.network.mode = "connected_v1"; rebuild(bad);
  const cache = new FabricVerificationCache(), cached = await populate(bad, cache);
  assert.ok(cache.stats().entries > 0, "uploads prime retained ref proofs");
  await assert.rejects(cached.cas.publishRoot("bad-partial", cached.lease.lease_id, wire(cached.publication)), /partial checkpoint DATA/i);
  const good = make(), loaded = await populate(good);
  const inherited = good.plan.remote_boundaries.find(p => p.edges.length > 1).reference;
  for (const key of [...loaded.storage.objects.keys()]) if (key.includes(inherited.digest)) loaded.storage.objects.delete(key);
  await assert.rejects(loaded.cas.publishRoot("missing-inherited", loaded.lease.lease_id, wire(loaded.publication)), /unavailable|missing/i);
});

test("historical generic root receipts remain byte durability, without DATA semantics or authority", async () => {
  const s = make(); s.drain.network.mode = "connected_v1"; rebuild(s);
  const loaded = await populate(s, new FabricVerificationCache());
  // An older server accepted this future purpose as an opaque ActionResult.
  // Reproduce its genuine complete byte closure without the new child checks.
  const seen = new Map(), pending = [wire(loaded.publication)];
  while (pending.length) {
    const ref = pending.pop(); if (seen.has(key(ref))) continue;
    const bytes = s.graph.get(key(ref)); assert.ok(bytes, "old receipt still requires every byte");
    seen.set(key(ref), ref);
    pending.push(...fabricReferences(ref, bytes));
  }
  const rootId = "historical-generic-partial";
  const receipt = { format_version: 1, root_id: rootId, root: wire(loaded.publication), durability: "replicated", published_at_ms: 1, object_count: seen.size, total_bytes: [...seen.values()].reduce((sum, ref) => sum + ref.size_bytes, 0) };
  const baseSuffix = `/roots/${s.template.selection.root.root_id}.json`;
  const baseKey = [...loaded.storage.objects.keys()].find(key => key.endsWith(baseSuffix));
  assert.ok(baseKey);
  await loaded.storage.putIfAbsent(`${baseKey.slice(0, -baseSuffix.length)}/roots/${rootId}.json`, codec.encodeCanonical(receipt));
  assert.deepEqual(await loaded.cas.getRoot(rootId), receipt);
  assert.deepEqual(await loaded.cas.publishRoot(rootId, loaded.lease.lease_id, wire(loaded.publication)), receipt);
  assert.deepEqual(Object.keys(receipt).sort(), ["format_version", "root_id", "root", "durability", "published_at_ms", "object_count", "total_bytes"].sort());
  await assert.rejects(validate(s), ContentFormatError, "saved bytes cannot recreate producer pins or live authorization");
  await assert.rejects(loaded.cas.publishRoot("new-typed-publication", loaded.lease.lease_id, wire(loaded.publication)), /partial checkpoint DATA/i);
});
