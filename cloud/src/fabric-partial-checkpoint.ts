/** Partial-checkpoint DATA parity, never a producer pin, live grant, accepted
 * source or complete-history proof. The caller owns authenticated account scope,
 * abortable I/O and the ordinary strong-edge closure (including leaf payloads).
 * Prior template refs are audit identities, not ancestry/seed strong edges.
 * These checks run on new closure validation, not historical generic root
 * receipt lookup or its identical-publication retry. Such receipts attest only
 * byte-closure durability and cannot confer these semantics or live authority. */
import { createHash } from "node:crypto";
import { performance } from "node:perf_hooks";
import { ContentFormatError, unsigned, validateContentReference, validateFileNode, fileNodeReferences, validateFileChild,
  type CanonicalMetadataCodec, type ContentObjectKind, type ContentObjectRef } from "./fabric-content.js";
import { directoryReferences, validateDirectoryChild } from "./fabric-directory.js";
import { snapshotReferences, validateSourceV2, validateRunnableRoot, type SourceV2, type RunnableRoot } from "./fabric-snapshot.js";

const PURPOSE = "partial-checkpoint-data-v1";
const ROLE = "quiesced_source_complement_data";
const PROTOCOL = "rudder_partial_workspace_v1";
const DOCUMENT = 64 * 1024, Q_LIMIT = 32 * 1024, PLAN_LIMIT = 4 * 1024 * 1024, DRAIN_LIMIT = 32 * 1024;
const U64 = (1n << 64n) - 1n, I64 = (1n << 63n) - 1n;
const MAX_OBJECTS = 16_384, MAX_BYTES = 4n * 1024n ** 3n;
const KINDS: ContentObjectKind[] = ["blob", "file", "directory", "directory_shard", "source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result"];
type ObjectData = Record<string, unknown>;
export type PartialCheckpointDataReader = (reference: ContentObjectRef, maxBytes: number) => Promise<Buffer>;
/** Must be bound to the authenticated CAS account by the host, not by payload. */
export type PartialCheckpointBaseResolver = (rootId: string) => Promise<ContentObjectRef | null>;
function fail(message = "Invalid partial checkpoint DATA"): never { throw new ContentFormatError(message); }
function record(value: unknown): ObjectData {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail();
  return value as ObjectData;
}
function exact(value: unknown, keys: readonly string[]): ObjectData {
  const object = record(value);
  if (Object.keys(object).length !== keys.length || keys.some(key => !Object.hasOwn(object, key))) fail("Unsupported partial checkpoint DATA field");
  return object;
}
function array(value: unknown, max: number): unknown[] { if (!Array.isArray(value) || value.length > max) fail(); return value; }
function version(value: unknown): void { if (unsigned(value) !== 1n) fail("Unsupported partial checkpoint DATA version"); }
function string(value: unknown, max = 512): string {
  if (typeof value !== "string" || !value.length || Buffer.byteLength(value) > max || /[\u0000-\u001f\u007f-\u009f]/u.test(value)) fail("Invalid partial checkpoint DATA identity");
  return value;
}
function uuid(value: unknown): string {
  const id = string(value, 36);
  if (!/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(id)) fail("Noncanonical partial checkpoint DATA UUID");
  return id;
}
function digest(value: unknown): string { if (typeof value !== "string" || !/^[a-f0-9]{64}$/.test(value)) fail("Invalid partial checkpoint DATA digest"); return value; }
function positive(value: unknown, max = U64): bigint { const n = unsigned(value, max); if (!n) fail(); return n; }
function hash(bytes: Buffer): string { return createHash("sha256").update(bytes).digest("hex"); }
/** Cap admission before the injected encoder constructs a whole untrusted tree. */
function canonical(value: unknown, codec: CanonicalMetadataCodec, limit = PLAN_LIMIT): Buffer {
  let cost = 0; const active = new Set<object>();
  const text = (value: string) => {
    cost += Buffer.byteLength(value); if (cost > limit) fail("Partial checkpoint DATA exceeds byte bound");
    for (const char of value) { const cp = char.codePointAt(0)!; if (cp >= 0xd800 && cp <= 0xdfff) fail(); }
  };
  const visit = (value: unknown, depth: number): void => {
    if (++cost > limit || depth > 128) fail("Partial checkpoint DATA exceeds admission bound");
    if (typeof value === "string") { text(value); return; }
    if (value === null || typeof value === "boolean") return;
    if (typeof value === "bigint") { if (value < -(1n << 63n) || value > U64) fail(); return; }
    if (typeof value === "number") { if (!Number.isSafeInteger(value) || Object.is(value, -0)) fail(); return; }
    if (!value || typeof value !== "object" || active.has(value)) fail();
    const proto = Object.getPrototypeOf(value); if (!Array.isArray(value) && proto !== null && proto !== Object.prototype) fail();
    if (Object.getOwnPropertySymbols(value).length) fail(); active.add(value);
    if (Array.isArray(value)) {
      if (value.length > limit || Object.keys(value).length !== value.length) fail();
      for (let i = 0; i < value.length; i++) { const property = Object.getOwnPropertyDescriptor(value, String(i)); if (!property || !("value" in property)) fail(); visit(property.value, depth + 1); }
    } else {
      for (const key of Object.keys(value)) { text(key); const property = Object.getOwnPropertyDescriptor(value, key); if (!property || !("value" in property)) fail(); visit(property.value, depth + 1); }
    }
    active.delete(value);
  };
  visit(value, 0);
  let bytes: Buffer; try { bytes = codec.encodeCanonical(value); } catch { return fail("Invalid partial checkpoint DATA encoding"); }
  if (!Buffer.isBuffer(bytes) || bytes.length > limit) fail("Partial checkpoint DATA exceeds byte bound");
  return bytes;
}
function equal(a: unknown, b: unknown, codec: CanonicalMetadataCodec): boolean { return canonical(a, codec).equals(canonical(b, codec)); }
function ref(value: unknown, kind?: ContentObjectKind, max = 256 * 1024 * 1024): ContentObjectRef {
  const r = validateContentReference(value);
  if ((kind && r.kind !== kind) || r.size_bytes > BigInt(max) || (r.kind !== "blob" && r.size_bytes > BigInt(PLAN_LIMIT)) || (["file", "directory_shard", "runnable_root"].includes(r.kind) && r.size_bytes > BigInt(DOCUMENT))) fail("Incorrect or oversized partial checkpoint DATA reference");
  return r;
}
function same(a: ContentObjectRef, b: ContentObjectRef): boolean { return a.kind === b.kind && a.algorithm === b.algorithm && a.digest === b.digest && a.size_bytes === b.size_bytes; }
function key(r: ContentObjectRef): string { return `${r.kind}:${r.digest}:${r.size_bytes}`; }
function compare(a: ContentObjectRef, b: ContentObjectRef): number {
  return KINDS.indexOf(a.kind) - KINDS.indexOf(b.kind) || (a.digest < b.digest ? -1 : a.digest > b.digest ? 1 : a.size_bytes < b.size_bytes ? -1 : a.size_bytes > b.size_bytes ? 1 : 0);
}
function sorted(values: ContentObjectRef[]): ContentObjectRef[] {
  if (values.length > MAX_OBJECTS) fail("Partial checkpoint DATA inventory exceeds bound");
  const refs = [...values].sort(compare).filter((r, i, all) => !i || !same(r, all[i - 1]));
  if (refs.reduce((sum, r) => sum + r.size_bytes, 0n) > MAX_BYTES) fail("Partial checkpoint DATA inventory exceeds byte bound");
  return refs;
}
function bytesRef(kind: ContentObjectKind, value: unknown, codec: CanonicalMetadataCodec): ContentObjectRef { const bytes = canonical(value, codec); return { kind, algorithm: "sha256", digest: hash(bytes), size_bytes: BigInt(bytes.length) }; }
function owner(value: unknown): ObjectData { const o = exact(value, ["owner", "epoch"]); if (!string(o.owner).trim()) fail(); positive(o.epoch); return o; }
function attempt(value: unknown): ObjectData {
  const a = exact(value, ["project_id", "run_id", "run_epoch", "owner", "execution_id", "operation_id", "view_id"]);
  string(a.project_id); string(a.run_id); positive(a.run_epoch); owner(a.owner); uuid(a.execution_id); uuid(a.operation_id); uuid(a.view_id); return a;
}
function rootBinding(value: unknown): ObjectData {
  const r = exact(value, ["scope", "root_id", "root"]), scope = exact(r.scope, ["provider_id", "tenant_id"]);
  for (const part of [scope.provider_id, scope.tenant_id]) if (typeof part !== "string" || !/^[A-Za-z0-9._:-]{1,128}$/.test(part)) fail("Invalid partial checkpoint DATA scope");
  string(r.root_id); ref(r.root, "runnable_root"); return r;
}
function observation(value: unknown, codec: CanonicalMetadataCodec): ObjectData {
  const o = exact(value, ["format_version", "attempt", "view_id", "owner", "namespace", "profile", "exclusion_policy_digest"]);
  version(o.format_version); const a = exact(o.attempt, ["execution_id", "operation_id"]); uuid(a.execution_id); uuid(a.operation_id); uuid(o.view_id); uuid(o.namespace); owner(o.owner); ref(o.profile, "blob", PLAN_LIMIT); digest(o.exclusion_policy_digest);
  if (o.exclusion_policy_digest !== hash(canonical({ format_version: 1, excluded_paths_hex: [] }, codec))) fail("Partial checkpoint DATA exclusion policy differs");
  return o;
}
function selection(value: unknown, codec: CanonicalMetadataCodec): ObjectData {
  const s = exact(value, ["format_version", "request_id", "root", "attempt", "observation", "projection_policy", "accepted_source", "accepted_seed"]);
  version(s.format_version); uuid(s.request_id); rootBinding(s.root); const a = attempt(s.attempt), o = observation(s.observation, codec);
  if (!equal(o.attempt, { execution_id: a.execution_id, operation_id: a.operation_id }, codec) || o.view_id !== a.view_id || !equal(o.owner, a.owner, codec)) fail("Partial checkpoint DATA observation differs from attempt");
  ref(s.projection_policy, "blob"); ref(s.accepted_source, "source_root");
  // A private historical seed is an audit identity only, outside transfer
  // budgets/strong edges. Its encoded u64 length does not authorize a read.
  if (validateContentReference(s.accepted_seed).kind !== "directory") fail();
  canonical(s, codec, 16 * 1024); return s;
}
interface Template { raw: ObjectData; selection: ObjectData; base: RunnableRoot; source: SourceV2; topology: ContentObjectRef }
function template(value: unknown, codec: CanonicalMetadataCodec): Template {
  const t = exact(value, ["format_version", "selection", "base_runnable", "base_source", "profile"]); version(t.format_version);
  const s = selection(t.selection, codec), base = validateRunnableRoot(t.base_runnable, codec), source = validateSourceV2(t.base_source, codec);
  const profile = exact(t.profile, ["format_version", "prepared", "scratch", "vcs_local"]); version(profile.format_version);
  for (const p of [profile.prepared, profile.scratch, profile.vcs_local]) if (array(p, 0).length) fail();
  const topology = bytesRef("blob", { format_version: 1, groups: [] }, codec), root = record(s.root), o = record(s.observation);
  if (!same(bytesRef("runnable_root", base, codec), ref(root.root)) || !same(bytesRef("source_root", source, codec), base.source) || !same(base.source, ref(s.accepted_source)) || !same(bytesRef("blob", profile, codec), base.profile) || !same(base.profile, ref(o.profile)) || base.environment !== null || !same(source.directory, base.view_root) || !same(source.projection_policy, ref(s.projection_policy)) || !same(base.hardlinks, topology)) fail("Partial checkpoint DATA template differs from exact source-only base");
  canonical(t, codec, DOCUMENT); return { raw: t, selection: s, base, source, topology };
}
interface Publication { payload: ObjectData; template: Template; refs: ContentObjectRef[]; q: ContentObjectRef; source: ContentObjectRef; runnable: ContentObjectRef; plan: ContentObjectRef; freeze: ContentObjectRef; drain: ContentObjectRef }
function publication(value: unknown, codec: CanonicalMetadataCodec): Publication | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const payloadValue = (value as ObjectData).payload;
  if (!payloadValue || typeof payloadValue !== "object" || Array.isArray(payloadValue) || (payloadValue as ObjectData).kind !== PURPOSE) return null;
  canonical(value, codec, DOCUMENT);
  const e = exact(value, ["format_version", "references", "payload"]), p = exact(e.payload, ["format_version", "kind", "source_role", "publication_id", "template", "q", "source", "runnable", "plan", "freeze_intent", "drain_record"]);
  version(e.format_version); version(p.format_version); if (p.source_role !== ROLE) fail("Unsupported partial checkpoint DATA source role");
  const t = template(p.template, codec), q = ref(p.q, "blob", Q_LIMIT), source = ref(p.source, "source_root", DOCUMENT), runnable = ref(p.runnable, "runnable_root", DOCUMENT), plan = ref(p.plan, "blob", PLAN_LIMIT), freeze = ref(p.freeze_intent, "blob", DOCUMENT), drain = ref(p.drain_record, "blob", DRAIN_LIMIT);
  const refs = sorted([q, source, runnable, plan, freeze, drain]);
  if (refs.length !== 6 || !equal(array(e.references, 6).map(v => ref(v)), refs, codec)) fail("Partial checkpoint DATA strong edges differ");
  const { publication_id: ignored, ...identity } = p; void ignored;
  if (p.publication_id !== `pcd1-${hash(canonical(identity, codec, DOCUMENT))}`) fail("Partial checkpoint DATA immutable id differs");
  return { payload: p, template: t, refs, q, source, runnable, plan, freeze, drain };
}
/** Strict purpose-specific inline validation. Other ActionResult purposes keep
 * their existing reader; none of the template audit refs become strong edges. */
export function partialCheckpointDataReferences(value: unknown, codec: CanonicalMetadataCodec): ContentObjectRef[] | null { return publication(value, codec)?.refs ?? null; }
function cursor(value: unknown): ObjectData { const c = exact(value, ["view_id", "owner", "namespace", "generation"]); uuid(c.view_id); owner(c.owner); uuid(c.namespace); unsigned(c.generation); return c; }
function stop(value: unknown): ObjectData {
  const s = exact(value, ["request_id", "execution_id", "operation_id", "view_id", "run_id", "run_epoch", "owner"]); uuid(s.request_id); uuid(s.execution_id); uuid(s.operation_id); uuid(s.view_id); string(s.run_id); positive(s.run_epoch, I64); owner(s.owner); return s;
}
function receipt(value: unknown, refs: ContentObjectRef[], codec: CanonicalMetadataCodec): ObjectData {
  const r = exact(value, ["format_version", "pin_id", "generation", "staging_id", "set_digest", "object_count", "total_bytes"]); version(r.format_version); string(r.pin_id); positive(r.generation, I64); uuid(r.staging_id); digest(r.set_digest);
  const set = sorted(refs);
  if (!set.length || unsigned(r.object_count, MAX_OBJECTS) !== BigInt(set.length) || unsigned(r.total_bytes, MAX_BYTES) !== set.reduce((sum, ref) => sum + ref.size_bytes, 0n) || r.set_digest !== hash(canonical(set, codec))) fail("Partial checkpoint DATA retention set differs");
  return r;
}
function qData(value: unknown, p: Publication, codec: CanonicalMetadataCodec): { q: ObjectData; capture: ObjectData; stop: ObjectData } {
  const q = exact(value, ["format_version", "capture", "stop", "observation", "execution_request_digest", "freeze_intent", "drain_record", "evidence_retention"]); version(q.format_version); digest(q.execution_request_digest);
  const c = exact(q.capture, ["format_version", "view_id", "owner", "attempt", "base", "begin", "end", "exclusion_policy_digest", "request_nonce", "previous_head", "head_sequence", "retention", "consistency"]); version(c.format_version);
  const a = attempt(c.attempt), b = rootBinding(c.base), o = observation(q.observation, codec), s = stop(q.stop), begin = cursor(c.begin); cursor(c.end); owner(c.owner); uuid(c.request_nonce); positive(c.head_sequence); digest(c.exclusion_policy_digest);
  if (c.previous_head !== null) string(c.previous_head);
  const retention = exact(c.retention, ["format_version", "plan", "local"]); version(retention.format_version);
  if (!same(ref(retention.plan, "blob", PLAN_LIMIT), p.plan) || !same(ref(q.freeze_intent), p.freeze) || !same(ref(q.drain_record), p.drain)) fail("Partial checkpoint DATA evidence differs from Q");
  if (!equal(b, p.template.selection.root, codec) || !equal(a, p.template.selection.attempt, codec) || !equal(o, p.template.selection.observation, codec) || !equal(c.begin, c.end, codec) || c.consistency !== "live_best_effort" || c.view_id !== a.view_id || !equal(c.owner, a.owner, codec) || begin.view_id !== c.view_id || !equal(begin.owner, c.owner, codec) || begin.namespace !== o.namespace || c.exclusion_policy_digest !== o.exclusion_policy_digest || s.execution_id !== a.execution_id || s.operation_id !== a.operation_id || s.view_id !== a.view_id || s.run_id !== a.run_id || !equal(s.run_epoch, a.run_epoch, codec) || !equal(s.owner, a.owner, codec) || c.request_nonce !== s.request_id) fail("Partial checkpoint DATA Q binding differs");
  receipt(q.evidence_retention, [p.freeze, p.drain], codec);
  return { q, capture: c, stop: s };
}
function freezeData(value: unknown, q: ObjectData, c: ObjectData, s: ObjectData, launch: string, codec: CanonicalMetadataCodec): void {
  const f = exact(value, ["format_version", "request", "launch_digest"]); version(f.format_version); const r = exact(f.request, ["format_version", "intent_id", "capture"]); version(r.format_version); uuid(r.intent_id);
  const capture = exact(r.capture, ["nonce", "expected_begin", "expected_head"]); uuid(capture.nonce); cursor(capture.expected_begin); if (capture.expected_head !== null) string(capture.expected_head);
  if (f.launch_digest !== launch || r.intent_id === s.request_id || capture.nonce !== s.request_id || !equal(capture.expected_begin, c.begin, codec) || capture.expected_head !== c.previous_head) fail("Partial checkpoint DATA freeze binding differs");
  void q;
}
function drainData(value: unknown, q: ObjectData, s: ObjectData, selection: ObjectData, launch: string, codec: CanonicalMetadataCodec): void {
  const d = exact(value, ["format_version", "execution_id", "operation_id", "view_id", "owner", "execution_request_digest", "launch_request_digest", "agent_exit", "stop", "partial_binding_digest", "endpoint_instance", "network", "kernel_drained"]); version(d.format_version); uuid(d.endpoint_instance); owner(d.owner); stop(d.stop);
  const n = exact(d.network, ["format_version", "profile_digest", "network_id", "readiness_nonce", "user", "network", "mode"]); version(n.format_version); digest(n.profile_digest); uuid(n.network_id); uuid(n.readiness_nonce);
  for (const v of [n.user, n.network]) { const id = exact(v, ["device", "inode"]); unsigned(id.device); unsigned(id.inode); }
  const exit = typeof d.agent_exit === "bigint" ? d.agent_exit : typeof d.agent_exit === "number" && Number.isSafeInteger(d.agent_exit) ? BigInt(d.agent_exit) : fail();
  if (exit < -2147483648n || exit > 2147483647n || d.kernel_drained !== true || n.mode !== "offline_v1" || d.execution_id !== s.execution_id || d.operation_id !== s.operation_id || d.view_id !== s.view_id || !equal(d.owner, s.owner, codec) || d.execution_request_digest !== q.execution_request_digest || d.launch_request_digest !== launch || !equal(d.stop, s, codec)) fail("Partial checkpoint DATA drain binding differs");
  const binding = { protocol: PROTOCOL, format_version: 1, launch_request_digest: launch, root: selection.root, attempt: selection.attempt, observation: selection.observation };
  if (d.partial_binding_digest !== hash(canonical(binding, codec))) fail("Partial checkpoint DATA partial binding digest differs");
}
/** The caller bounds/aborts actual I/O; timeouts here never claim settled I/O. */
class Reads {
  private readonly end = performance.now() + 30_000;
  private bytes = 0; private steps = 0;
  private readonly seen = new Map<string, Buffer>();
  constructor(private readonly read: PartialCheckpointDataReader, readonly codec: CanonicalMetadataCodec) {}
  check(): void { if (++this.steps > 500_000 || performance.now() >= this.end) fail("Partial checkpoint DATA proof budget exhausted"); }
  async bytesFor(r: ContentObjectRef, limit = DOCUMENT): Promise<Buffer> {
    this.check(); if (r.size_bytes > BigInt(limit)) fail("Partial checkpoint DATA proof metadata exceeds bound");
    const previous = this.seen.get(key(r)); if (previous) return previous;
    if (this.seen.size >= MAX_OBJECTS || this.bytes + Number(r.size_bytes) > 64 * 1024 * 1024) fail("Partial checkpoint DATA proof inventory exhausted");
    this.bytes += Number(r.size_bytes);
    const bytes = await this.read(r, limit); this.check();
    if (!Buffer.isBuffer(bytes) || BigInt(bytes.length) !== r.size_bytes || hash(bytes) !== r.digest) fail("Partial checkpoint DATA child bytes differ from reference");
    this.seen.set(key(r), bytes); return bytes;
  }
  async value(r: ContentObjectRef, limit = DOCUMENT): Promise<unknown> {
    const bytes = await this.bytesFor(r, limit); let value: unknown;
    try { value = this.codec.parseCanonical(bytes); } catch { return fail("Invalid canonical partial checkpoint DATA child"); }
    if (!canonical(value, this.codec, limit).equals(bytes)) fail("Noncanonical partial checkpoint DATA child"); return value;
  }
}
function legacyDirectory(value: unknown): ContentObjectRef[] {
  const d = exact(value, ["format_version", "mode", "entries"]); version(d.format_version); unsigned(d.mode, 0o7777); let previous: Buffer | undefined;
  return array(d.entries, 100_000).flatMap(item => {
    const entry = exact(item, ["name_hex", "node"]); if (typeof entry.name_hex !== "string" || !/^(?:[a-f0-9]{2})+$/.test(entry.name_hex)) fail();
    const name = Buffer.from(entry.name_hex, "hex"); if (name.includes(0) || name.includes(47) || name.equals(Buffer.from(".")) || name.equals(Buffer.from("..")) || (previous && Buffer.compare(previous, name) >= 0)) fail(); previous = name;
    const n = record(entry.node);
    if (n.type === "file") { exact(n, ["type", "object", "mode"]); unsigned(n.mode, 0o7777); return [ref(n.object, "blob")]; }
    if (n.type === "directory") { exact(n, ["type", "object"]); return [ref(n.object, "directory")]; }
    if (n.type === "symlink") { exact(n, ["type", "target_hex"]); if (typeof n.target_hex !== "string" || !/^(?:[a-f0-9]{2})+$/.test(n.target_hex) || Buffer.from(n.target_hex, "hex").includes(0)) fail(); return []; }
    return fail("Unsupported partial checkpoint DATA directory entry");
  });
}
function edges(r: ContentObjectRef, value: unknown, codec: CanonicalMetadataCodec): ContentObjectRef[] {
  if (r.kind === "directory") return unsigned(record(value).format_version) === 1n ? legacyDirectory(value) : directoryReferences("directory", value, codec);
  if (r.kind === "directory_shard") return directoryReferences("directory_shard", value, codec);
  if (r.kind === "file") return fileNodeReferences(canonical(value, codec, DOCUMENT), codec);
  if (r.kind === "runnable_root" || r.kind === "source_root") return snapshotReferences(r.kind, value, codec);
  return fail("Partial checkpoint DATA proof traverses unsupported metadata");
}
function edgeChild(parent: ContentObjectRef, value: unknown, child: ContentObjectRef, childValue: unknown, codec: CanonicalMetadataCodec): void {
  if ((parent.kind === "directory" && unsigned(record(value).format_version) === 2n) || parent.kind === "directory_shard") validateDirectoryChild(parent.kind as "directory" | "directory_shard", value, child, childValue, codec);
  else if (parent.kind === "file") {
    const node = validateFileNode(value); for (const [i, r] of node.children.entries()) if (same(r, child)) validateFileChild(node, i, childValue);
  }
}
async function provenance(plan: ObjectData, p: Publication, reads: Reads): Promise<void> {
  const codec = reads.codec, locals = array(plan.local_objects, 16_000).map(v => ref(v)), paths = array(plan.remote_boundaries, 4096).map(v => {
    const path = exact(v, ["reference", "edges"]), r = ref(path.reference); const indices = array(path.edges, 1024).map(v => Number(unsigned(v, 0xffffffffn))); return { reference: r, edges: indices };
  });
  if (!equal(sorted(locals), locals, codec)) fail("Partial checkpoint DATA local provenance is not sorted and unique");
  const local = new Map(locals.map(r => [key(r), r])), boundaries = new Map<string, ContentObjectRef>(); let prior: ContentObjectRef | undefined;
  for (const path of paths) {
    if ((prior && compare(prior, path.reference) >= 0) || local.has(key(path.reference))) fail("Partial checkpoint DATA boundary provenance repeats or overlaps");
    prior = path.reference; boundaries.set(key(path.reference), path.reference);
  }
  for (const r of [...locals, ...boundaries.values()]) if (!["blob", "file", "directory", "directory_shard"].includes(r.kind)) fail("Partial checkpoint DATA tree provenance has non-content kind");
  const directory = ref(plan.directory, "directory", PLAN_LIMIT), topology = ref(plan.hardlinks, "blob");
  for (const r of [directory, topology]) if (!local.has(key(r)) && !boundaries.has(key(r))) fail("Partial checkpoint DATA root lacks explicit provenance");
  // Replay only metadata parents from the authenticated immutable base. The
  // final inherited object is a reference boundary, never an old closure walk.
  for (const path of paths) {
    let current = ref(record(p.template.selection.root).root), previous: { r: ContentObjectRef; value: unknown } | undefined;
    for (const index of path.edges) {
      reads.check(); const value = await reads.value(current, current.kind === "directory" ? PLAN_LIMIT : DOCUMENT);
      if (previous) edgeChild(previous.r, previous.value, current, value, codec);
      const children = edges(current, value, codec); if (index >= children.length) fail("Partial checkpoint DATA remote path edge is absent");
      previous = { r: current, value }; current = ref(children[index]);
    }
    if (!same(current, path.reference)) fail("Partial checkpoint DATA boundary path differs from base");
  }
  const usedLocal = new Set<string>(), usedBoundary = new Set<string>(), pending = [directory, topology];
  while (pending.length) {
    reads.check(); const r = pending.pop()!, id = key(r); if (usedLocal.has(id) || usedBoundary.has(id)) continue;
    if (boundaries.has(id)) { usedBoundary.add(id); continue; }
    if (!local.has(id)) fail("Partial checkpoint DATA tree has no local/remote provenance"); usedLocal.add(id);
    if (r.kind === "blob") continue; const value = await reads.value(r, r.kind === "directory" ? PLAN_LIMIT : DOCUMENT);
    for (const child of edges(r, value, codec)) {
      reads.check(); if (!local.has(key(child)) && !boundaries.has(key(child))) fail("Partial checkpoint DATA child lacks provenance");
      if (child.kind !== "blob" && local.has(key(child))) edgeChild(r, value, child, await reads.value(child, child.kind === "directory" ? PLAN_LIMIT : DOCUMENT), codec);
      pending.push(child);
    }
  }
  if (usedLocal.size !== local.size || usedBoundary.size !== boundaries.size) fail("Partial checkpoint DATA plan contains unused provenance");
}
/** Validates canonical audit/plan/runnable child bytes and scoped base paths.
 * Receipt fields remain DATA; this cannot reconstruct any local active pin.
 * The caller must still verify the ordinary new strong-edge byte closure. */
export async function validatePartialCheckpointDataChildren(value: unknown, codec: CanonicalMetadataCodec, read: PartialCheckpointDataReader, resolveBase: PartialCheckpointBaseResolver): Promise<void> {
  const p = publication(value, codec); if (!p) return; const reads = new Reads(read, codec);
  const base = await resolveBase(string(record(p.template.selection.root).root_id)); reads.check();
  if (base === null || !same(ref(base), ref(record(p.template.selection.root).root))) fail("Partial checkpoint DATA scoped immutable base differs");
  const q = qData(await reads.value(p.q, Q_LIMIT), p, codec), plan = exact(await reads.value(p.plan, PLAN_LIMIT), ["format_version", "capture_id", "base", "attempt", "directory", "hardlinks", "local_objects", "remote_boundaries"]); version(plan.format_version);
  if (plan.capture_id !== q.stop.request_id || !equal(plan.base, q.capture.base, codec) || !equal(plan.attempt, q.capture.attempt, codec) || !same(ref(plan.hardlinks), p.template.topology)) fail("Partial checkpoint DATA plan binding differs");
  const locals = array(plan.local_objects, 16_000).map(v => ref(v)); receipt(record(q.capture.retention).local, [...locals, p.plan], codec);
  const launch = hash(canonical(p.template.selection, codec));
  freezeData(await reads.value(p.freeze), q.q, q.capture, q.stop, launch, codec);
  drainData(await reads.value(p.drain, DRAIN_LIMIT), q.q, q.stop, p.template.selection, launch, codec);
  const source = validateSourceV2(await reads.value(p.source), codec), runnable = validateRunnableRoot(await reads.value(p.runnable), codec);
  if (!same(source.directory, ref(plan.directory)) || !same(source.projection_policy, ref(p.template.selection.projection_policy)) || !same(runnable.source, p.source) || !same(runnable.view_root, ref(plan.directory)) || runnable.environment !== null || !same(runnable.profile, ref(record(q.q.observation).profile)) || !same(runnable.hardlinks, ref(plan.hardlinks)) || runnable.knowledge_cursor !== p.template.base.knowledge_cursor) fail("Partial checkpoint DATA source/runnable binding differs");
  // Bind actual stored base descriptors even when a path could be empty.
  if (!equal(await reads.value(ref(record(p.template.selection.root).root)), p.template.base, codec) || !equal(await reads.value(p.template.base.source), p.template.source, codec)) fail("Partial checkpoint DATA actual base descriptors differ");
  await provenance(plan, p, reads); reads.check();
}
