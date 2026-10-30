import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { Readable } from "node:stream";
import {
  FabricCas, FabricCasError, FabricVerificationCache, MemoryFabricObjectStorage, S3FabricObjectStorage,
  canonicalFabricMetadata, fabricReferences, validateFabricObjectRef,
} from "../cloud/dist/fabric-cas.js";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: hash(bytes), size_bytes: bytes.length });
const metadata = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { bytes, object: ref(kind, bytes) }; };
const source = (directory, parents = []) => metadata("source_root", { format_version: 1, directory, parents, consistency: "quiesced" });
const empty = () => metadata("directory", { format_version: 1, mode: 0o755, entries: [] });
const file = (name, object) => ({ name_hex: Buffer.from(name).toString("hex"), node: { type: "file", object, mode: 0o644 } });
const fixture = (options = {}) => { const storage = options.storage || new MemoryFabricObjectStorage(); const cas = new FabricCas({ storage, accountId: "account-a", ...options }); return { storage, cas }; };
async function upload(cas, objects) { const lease = await cas.beginStaging(); for (const { object, bytes } of objects) await cas.putObject(lease.lease_id, object, bytes); return lease; }

test("raw SHA-256 objects require persisted staging and are immutable and verified", async () => {
  const { cas, storage } = fixture(); const bytes = Buffer.from("abc"); const object = ref("blob", bytes);
  assert.equal(object.digest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  await assert.rejects(cas.putObject("00000000-0000-4000-8000-000000000000", object, bytes), { code: "lease_missing" });
  const lease = await cas.beginStaging();
  assert.equal((await cas.putObject(lease.lease_id, object, bytes)).stored, true);
  assert.equal((await cas.putObject(lease.lease_id, object, bytes)).stored, false);
  await assert.rejects(cas.putObject(lease.lease_id, object, Buffer.from("abd")), { code: "checksum_mismatch" });
  assert.deepEqual(await cas.getObject(object), bytes);
  const reopened = new FabricCas({ storage, accountId: "account-a" });
  assert.deepEqual(await reopened.getObject(object), bytes);
  assert.deepEqual(await reopened.missing([object]), []);
});

test("metadata matches canonical Rust byte names, modes, symlinks and full u64 cursors", async () => {
  const tree = empty();
  assert.equal(tree.bytes.toString(), '{"entries":[],"format_version":1,"mode":493}');
  assert.deepEqual(fabricReferences(tree.object, tree.bytes), []);
  const root = source(tree.object);
  const checkpoint = metadata("checkpoint", { format_version: 1, source: root.object, environment: null, parents: [], consistency: "quiesced", durability: "local", knowledge_cursor: 18446744073709551615n });
  assert.deepEqual(fabricReferences(checkpoint.object, checkpoint.bytes), [root.object]);
  const links = metadata("directory", { format_version: 1, mode: 0o711, entries: [{ name_hex: "ff", node: { type: "symlink", target_hex: "2e2e2f65787465726e616c" } }] });
  assert.deepEqual(fabricReferences(links.object, links.bytes), []);
  const ordered = canonicalFabricMetadata({ "\u{10000}": 1, "\uffff": 2 });
  assert.equal(ordered.toString(), '{"￿":2,"𐀀":1}', "UTF-8 map ordering differs from JS UTF-16 order");
});

test("invalid metadata, unsupported types and traversal names fail before storage", async () => {
  const { cas } = fixture(); const lease = await cas.beginStaging();
  const bytes = Buffer.from("x"); const object = ref("blob", bytes);
  for (const name_hex of ["", "2e", "2e2e", "612f62", "00", "FF", "f"]) {
    const item = metadata("directory", { format_version: 1, mode: 493, entries: [{ name_hex, node: { type: "file", object, mode: 420 } }] });
    await assert.rejects(cas.putObject(lease.lease_id, item.object, item.bytes));
  }
  const invalid = [
    { format_version: 2, mode: 493, entries: [] },
    { format_version: 1, mode: 0o100755, entries: [] },
    { format_version: 1, mode: 493, entries: [file("z", object), file("a", object)] },
    { format_version: 1, mode: 493, entries: [file("a", object), file("a", object)] },
    { format_version: 1, mode: 493, entries: [{ name_hex: "61", node: { type: "socket" } }] },
    { format_version: 1, mode: 493, entries: [], untracked: object },
  ];
  for (const value of invalid) { const item = metadata("directory", value); await assert.rejects(cas.putObject(lease.lease_id, item.object, item.bytes)); }
  for (const text of ['{ "entries":[],"format_version":1,"mode":493}', '{"entries":[],"format_version":1,"mode":493.0}', '{"entries":[],"format_version":1,"mode":493,"mode":493}', '{"entries":[],"format_version":1,"mode":493e0}']) {
    const bytes = Buffer.from(text); await assert.rejects(cas.putObject(lease.lease_id, ref("directory", bytes), bytes));
  }
  assert.throws(() => validateFabricObjectRef({ ...object, digest: "../escape" }));
  assert.throws(() => validateFabricObjectRef({ ...object, kind: "device" }));
  assert.throws(() => validateFabricObjectRef({ ...object, size_bytes: 257 * 1024 * 1024 }));
});

test("partial closure cannot publish, and retry after transfer atomically creates a durable root receipt", async () => {
  const { cas, storage } = fixture(); const body = Buffer.from("source"); const blob = { bytes: body, object: ref("blob", body) };
  const tree = metadata("directory", { format_version: 1, mode: 493, entries: [file("main", blob.object)] }); const root = source(tree.object);
  const lease = await upload(cas, [root]);
  await assert.rejects(cas.publishRoot("snapshot-a", lease.lease_id, root.object), { code: "object_missing" });
  assert.equal(await cas.getRoot("snapshot-a"), null);
  await cas.putObject(lease.lease_id, blob.object, blob.bytes); await cas.putObject(lease.lease_id, tree.object, tree.bytes);
  const receipt = await cas.publishRoot("snapshot-a", lease.lease_id, root.object);
  assert.equal(receipt.durability, "replicated"); assert.equal(receipt.object_count, 3);
  const reopened = new FabricCas({ storage, accountId: "account-a" });
  assert.deepEqual(await reopened.getRoot("snapshot-a"), receipt);
  assert.deepEqual(await reopened.publishRoot("snapshot-a", "expired-and-invalid-is-irrelevant", root.object), receipt);
  await assert.rejects(cas.publishRoot("snapshot-a", lease.lease_id, blob.object), { code: "root_conflict" });
});

test("same-root publication races return one receipt; conflicting roots never overwrite", async () => {
  const { cas } = fixture(); const a = Buffer.from("a"); const b = Buffer.from("b");
  const objects = [{ bytes: a, object: ref("blob", a) }, { bytes: b, object: ref("blob", b) }]; const lease = await upload(cas, objects);
  const same = await Promise.all([cas.publishRoot("same", lease.lease_id, objects[0].object), cas.publishRoot("same", lease.lease_id, objects[0].object)]);
  assert.deepEqual(same[0], same[1]);
  const second = await cas.beginStaging();
  const outcomes = await Promise.allSettled([cas.publishRoot("conflict", second.lease_id, objects[0].object), cas.publishRoot("conflict", second.lease_id, objects[1].object)]);
  assert.equal(outcomes.filter((outcome) => outcome.status === "fulfilled").length, 1);
  assert.equal(outcomes.find((outcome) => outcome.status === "rejected").reason.code, "root_conflict");
});

test("lease cleanup failure leaves a committed immutable receipt and never deletes data", async () => {
  class CleanupFailure extends MemoryFabricObjectStorage { async delete() { throw new Error("private backend detail"); } }
  let now = 1000; const { cas } = fixture({ storage: new CleanupFailure(), now: () => now });
  const bytes = Buffer.from("retained"); const object = ref("blob", bytes); const lease = await upload(cas, [{ bytes, object }]);
  const receipt = await cas.publishRoot("proof", lease.lease_id, object); now = 9_000_000;
  assert.deepEqual(await cas.publishRoot("proof", lease.lease_id, object), receipt);
  assert.deepEqual(await cas.getObject(object), bytes);
});

test("expired or cross-account leases cannot upload or publish; objects are never swept", async () => {
  let now = 1000; const { cas, storage } = fixture({ now: () => now });
  const bytes = Buffer.from("orphan retained"); const object = ref("blob", bytes); const lease = await cas.beginStaging(1000);
  await cas.putObject(lease.lease_id, object, bytes); now = 2000;
  await assert.rejects(cas.publishRoot("expired", lease.lease_id, object), { code: "lease_expired" });
  await assert.rejects(cas.renewStaging(lease.lease_id), { code: "lease_expired" });
  assert.deepEqual(await cas.getObject(object), bytes);
  const other = new FabricCas({ storage, accountId: "account-b" });
  await assert.rejects(other.getObject(object), { code: "object_missing" });
  await assert.rejects(other.putObject(lease.lease_id, object, bytes), { code: "lease_missing" });
  assert.equal(await other.getRoot("expired"), null);
  for (const id of ["../root", "a/b", ".", "..", "%2fsecret", "x".repeat(129)]) await assert.rejects(cas.getRoot(id));
});

test("lease renewals use compare-and-swap and bounded conflict retries", async () => {
  let now = 1000; const { cas } = fixture({ now: () => now }); const lease = await cas.beginStaging(1000); now = 1500;
  const renewed = await Promise.all([cas.renewStaging(lease.lease_id, 2000), cas.renewStaging(lease.lease_id, 2000)]);
  assert.ok(renewed.every((item) => item.expires_at_ms === 3500));
  class Conflict extends MemoryFabricObjectStorage { async replaceIfMatch() { return false; } }
  const conflict = fixture({ storage: new Conflict() }).cas; const item = await conflict.beginStaging();
  await assert.rejects(conflict.renewStaging(item.lease_id), { code: "lease_conflict" });
});

test("closure verifies environment inputs and action outputs with count, depth, byte and latency caps", async () => {
  const { cas, storage } = fixture(); const tree = empty(); const root = source(tree.object);
  const checkpoint = metadata("checkpoint", { format_version: 1, source: root.object, environment: null, parents: [], consistency: "quiesced", durability: "local", knowledge_cursor: 0 });
  const action = metadata("action_result", { format_version: 1, references: [checkpoint.object], payload: { exit_code: 0 } });
  const lease = await upload(cas, [tree, root, checkpoint, action]);
  assert.equal((await cas.closure(action.object)).objects.length, 4);
  for (const limits of [{ maxClosureObjects: 2 }, { maxClosureDepth: 1 }, { maxClosureBytes: action.bytes.length }]) {
    const bounded = new FabricCas({ storage, accountId: "account-a", limits });
    await assert.rejects(bounded.publishRoot("bounded", lease.lease_id, action.object), { code: "too_large" });
    assert.equal(await bounded.getRoot("bounded"), null);
  }
  class Hang extends MemoryFabricObjectStorage { async get() { return new Promise(() => { }); } }
  const hung = fixture({ storage: new Hang(), limits: { closureTimeoutMs: 10 } }).cas;
  await assert.rejects(hung.closure(tree.object), { code: "verification_timeout" });
  await assert.rejects(cas.missing(Array(257).fill(tree.object)), { code: "too_large" });
});

test("S3 adapter sends conditional durable writes and bounded verified streams without trusting ETag hashes", async () => {
  const requests = []; const body = Buffer.from("payload");
  const client = { async send(command) { requests.push(command); if (command.constructor.name === "GetObjectCommand") return { Body: Readable.from([body]), ETag: '"opaque-etag"', ContentLength: body.length }; return {}; } };
  const storage = new S3FabricObjectStorage({ client, bucket: "test-bucket" });
  assert.equal(await storage.putIfAbsent("key", body), true);
  assert.equal(requests[0].input.IfNoneMatch, "*"); assert.equal(requests[0].input.ServerSideEncryption, "AES256");
  assert.equal(requests[0].input.ChecksumSHA256, createHash("sha256").update(body).digest("base64"));
  assert.equal(await storage.replaceIfMatch("key", '"version"', body), true); assert.equal(requests[1].input.IfMatch, '"version"');
  assert.deepEqual(await storage.get("key", body.length), { body, etag: '"opaque-etag"' });
  await assert.rejects(storage.get("key", body.length - 1), { code: "too_large" });
  const conflict = new S3FabricObjectStorage({ client: { async send() { throw { $metadata: { httpStatusCode: 412 } }; } }, bucket: "test" });
  assert.equal(await conflict.putIfAbsent("key", body), false);
  const failed = new S3FabricObjectStorage({ client: { async send() { throw new Error("secret-from-aws"); } }, bucket: "test" });
  await assert.rejects(failed.get("key", 100), (error) => error instanceof FabricCasError && !error.message.includes("secret-from-aws"));
});

test("S3 deadlines preserve admission limits even if a faulty transport ignores abort", async () => {
  let resolve; let calls = 0;
  const storage = new S3FabricObjectStorage({ client: { send() { calls++; return new Promise((done) => { resolve = done; }); } }, bucket: "test", timeoutMs: 10, maxConcurrency: 1 });
  await assert.rejects(storage.get("key", 1), { code: "storage_timeout" });
  await assert.rejects(storage.get("key", 1), { code: "storage_busy" });
  assert.equal(calls, 1);
  resolve({ Body: Readable.from([]), ETag: "value" });
  await new Promise((done) => setImmediate(done));
});

test("trusted shared retention proofs avoid per-file reads for 10,000 freshly stored source files", async () => {
  class CountReads extends MemoryFabricObjectStorage {
    objectReads = 0;
    async get(key, limit) { if (key.includes('/objects/')) this.objectReads++; return super.get(key, limit); }
  }
  const storage = new CountReads(); const cache = new FabricVerificationCache(32 * 1024 * 1024);
  const cas = new FabricCas({ storage, accountId: "account-a", verificationCache: cache });
  const lease = await cas.beginStaging(); const entries = [];
  for (let index = 0; index < 10_000; index++) {
    const body = Buffer.from(`source file ${index}`); const object = ref("blob", body);
    await cas.putObject(lease.lease_id, object, body);
    entries.push(file(`file-${String(index).padStart(5, '0')}`, object));
  }
  const tree = metadata("directory", { format_version: 1, mode: 493, entries }); const root = source(tree.object);
  await cas.putObject(lease.lease_id, tree.object, tree.bytes); await cas.putObject(lease.lease_id, root.object, root.bytes);
  // A second service instance shares exact backend identity and account scope.
  const reopened = new FabricCas({ storage, accountId: "account-a", verificationCache: cache });
  const receipt = await reopened.publishRoot("prepared-10k", lease.lease_id, root.object);
  assert.equal(receipt.object_count, 10_002); assert.equal(storage.objectReads, 0);
  assert.ok(cache.stats().estimated_bytes <= 32 * 1024 * 1024);
  assert.ok(cache.stats().hits >= 10_002);
  assert.deepEqual(await reopened.getObject(entries[0].node.object), Buffer.from("source file 0"));
  assert.equal(storage.objectReads, 1, "GET always verifies actual bytes even with a proof");
});

test("cached metadata never certifies missing children and cache proof scope includes account and backend", async () => {
  const storage = new MemoryFabricObjectStorage(); const cache = new FabricVerificationCache();
  const cas = new FabricCas({ storage, accountId: "account-a", verificationCache: cache });
  const bytes = Buffer.from("missing"); const blob = ref("blob", bytes);
  const tree = metadata("directory", { format_version: 1, mode: 493, entries: [file("main", blob)] });
  const lease = await upload(cas, [tree]);
  await assert.rejects(cas.publishRoot("no-missing-children", lease.lease_id, tree.object), { code: "object_missing" });
  await cas.putObject(lease.lease_id, blob, bytes);
  assert.equal((await cas.closure(tree.object)).objects.length, 2);
  for (const other of [
    new FabricCas({ storage, accountId: "account-b", verificationCache: cache }),
    new FabricCas({ storage: new MemoryFabricObjectStorage(), accountId: "account-a", verificationCache: cache }),
  ]) await assert.rejects(other.closure(tree.object), { code: "object_missing" });
  const restricted = new FabricCas({ storage, accountId: "account-a", verificationCache: cache, limits: { maxObjectBytes: tree.bytes.length - 1 } });
  await assert.rejects(restricted.closure(tree.object));
});

test("cache eviction falls back to verification and failed writes never create success proofs", async () => {
  class Faults extends MemoryFabricObjectStorage {
    reads = 0; fail = false;
    async get(key, limit) { if (key.includes('/objects/')) this.reads++; return super.get(key, limit); }
    async putIfAbsent(key, bytes) { if (this.fail && key.includes('/objects/')) throw new Error('injected put failure'); return super.putIfAbsent(key, bytes); }
  }
  const storage = new Faults(); const cache = new FabricVerificationCache(1400);
  const cas = new FabricCas({ storage, accountId: "account-a", verificationCache: cache });
  const first = Buffer.from('first'); const second = Buffer.from('second'); const a = ref('blob', first), b = ref('blob', second);
  const stage = await cas.beginStaging(); await cas.putObject(stage.lease_id, a, first); await cas.putObject(stage.lease_id, b, second);
  assert.equal(cache.stats().entries, 1); await cas.closure(a); assert.ok(storage.reads >= 1);
  storage.fail = true; const missing = ref('blob', Buffer.from('never stored'));
  await assert.rejects(cas.putObject(stage.lease_id, missing, Buffer.from('never stored')));
  await assert.rejects(cas.closure(missing), { code: "object_missing" });
  assert.ok(cache.stats().estimated_bytes <= cache.stats().max_bytes);
});

test("GET detects corruption despite retained proof and invalidates that object's authority", async () => {
  class Tamper extends MemoryFabricObjectStorage {
    corrupt = false;
    async get(key, limit) { const value = await super.get(key, limit); if (this.corrupt && key.includes('/objects/') && value) value.body[0] ^= 1; return value; }
  }
  const storage = new Tamper(); const cache = new FabricVerificationCache();
  const cas = new FabricCas({ storage, accountId: "account-a", verificationCache: cache });
  const bytes = Buffer.from('object'); const object = ref('blob', bytes); const stage = await upload(cas, [{ bytes, object }]);
  assert.equal(cache.stats().entries, 1); storage.corrupt = true;
  await assert.rejects(cas.getObject(object), { code: "checksum_mismatch" });
  assert.equal(cache.stats().entries, 0);
  await assert.rejects(cas.publishRoot('not-certified', stage.lease_id, object), { code: "checksum_mismatch" });
  assert.equal(await cas.getRoot('not-certified'), null);
});
