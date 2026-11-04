import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { FabricRemoteClient } from "../dist/fabric-remote.js";
import { createFabricHttpHandler } from "../cloud/dist/fabric-http.js";
import { FabricCas, FabricVerificationCache, canonicalFabricMetadata, parseCanonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";

const goldenBytes = await fs.readFile(new URL("./fixtures/partial-checkpoint-v1.json", import.meta.url));
const sha = bytes => createHash("sha256").update(bytes).digest("hex");
assert.equal(sha(goldenBytes), "5a47a7321952d5064734d9667bf3ddd2ed66f96ada4f871cfb7e0e47530ae76f");
const golden = JSON.parse(goldenBytes.toString());
const decode = text => parseCanonicalFabricMetadata(Buffer.from(text));
const identity = ref => `${ref.kind}:${ref.digest}:${ref.size_bytes}`;
const wire = ref => ({ ...ref, size_bytes: Number(ref.size_bytes) });
const describe = (kind, value) => {
  const bytes = Buffer.isBuffer(value) ? value : canonicalFabricMetadata(value);
  return { reference: { kind, algorithm: "sha256", digest: sha(bytes), size_bytes: bytes.length }, bytes };
};
function data() {
  const envelope = decode(golden.envelope_json), plan = decode(golden.plan_json);
  return { envelope, plan, publication: golden.publication, publicationId: golden.publication_id,
    objects: golden.objects.map(item => ({ reference: item.reference, bytes: Buffer.from(item.hex, "hex") })) };
}

// A small local fixture, not a replicated provider or a power-loss/RPO test.
// Fully written files become visible by conditional link; staging replacement
// compares its ETag under one mutation queue, then atomically renames.
class FileStorage {
  tail = Promise.resolve();
  constructor(root) { this.root = root; }
  file(key) {
    assert.match(key, /^fabric\/v1\/accounts\/[a-f0-9]{64}\/(?:objects\/sha256\/[a-f0-9]{64}|staging\/[a-f0-9-]+\.json|roots\/[A-Za-z0-9._-]+\.json)$/);
    return path.join(this.root, ...key.split("/"));
  }
  mutate(work) {
    const result = this.tail.then(work); this.tail = result.catch(() => {}); return result;
  }
  async get(key, maxBytes) {
    let file;
    try { file = await fs.open(this.file(key), "r"); }
    catch (error) { if (error.code === "ENOENT") return null; throw error; }
    try {
      const stat = await file.stat(); assert.ok(stat.size <= maxBytes, "storage read admission precedes byte allocation");
      const bytes = Buffer.alloc(stat.size + 1); const { bytesRead } = await file.read(bytes, 0, bytes.length, 0);
      assert.equal(bytesRead, stat.size); const body = bytes.subarray(0, bytesRead);
      return { body, etag: sha(body) };
    } finally { await file.close(); }
  }
  async privateFile(file, bytes) {
    await fs.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
    const temporary = `${file}.${randomUUID()}`; const handle = await fs.open(temporary, "wx", 0o600);
    try { await handle.writeFile(bytes); await handle.sync(); } finally { await handle.close(); }
    return temporary;
  }
  putIfAbsent(key, bytes) {
    return this.mutate(async () => {
      const file = this.file(key), temporary = await this.privateFile(file, bytes);
      try { await fs.link(temporary, file); return true; }
      catch (error) { if (error.code === "EEXIST") return false; throw error; }
      finally { await fs.unlink(temporary); }
    });
  }
  replaceIfMatch(key, etag, bytes) {
    return this.mutate(async () => {
      const old = await this.get(key, 4096); if (!old || old.etag !== etag) return false;
      const file = this.file(key), temporary = await this.privateFile(file, bytes);
      await fs.rename(temporary, file); return true;
    });
  }
  delete(key) { return this.mutate(() => fs.rm(this.file(key), { force: true })); }
}

async function fixture(t) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), "rudder-pcd-http-"));
  await fs.chmod(directory, 0o700);
  const storage = new FileStorage(directory), requests = [];
  let server, baseUrl, dropAcknowledgment, lostAcknowledgment;
  const accounts = new Map([["fixture-a", "account-a"], ["fixture-b", "account-b"]]);
  const cas = accountId => new FabricCas({ storage, accountId });
  async function stop() {
    if (server) { const previous = server; server = undefined; previous.closeAllConnections(); await new Promise(resolve => previous.close(resolve)); }
  }
  async function start() {
    const handler = createFabricHttpHandler({ storage, verificationCache: new FabricVerificationCache(), authenticate: request => {
      const account = accounts.get(request.headers.authorization?.replace(/^Bearer /, ""));
      if (!account) throw Object.assign(new Error("fixture credential refused"), { status: 401 });
      return account;
    } });
    server = http.createServer((request, response) => {
      const url = new URL(request.url, "http://127.0.0.1");
      requests.push({ method: request.method, path: url.pathname });
      if (request.method === "POST" && url.pathname.endsWith(`/roots/${dropAcknowledgment}`)) {
        const end = response.end;
        response.end = function (...args) {
          if (response.statusCode === 200) {
            lostAcknowledgment = dropAcknowledgment; dropAcknowledgment = undefined;
            // The real handler calls end only after publishRoot committed.
            // Drop the actual TCP response; never manufacture a success DTO.
            response.destroy(); return response;
          }
          return end.apply(this, args);
        };
      }
      void handler(request, response, url);
    });
    server.on("clientError", (_error, socket) => socket.destroy());
    await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
    baseUrl = `http://127.0.0.1:${server.address().port}/api/rudder/fabric`;
  }
  t.after(async () => { await stop(); await storage.tail; await fs.rm(directory, { recursive: true, force: true }); });
  await start();
  const selection = decode(golden.template_json).selection;
  const client = (token = "fixture-a", scope = selection.root.scope) => new FabricRemoteClient({ baseUrl,
    authHeaders: () => ({ Authorization: `Bearer ${token}` }), scope, concurrency: 1, requestTimeoutMs: 5000 });
  const seed = async () => {
    const remote = client(), staging = await remote.beginStaging();
    for (const item of golden.base_objects) await remote.putObject(staging.lease_id, item.reference, Buffer.from(item.hex, "hex"));
    await remote.publishRoot(selection.root.root_id, staging.lease_id, wire(selection.root.root));
  };
  return { storage, requests, cas, client, seed, selection,
    async restart() { await stop(); await start(); },
    loseNextAcknowledgment(id) { dropAcknowledgment = id; },
    lostAcknowledgment: () => lostAcknowledgment };
}

// The adapter models only this fixture's acknowledged transfer clone. It does
// not reconstruct native active producer pins from the saved audit receipt.
function transferSource(bundle) {
  const bytes = new Map(bundle.objects.map(item => [identity(item.reference), item.bytes]));
  const pins = new Set(), reads = [], receipts = [];
  return { pins, reads, receipts,
    async pinPartialCheckpoint(receiptJson) {
      receipts.push(receiptJson); const pin = `transfer:partial-checkpoint:rpc:${randomUUID()}`; pins.add(pin);
      const p = bundle.envelope.payload;
      return { format_version: 1, owned_pin: pin, publication_id: bundle.publicationId, publication: bundle.publication,
        q: wire(p.q), source: wire(p.source), runnable: wire(p.runnable), plan: wire(p.plan),
        freeze_intent: wire(p.freeze_intent), drain_record: wire(p.drain_record),
        base: { ...p.template.selection.root, root: wire(p.template.selection.root.root) }, objects: bundle.objects.map(item => item.reference) };
    },
    async unpin(pin) { assert.ok(pins.delete(pin), "release only the exact admitted fixture clone"); },
    async read(ref) {
      assert.equal(pins.size, 1); const value = bytes.get(identity(ref));
      assert.ok(value, "inherited payload and old seed are not local transfer bytes");
      reads.push(identity(ref)); return Buffer.from(value);
    } };
}

async function exactClosure(f, bundle, receipt) {
  // All nine new fixture objects and four inherited/policy/profile Blobs are
  // reachable. Old base Source/Runnable/Directory and seed are audit-only.
  const expected = new Map([...bundle.objects, ...golden.base_objects.filter(item => item.reference.kind === "blob").map(item => ({ reference: item.reference, bytes: Buffer.from(item.hex, "hex") }))].map(item => [identity(item.reference), item]));
  assert.equal(expected.size, 13);
  const fresh = f.cas("account-a"), closure = await fresh.closure(bundle.publication);
  assert.deepEqual(new Set(closure.objects.map(identity)), new Set(expected.keys()));
  assert.equal(receipt.object_count, expected.size);
  assert.equal(receipt.total_bytes, [...expected.values()].reduce((sum, item) => sum + item.bytes.length, 0));
  const remote = f.client();
  for (const item of expected.values()) assert.deepEqual(await remote.getObject(item.reference), item.bytes);
  assert.equal(decode((await remote.getObject(wire(bundle.envelope.payload.runnable))).toString()).knowledge_cursor, 9007199254740999n);
}

test("real HTTP publishes the golden checkpoint DATA closure and preserves opaque high-u64 audit bytes", { timeout: 20_000 }, async t => {
  const f = await fixture(t); await f.seed(); const bundle = data(), source = transferSource(bundle);
  assert.match(golden.receipt_json, /"generation":9007199254740995(?:,|})/);
  const result = await f.client().uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, source);
  assert.deepEqual(source.receipts, [golden.receipt_json]); assert.equal(source.pins.size, 0);
  assert.equal(result.kind, "partial_checkpoint_data"); assert.equal(result.source_role, "quiesced_source_complement_data");
  assert.deepEqual(result.bundle.root, bundle.publication); assert.equal(result.bundle.root.kind, "action_result");
  assert.equal(Object.hasOwn(result, "receipt"), false); assert.equal(Object.hasOwn(result, "local"), false);
  assert.equal(result.uploaded_objects, bundle.objects.length);
  assert.deepEqual(new Set(source.reads), new Set(bundle.objects.map(item => identity(item.reference))));
  await f.restart();
  assert.deepEqual(await f.client().getRoot(bundle.publicationId), result.bundle);
  await exactClosure(f, bundle, result.bundle);
});

test("real committed HTTP publication with a lost acknowledgment reconciles the exact ID after handler restart", { timeout: 20_000 }, async t => {
  const f = await fixture(t); await f.seed(); const bundle = data(), source = transferSource(bundle);
  f.loseNextAcknowledgment(bundle.publicationId);
  await assert.rejects(f.client().uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, source), { code: "request_failed" });
  assert.equal(f.lostAcknowledgment(), bundle.publicationId); assert.equal(source.pins.size, 0);
  const committed = await f.cas("account-a").getRoot(bundle.publicationId);
  assert.deepEqual(committed.root, bundle.publication, "the server committed actual bytes before losing its TCP response");
  const reads = source.reads.length, start = f.requests.length;
  await f.restart();
  const result = await f.client().uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, source);
  assert.deepEqual(result.bundle, committed); assert.equal(result.uploaded_objects, 0); assert.equal(result.uploaded_bytes, 0);
  assert.equal(source.reads.length, reads); assert.equal(source.pins.size, 0);
  assert.ok(f.requests.slice(start).every(request => request.method === "GET"), "reconciliation performs no write or second uncertain publication");
  await exactClosure(f, bundle, committed);
});

test("real HTTP keeps account scope private and rejects rehashed malformed DATA evidence before publication", { timeout: 20_000 }, async t => {
  const f = await fixture(t); await f.seed(); const bundle = data();
  await assert.rejects(f.client("fixture-b").getRoot(f.selection.root.root_id), { status: 404 });
  await assert.rejects(f.client("fixture-b").getObject(golden.base_objects[0].reference), { status: 404 });
  const isolated = transferSource(bundle);
  await assert.rejects(f.client("fixture-b").uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, isolated), { status: 404 });
  assert.equal(isolated.pins.size, 0); assert.equal(isolated.reads.length, 0);
  const foreign = transferSource(bundle), start = f.requests.length;
  await assert.rejects(f.client("fixture-a", { ...f.selection.root.scope, tenant_id: "another-tenant" }).uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, foreign), { code: "scope_mismatch" });
  assert.equal(f.requests.length, start); assert.equal(foreign.pins.size, 0);

  // Deliberately faulty source adapter: real Native also refuses forged DATA.
  // Rebind public hashes/set receipts so the HTTP child-semantic validator,
  // rather than an unrelated stale parent SHA, must reject this audit field.
  const oldQ = wire(bundle.envelope.payload.q), oldFreeze = wire(bundle.envelope.payload.freeze_intent);
  const freeze = decode(golden.freeze_json); freeze.extra = "saved audit cannot confer live authority";
  const changedFreeze = describe("blob", freeze), q = decode(golden.q_json);
  q.freeze_intent = changedFreeze.reference;
  const evidence = [changedFreeze.reference, wire(q.drain_record)].sort((a, b) => a.digest.localeCompare(b.digest));
  q.evidence_retention.set_digest = sha(canonicalFabricMetadata(evidence));
  q.evidence_retention.total_bytes = BigInt(evidence.reduce((sum, ref) => sum + ref.size_bytes, 0));
  const changedQ = describe("blob", q), p = bundle.envelope.payload;
  p.q = changedQ.reference; p.freeze_intent = changedFreeze.reference;
  const order = { blob: 0, source_root: 1, runnable_root: 2 };
  bundle.envelope.references = [p.q, p.source, p.runnable, p.plan, p.freeze_intent, p.drain_record].sort((a, b) => order[a.kind] - order[b.kind] || a.digest.localeCompare(b.digest));
  const { publication_id: _, ...payload } = p;
  p.publication_id = `pcd1-${sha(canonicalFabricMetadata(payload))}`;
  const publication = describe("action_result", bundle.envelope);
  bundle.publication = publication.reference; bundle.publicationId = p.publication_id;
  bundle.objects = bundle.objects.filter(item => ![identity(oldQ), identity(oldFreeze), identity(golden.publication)].includes(identity(item.reference)));
  bundle.objects.push(changedFreeze, changedQ, publication);
  const faulty = transferSource(bundle);
  await assert.rejects(f.client().uploadPartialCheckpoint(bundle.publicationId, golden.receipt_json, faulty), { status: 400 });
  assert.equal(faulty.pins.size, 0);
  assert.equal(await f.cas("account-a").getRoot(bundle.publicationId), null);
  await assert.rejects(f.client().getRoot(bundle.publicationId), { status: 404 });
});
