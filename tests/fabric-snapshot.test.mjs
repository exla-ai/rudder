import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { FabricCas, FabricVerificationCache, MemoryFabricObjectStorage, canonicalFabricMetadata, parseCanonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";
import { decodeSourceV2, decodeEnvironmentV2, decodeRunnableRoot, snapshotReferences } from "../cloud/dist/fabric-snapshot.js";

const codec = { parseCanonical: parseCanonicalFabricMetadata, encodeCanonical: canonicalFabricMetadata };
const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
const meta = (kind, value) => { const bytes = canonicalFabricMetadata(value); return { value, bytes, object: ref(kind, bytes) }; };
const blob = (value) => { const bytes = Buffer.from(value); return { bytes, object: ref("blob", bytes) }; };
const dir = () => meta("directory", { format_version: 1, mode: 493, entries: [] });
function prepared() {
  const directory = dir(), policy = blob('{"source":"all"}'), profile = blob('{"profile":"test"}'), hardlinks = blob('{"format_version":1,"groups":[]}');
  const source = meta("source_root", { format_version: 2, consistency: "quiesced", directory: directory.object, projection_policy: policy.object });
  const environment = meta("environment_root", { format_version: 2, directory: directory.object, platform: { os: "linux", arch: "x86_64" }, installer_inputs: [policy.object, directory.object], provenance: {} });
  const root = meta("runnable_root", { format_version: 1, consistency: "quiesced", source: source.object, environment: environment.object, view_root: directory.object, profile: profile.object, hardlinks: hardlinks.object, knowledge_cursor: (1n << 64n) - 1n });
  return { directory, policy, profile, hardlinks, source, environment, root, all: [directory, policy, profile, hardlinks, source, environment, root] };
}
async function fixture(objects, cached = true) {
  const storage = new MemoryFabricObjectStorage();
  const cas = new FabricCas({ storage, accountId: "runnable-account", verificationCache: cached ? new FabricVerificationCache() : undefined });
  const lease = await cas.beginStaging();
  for (const item of objects) await cas.putObject(lease.lease_id, item.object, item.bytes);
  return { cas, lease };
}

test("ancestry-free launch publication preserves u64 cursors without growing with retained roots", async () => {
  const p = prepared(), oldDirectory = meta("directory", { format_version: 1, mode: 448, entries: [] });
  const old = meta("source_root", { format_version: 1, consistency: "quiesced", directory: oldDirectory.object, parents: [] });
  const { cas, lease } = await fixture([...p.all, oldDirectory, old]);
  await cas.publishRoot("old-history", lease.lease_id, old.object);
  assert.equal(decodeRunnableRoot(p.root.bytes, codec).knowledge_cursor, 18_446_744_073_709_551_615n);
  assert.equal(decodeSourceV2(p.source.bytes, codec).format_version, 2);
  assert.equal(decodeEnvironmentV2(p.environment.bytes, codec).format_version, 2);
  for (let index = 0; index < 12; index++) {
    const next = meta("runnable_root", { ...p.root.value, knowledge_cursor: BigInt(index) });
    const stage = await cas.beginStaging(); await cas.putObject(stage.lease_id, next.object, next.bytes);
    const receipt = await cas.publishRoot("launch-" + index, stage.lease_id, next.object);
    assert.equal(receipt.object_count, p.all.length);
    const closure = await cas.closure(next.object);
    assert(!closure.objects.some((object) => object.digest === old.object.digest));
  }
  assert.deepEqual((await cas.getRoot("old-history")).root, old.object);
});

for (const cached of [false, true]) test(`actual child versions reject old history-bearing descriptors even with retained proofs=${cached}`, async () => {
  const p = prepared();
  const sourceV1 = meta("source_root", { format_version: 1, consistency: "quiesced", directory: p.directory.object, parents: [] });
  const environmentV1 = meta("environment_root", { ...p.environment.value, format_version: 1, installer_inputs: [sourceV1.object] });
  const wrongSource = meta("runnable_root", { ...p.root.value, source: sourceV1.object });
  const wrongEnvironment = meta("runnable_root", { ...p.root.value, environment: environmentV1.object });
  const { cas, lease } = await fixture([...p.all, sourceV1, environmentV1, wrongSource, wrongEnvironment], cached);
  for (const [name, root] of [["wrong-source", wrongSource], ["wrong-environment", wrongEnvironment]]) {
    await assert.rejects(cas.publishRoot(name, lease.lease_id, root.object), { code: "invalid_metadata" });
    assert.equal(await cas.getRoot(name), null);
  }
});

test("history cannot enter through installer inputs, unknown fields or false consistency", () => {
  const p = prepared();
  for (const kind of ["source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result", "directory_shard"]) {
    const environment = meta("environment_root", { ...p.environment.value, installer_inputs: [{ ...p.policy.object, kind }] });
    assert.throws(() => decodeEnvironmentV2(environment.bytes, codec));
  }
  for (const value of [{ ...p.source.value, parents: [] }, { ...p.source.value, consistency: "best_effort" }]) assert.throws(() => decodeSourceV2(canonicalFabricMetadata(value), codec));
  for (const value of [{ ...p.root.value, parents: [] }, { ...p.root.value, durability: "replicated" }, { ...p.root.value, consistency: "best_effort" }]) assert.throws(() => decodeRunnableRoot(canonicalFabricMetadata(value), codec));
  assert.deepEqual(snapshotReferences("source_root", p.source.value, codec).map((r) => r.kind), ["directory", "blob"]);
});
