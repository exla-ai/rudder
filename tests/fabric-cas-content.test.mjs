import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  FabricCas, FabricVerificationCache, MemoryFabricObjectStorage,
  canonicalFabricMetadata, validateFabricObjectRef,
} from "../cloud/dist/fabric-cas.js";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const object = (kind, bytes) => ({ kind, algorithm: "sha256", digest: hash(bytes), size_bytes: bytes.length });
const metadata = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { value, bytes, object: object(kind, bytes) }; };
const blob = (bytes) => ({ bytes, object: object("blob", bytes) });
const file = (logical_size, children, level = 0) => metadata("file", { format_version: 1, layout: "fixed_4mib_fanout256_v1", logical_size, level, children });
const entry = (name, content, logical_size = content.object.size_bytes) => ({ name_hex: Buffer.from(name).toString("hex"), node: { type: "file", mode: 0o644, object: content.object, logical_size } });
const leaf = (entries) => {
  let prefix_hex = "";
  if (entries.length) {
    const first = Buffer.from(entries[0].name_hex, "hex"), last = Buffer.from(entries.at(-1).name_hex, "hex");
    let length = 0; while (length < first.length && length < last.length && first[length] === last[length]) length++;
    prefix_hex = first.subarray(0, length).toString("hex");
  }
  return metadata("directory_shard", { format_version: 1, prefix_hex,
    summary: { entry_count: entries.length, subdirectory_count: 0, entries_bytes: entries.reduce((n, e) => n + canonicalFabricMetadata(e).length, 0) },
    node: { type: "leaf", entries } });
};
const directory = (shard, extra = {}) => metadata("directory", { format_version: 2, mode: 0o755, entry_count: shard.value.summary.entry_count, subdirectory_count: 0, directory_shard: shard.object, ...extra });
class CountReads extends MemoryFabricObjectStorage {
  objectReads = 0;
  async get(key, limit) { if (key.includes("/objects/")) this.objectReads++; return super.get(key, limit); }
}
async function setup(objects, cached) {
  const storage = new CountReads();
  const verificationCache = cached ? new FabricVerificationCache() : undefined;
  const cas = new FabricCas({ storage, verificationCache, accountId: "chunked-account" });
  const lease = await cas.beginStaging();
  for (const item of objects) await cas.putObject(lease.lease_id, item.object, item.bytes);
  return { cas, lease, storage, verificationCache };
}

test("cached chunked closure publishes repeated chunks and keeps descriptor identity distinct from logical length", async () => {
  const chunk = blob(Buffer.alloc(4 * 1024 * 1024, 7)), tail = blob(Buffer.from("end"));
  const full = file(1024 ** 3, Array(256).fill(chunk.object)), short = file(3, [tail.object]);
  const content = file(1024 ** 3 + 3, [full.object, short.object], 1);
  const shard = leaf([entry("large", content, 1024 ** 3 + 3)]), root = directory(shard);
  const { cas, lease, storage, verificationCache } = await setup([chunk, tail, full, short, content, shard, root], true);
  const reopened = new FabricCas({ storage, verificationCache, accountId: "chunked-account" });
  const receipt = await reopened.publishRoot("chunked", lease.lease_id, root.object);
  assert.equal(receipt.object_count, 7);
  assert.equal(receipt.total_bytes, [chunk, tail, full, short, content, shard, root].reduce((n, o) => n + o.bytes.length, 0));
  assert.equal(storage.objectReads, 0, "bounded verified metadata must support real edge checks without re-fetching cached objects");
  assert.notEqual(content.object.size_bytes, 1024 ** 3 + 3);
  assert.deepEqual(await reopened.getRoot("chunked"), receipt);
});

for (const cached of [false, true]) {
  test(`publication rejects forged File child spans with retained proofs=${cached}`, async () => {
    const chunk = blob(Buffer.alloc(4 * 1024 * 1024, 9)), tail = blob(Buffer.from("four"));
    const full = file(1024 ** 3, Array(256).fill(chunk.object)), wrong = file(4, [tail.object]);
    const parent = file(1024 ** 3 + 3, [full.object, wrong.object], 1);
    const { cas, lease } = await setup([chunk, tail, full, wrong, parent], cached);
    await assert.rejects(cas.publishRoot("wrong-span", lease.lease_id, parent.object), { code: "invalid_metadata" });
    assert.equal(await cas.getRoot("wrong-span"), null);
  });

  test(`publication rejects actual shard counts and every repeated File alias with retained proofs=${cached}`, async () => {
    const raw = blob(Buffer.from("abc")), content = file(3, [raw.object]);
    const shard = leaf([entry("a", content, 3), entry("b", content, 4)]), wrongLength = directory(shard);
    const correctShard = leaf([entry("c", content, 3)]), wrongCount = directory(correctShard, { entry_count: 2 });
    const { cas, lease } = await setup([raw, content, shard, wrongLength, correctShard, wrongCount], cached);
    for (const [name, root] of [["wrong-length", wrongLength], ["wrong-count", wrongCount]]) {
      await assert.rejects(cas.publishRoot(name, lease.lease_id, root.object), { code: "invalid_metadata" });
      assert.equal(await cas.getRoot(name), null);
    }
  });
}

test("publication validates actual radix child ranges even when parent summaries and every hash are valid", async () => {
  const raw = blob(Buffer.from("x"));
  const a = leaf(Array.from({ length: 250 }, (_, i) => entry("a" + String(i).padStart(3, "0"), raw)));
  const b = leaf(Array.from({ length: 250 }, (_, i) => entry("b" + String(i).padStart(3, "0"), raw)));
  const link = (edge, shard) => ({ edge, shard: { digest: shard.object.digest, size_bytes: shard.object.size_bytes, summary: shard.value.summary } });
  const branch = metadata("directory_shard", { format_version: 1, prefix_hex: "",
    summary: { entry_count: 500, subdirectory_count: 0, entries_bytes: a.value.summary.entries_bytes + b.value.summary.entries_bytes },
    node: { type: "branch", children: [link(98, b), link(99, a)] } });
  const root = directory(branch);
  const { cas, lease } = await setup([raw, a, b, branch, root], true);
  await assert.rejects(cas.publishRoot("wrong-route", lease.lease_id, root.object), { code: "invalid_metadata" });
  assert.equal(await cas.getRoot("wrong-route"), null);
});

test("descriptor format limits apply before storage access", () => {
  for (const kind of ["file", "directory_shard"]) {
    assert.throws(() => validateFabricObjectRef({ kind, algorithm: "sha256", digest: "a".repeat(64), size_bytes: 65537 }), { code: "too_large" });
  }
});
