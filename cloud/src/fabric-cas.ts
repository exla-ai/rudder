/** Account-scoped immutable remote CAS. There is deliberately NO remote GC.
 * S3 has no transaction spanning lease, objects and receipt: publication writes
 * the immutable root receipt first and only then attempts lease cleanup. Data
 * and receipts must have no bucket lifecycle expiration or external deleter.
 */
import { createHash, randomUUID } from "node:crypto";
import { GetObjectCommand, PutObjectCommand, DeleteObjectCommand, type S3Client } from "@aws-sdk/client-s3";
import { readFile } from "node:fs/promises";
import { blobFileStream, checkedBlobStream, withVerifiedSpool } from "./fabric-stream.js";
import { ContentFormatError, fileNodeReferences, decodeFileNode, validateFileChild, validateContentReference,
  type ContentObjectKind } from "./fabric-content.js";
import { directoryReferences, validateDirectoryChild } from "./fabric-directory.js";
import { snapshotReferences, validateRunnableChild } from "./fabric-snapshot.js";
import { partialCheckpointDataReferences, validatePartialCheckpointDataChildren } from "./fabric-partial-checkpoint.js";

export const FABRIC_FORMAT_VERSION = 1;
export type FabricObjectKind = ContentObjectKind;
export interface FabricObjectRef { kind: FabricObjectKind; algorithm: "sha256"; digest: string; size_bytes: number }
export interface FabricLease { format_version: 1; lease_id: string; expires_at_ms: number }
export interface FabricRootReceipt { format_version: 1; root_id: string; root: FabricObjectRef; durability: "replicated"; published_at_ms: number; object_count: number; total_bytes: number }
export interface FabricStoredObject { body: Buffer; etag: string }
export interface FabricObjectStorage {
  get(key: string, maxBytes: number): Promise<FabricStoredObject | null>;
  putIfAbsent(key: string, bytes: Buffer): Promise<boolean>;
  replaceIfMatch(key: string, etag: string, bytes: Buffer): Promise<boolean>;
  delete(key: string): Promise<void>;
  /** Streaming operations keep compiler-sized blobs out of the control heap. */
  readStream?(key: string, maxBytes: number, consume: (body: AsyncIterable<Uint8Array>) => Promise<void>): Promise<boolean>;
  putFileIfAbsent?(key: string, file: string, reference: FabricObjectRef): Promise<boolean>;
}
export interface FabricCasLimits {
  maxObjectBytes: number; maxMetadataBytes: number; maxMissingBatch: number;
  maxClosureObjects: number; maxClosureBytes: number; maxClosureEdges: number;
  maxClosureDepth: number; maxConcurrency: number; closureTimeoutMs: number;
}
export const DEFAULT_FABRIC_LIMITS: Readonly<FabricCasLimits> = Object.freeze({
  maxObjectBytes: 256 * 1024 * 1024, maxMetadataBytes: 4 * 1024 * 1024, maxMissingBatch: 256,
  maxClosureObjects: 100_000, maxClosureBytes: 4 * 1024 ** 3, maxClosureEdges: 500_000,
  maxClosureDepth: 1024, maxConcurrency: 4, closureTimeoutMs: 60_000,
});
export class FabricCasError extends Error {
  constructor(public readonly code: string, message: string, public readonly status = 400) { super(message); this.name = "FabricCasError"; }
}
const error = (code: string, message: string, status = 400): never => { throw new FabricCasError(code, message, status); };
const digest = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
const kinds = new Set<FabricObjectKind>(["blob", "file", "directory", "directory_shard", "source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result"]);
const record = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value)) error("invalid_metadata", "Expected an object");
  return value as Record<string, unknown>;
};
function exact(value: unknown, keys: string[]): Record<string, unknown> {
  const object = record(value);
  if (Object.keys(object).length !== keys.length || keys.some((key) => !Object.hasOwn(object, key))) error("invalid_metadata", "Unsupported or missing metadata field");
  return object;
}
function integer(value: unknown, max = Number.MAX_SAFE_INTEGER): number {
  if (typeof value === "bigint" && value >= 0n && value <= BigInt(max)) return Number(value);
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0 || value > max) error("invalid_metadata", "Unsigned integer is out of range");
  return value as number;
}
export function validateFabricObjectRef(value: unknown, maxBytes = DEFAULT_FABRIC_LIMITS.maxObjectBytes): FabricObjectRef {
  const object = exact(value, ["kind", "algorithm", "digest", "size_bytes"]);
  if (!kinds.has(object.kind as FabricObjectKind) || object.algorithm !== "sha256" || typeof object.digest !== "string" || !/^[a-f0-9]{64}$/.test(object.digest)) error("invalid_reference", "Invalid typed SHA-256 object reference");
  const size_bytes = integer(object.size_bytes, maxBytes);
  if ((object.kind === "file" || object.kind === "directory_shard" || object.kind === "runnable_root") && size_bytes > 64 * 1024) error("too_large", "Content descriptor exceeds its format byte limit", 413);
  return { kind: object.kind as FabricObjectKind, algorithm: "sha256", digest: object.digest as string, size_bytes };
}
const identity = (reference: FabricObjectRef) => `${reference.kind}:${reference.digest}:${reference.size_bytes}`;
const sameRef = (a: FabricObjectRef, b: FabricObjectRef) => identity(a) === identity(b);

interface RetentionProof { children: readonly FabricObjectRef[]; metadata?: Buffer; weight: number }
interface ProofCacheState { entries: Map<string, RetentionProof>; bytes: number; hits: number; misses: number; backends: WeakMap<FabricObjectStorage, number>; nextBackend: number; maxBytes: number }
const proofCaches = new WeakMap<FabricVerificationCache, ProofCacheState>();
/** Opt in ONLY after verifying immutable storage has no GC, lifecycle expiry,
 * alternate writers or external deletion. This is retained verification evidence,
 * not a fresh integrity scrub. Budgets include conservative per-entry overhead;
 * stats report estimated retained heap, not an OS RSS allocation guarantee. */
export class FabricVerificationCache {
  constructor(maxBytes = 32 * 1024 * 1024) {
    if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 128 * 1024 * 1024) error("invalid_limits", "Invalid proof-cache budget");
    proofCaches.set(this, { entries: new Map(), bytes: 0, hits: 0, misses: 0, backends: new WeakMap(), nextBackend: 1, maxBytes });
  }
  clear(): void { const state = proofCaches.get(this)!; state.entries.clear(); state.bytes = 0; }
  stats(): { entries: number; estimated_bytes: number; max_bytes: number; hits: number; misses: number } {
    const state = proofCaches.get(this)!; return { entries: state.entries.size, estimated_bytes: state.bytes, max_bytes: state.maxBytes, hits: state.hits, misses: state.misses };
  }
}
function proofKey(cache: FabricVerificationCache, backend: FabricObjectStorage, scope: string, reference: FabricObjectRef): string {
  const state = proofCaches.get(cache)!; let id = state.backends.get(backend);
  if (!id) { id = state.nextBackend++; state.backends.set(backend, id); }
  return `${id}:${scope}:${identity(reference)}`;
}
function proofReadEntry(cache: FabricVerificationCache | undefined, backend: FabricObjectStorage, scope: string, reference: FabricObjectRef): RetentionProof | undefined {
  if (!cache) return undefined;
  const state = proofCaches.get(cache)!; const key = proofKey(cache, backend, scope, reference); const value = state.entries.get(key);
  if (!value) { state.misses++; return undefined; }
  state.hits++; state.entries.delete(key); state.entries.set(key, value); return value;
}
function proofRead(cache: FabricVerificationCache | undefined, backend: FabricObjectStorage, scope: string, reference: FabricObjectRef): readonly FabricObjectRef[] | undefined {
  return proofReadEntry(cache, backend, scope, reference)?.children;
}
function proofForget(cache: FabricVerificationCache | undefined, backend: FabricObjectStorage, scope: string, reference: FabricObjectRef): void {
  if (!cache) return; const state = proofCaches.get(cache)!; const key = proofKey(cache, backend, scope, reference); const previous = state.entries.get(key); if (previous) { state.bytes -= previous.weight; state.entries.delete(key); }
}
function proofRemember(cache: FabricVerificationCache | undefined, backend: FabricObjectStorage, scope: string, reference: FabricObjectRef, children: FabricObjectRef[], bytes?: Buffer): void {
  if (!cache) return;
  proofForget(cache, backend, scope, reference);
  const state = proofCaches.get(cache)!; const key = proofKey(cache, backend, scope, reference);
  // Retained per-object verification is not a closure/semantic-edge proof.
  // Bounded verified metadata lets publication check actual parent/child spans
  // even when their raw bytes need no repeated immutable-store fetch.
  const metadata = reference.kind !== "blob" && bytes && bytes.byteLength <= 64 * 1024 ? Buffer.from(bytes) : undefined;
  const weight = 512 + key.length * 2 + (metadata?.byteLength ?? 0) + children.reduce((total, child) => total + 512 + JSON.stringify(child).length * 2, 0);
  if (weight > state.maxBytes) return;
  while (state.bytes + weight > state.maxBytes) { const oldest = state.entries.keys().next().value!; state.bytes -= state.entries.get(oldest)!.weight; state.entries.delete(oldest); }
  state.entries.set(key, { weight, metadata, children: Object.freeze(children.map((child) => Object.freeze({ ...child }))) }); state.bytes += weight;
}
function hexBytes(value: unknown, name = false): Buffer {
  if (typeof value !== "string" || !/^(?:[0-9a-f]{2})*$/.test(value)) error("invalid_metadata", "Byte fields must be lowercase hexadecimal");
  const bytes = Buffer.from(value as string, "hex");
  if (name && (!bytes.length || bytes.includes(0) || bytes.includes(47) || bytes.equals(Buffer.from(".")) || bytes.equals(Buffer.from("..")))) error("unsafe_name", "Unsafe directory entry name");
  return bytes;
}
const array = (value: unknown): unknown[] => { if (!Array.isArray(value)) error("invalid_metadata", "Expected an array"); return value as unknown[]; };
const version = (value: unknown) => { if (integer(value) !== 1) error("unsupported_version", "Unsupported fabric format version"); };
const mode = (value: unknown) => integer(value, 0o7777);
function stringMap(value: unknown): Record<string, unknown> { const object = record(value); if (Object.values(object).some((item) => typeof item !== "string")) error("invalid_metadata", "Expected a string map"); return object; }

// Lossless JSON integers and UTF-8 key ordering match Rust serde_json, including
// u64 knowledge cursors. JSON.parse alone silently rounds integers above 2^53.
type Json = null | boolean | string | bigint | Json[] | { [key: string]: Json };
function parseCanonical(bytes: Buffer): Json {
  const text = bytes.toString("utf8"); let offset = 0;
  function parse(depth = 0): Json {
    if (depth > 128) error("invalid_metadata", "Metadata nesting is too deep");
    const start = offset; const c = text[offset];
    if (c === '"') {
      offset++;
      while (offset < text.length) { if (text[offset] === "\\") { offset += 2; continue; } if (text[offset++] === '"') break; }
      let value: unknown; try { value = JSON.parse(text.slice(start, offset)); } catch { error("invalid_metadata", "Invalid JSON string"); }
      if (typeof value !== "string" || [...value].some((char) => { const cp = char.codePointAt(0)!; return cp >= 0xd800 && cp <= 0xdfff; })) error("invalid_metadata", "Invalid Unicode string");
      return value as string;
    }
    if (c === "[") { offset++; const values: Json[] = []; if (text[offset] === "]") { offset++; return values; } while (true) { values.push(parse(depth + 1)); if (text[offset++] === "]") break; if (text[offset - 1] !== ",") error("invalid_metadata", "Invalid JSON array"); } return values; }
    if (c === "{") { offset++; const values: { [key: string]: Json } = Object.create(null); if (text[offset] === "}") { offset++; return values; } while (true) { const key = parse(depth + 1); if (typeof key !== "string" || Object.hasOwn(values, key) || text[offset++] !== ":") error("invalid_metadata", "Invalid or duplicate JSON key"); values[key as string] = parse(depth + 1); if (text[offset++] === "}") break; if (text[offset - 1] !== ",") error("invalid_metadata", "Invalid JSON object"); } return values; }
    for (const [token, value] of [["null", null], ["true", true], ["false", false]] as const) if (text.startsWith(token, offset)) { offset += token.length; return value; }
    const number = /^-?(?:0|[1-9][0-9]*)/.exec(text.slice(offset));
    if (!number || number[0] === "-0") error("invalid_metadata", "Metadata numbers must be canonical integers");
    if (number![0].length > 20) error("invalid_metadata", "Metadata integer exceeds serde_json range");
    offset += number![0].length; const value = BigInt(number![0]);
    if (value < -(1n << 63n) || value > (1n << 64n) - 1n) error("invalid_metadata", "Metadata integer exceeds serde_json range");
    return value;
  }
  const value = parse();
  if (offset !== text.length || !Buffer.from(encodeJson(value)).equals(bytes)) error("invalid_metadata", "Metadata must be canonical compact JSON");
  return value;
}
function encodeJson(value: unknown): string {
  if (typeof value === "bigint") return value.toString();
  if (typeof value === "number") { if (!Number.isSafeInteger(value)) error("invalid_metadata", "Unsafe or non-integer metadata number"); return String(value); }
  if (value === null || typeof value === "boolean" || typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(encodeJson).join(",")}]`;
  const object = record(value); return `{${Object.keys(object).sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b))).map((key) => `${JSON.stringify(key)}:${encodeJson(object[key])}`).join(",")}}`;
}
export { parseCanonical as parseCanonicalFabricMetadata };
export function canonicalFabricMetadata(value: unknown): Buffer { return Buffer.from(encodeJson(value)); }
const metadataCodec = { parseCanonical, encodeCanonical: canonicalFabricMetadata };
function contentFormat<T>(read: () => T): T {
  try { return read(); }
  catch (failure) { if (failure instanceof ContentFormatError) return error("invalid_metadata", failure.message); throw failure; }
}

/** Verify schema and return all declared edges. Unknown kinds/fields fail closed. */
export function fabricReferences(reference: FabricObjectRef, bytes: Buffer, metadataLimit = DEFAULT_FABRIC_LIMITS.maxMetadataBytes): FabricObjectRef[] {
  validateFabricObjectRef(reference);
  if (bytes.length !== reference.size_bytes || digest(bytes) !== reference.digest) error("checksum_mismatch", "Object size or SHA-256 mismatch", 422);
  if (reference.kind === "blob") return [];
  if (bytes.length > metadataLimit) error("too_large", "Metadata object exceeds the byte limit", 413);
  const value = parseCanonical(bytes); const refs: FabricObjectRef[] = [];
  const ref = (value: unknown, kind?: FabricObjectKind) => { const parsed = validateFabricObjectRef(value); if (kind && parsed.kind !== kind) error("invalid_metadata", "Referenced object has the wrong kind"); refs.push(parsed); };
  if (reference.kind === "runnable_root" || (reference.kind === "source_root" || reference.kind === "environment_root") && record(value).format_version === 2n) {
    return contentFormat(() => snapshotReferences(reference.kind as "source_root" | "environment_root" | "runnable_root", value, metadataCodec).map((child) => validateFabricObjectRef(child)));
  } else if (reference.kind === "file") {
    return contentFormat(() => fileNodeReferences(bytes, metadataCodec).map((child) => validateFabricObjectRef(child)));
  } else if (reference.kind === "directory_shard" || reference.kind === "directory" && record(value).format_version === 2n) {
    return contentFormat(() => directoryReferences(reference.kind as "directory" | "directory_shard", value, metadataCodec).map((child) => validateFabricObjectRef(child)));
  } else if (reference.kind === "directory") {
    const object = exact(value, ["format_version", "mode", "entries"]); version(object.format_version); mode(object.mode);
    let previous: Buffer | null = null;
    for (const item of array(object.entries)) {
      const entry = exact(item, ["name_hex", "node"]); const name = hexBytes(entry.name_hex, true);
      if (previous && Buffer.compare(previous, name) >= 0) error("invalid_metadata", "Directory names must be unique and byte-sorted"); previous = name;
      const node = record(entry.node);
      if (node.type === "file") { exact(node, ["type", "object", "mode"]); mode(node.mode); ref(node.object, "blob"); }
      else if (node.type === "directory") { exact(node, ["type", "object"]); ref(node.object, "directory"); }
      else if (node.type === "symlink") { exact(node, ["type", "target_hex"]); const target = hexBytes(node.target_hex); if (!target.length || target.includes(0)) error("invalid_metadata", "Invalid symlink target"); }
      else error("invalid_metadata", "Unsupported source entry type");
    }
  } else if (reference.kind === "source_root") {
    const object = exact(value, ["format_version", "directory", "parents", "consistency"]); version(object.format_version); ref(object.directory, "directory"); for (const parent of array(object.parents)) ref(parent, "source_root"); if (object.consistency !== "quiesced") error("invalid_metadata", "Only quiesced source roots are supported");
  } else if (reference.kind === "environment_root") {
    const object = exact(value, ["format_version", "directory", "platform", "installer_inputs", "provenance"]); version(object.format_version); ref(object.directory, "directory"); if (!Object.keys(stringMap(object.platform)).length) error("invalid_metadata", "Platform/ABI identity is required"); stringMap(object.provenance); for (const item of array(object.installer_inputs)) ref(item);
  } else if (reference.kind === "checkpoint") {
    const checkpointVersion = record(value).format_version;
    const keys = ["format_version", "source", "environment", "parents", "consistency", "durability", "knowledge_cursor"];
    if (checkpointVersion === 2n || checkpointVersion === 3n) keys.push("view_root", "profile");
    if (checkpointVersion === 3n) keys.push("hardlinks");
    const object = exact(value, keys); if (checkpointVersion !== 2n && checkpointVersion !== 3n) version(object.format_version); ref(object.source, "source_root"); if (object.environment !== null) ref(object.environment, "environment_root"); for (const item of array(object.parents)) ref(item, "checkpoint"); if (object.consistency !== "quiesced" || !["local", "replicated"].includes(object.durability as string)) error("invalid_metadata", "Invalid checkpoint consistency or durability"); if (typeof object.knowledge_cursor !== "bigint" || object.knowledge_cursor < 0n || object.knowledge_cursor >= 1n << 64n) error("invalid_metadata", "Invalid knowledge cursor");
    if (checkpointVersion === 2n || checkpointVersion === 3n) { ref(object.view_root, "directory"); ref(object.profile, "blob"); }
    if (checkpointVersion === 3n) ref(object.hardlinks, "blob");
  } else {
    if (reference.kind === "action_result") {
      const checkpoint = contentFormat(() => partialCheckpointDataReferences(value, metadataCodec));
      if (checkpoint !== null) return checkpoint.map((child) => validateFabricObjectRef(child));
    }
    const object = exact(value, ["format_version", "references", "payload"]); version(object.format_version); for (const item of array(object.references)) ref(item);
  }
  return refs;
}

function validRootId(value: string): string { if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(value)) error("invalid_root_id", "Invalid root publication identifier"); return value; }
function validLeaseId(value: string): string { if (typeof value !== "string" || !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(value)) error("invalid_lease", "Invalid staging lease identifier"); return value; }
function ttl(value: number): number { if (!Number.isSafeInteger(value) || value < 1000 || value > 3_600_000) error("invalid_lease", "Lease duration must be between one second and one hour"); return value; }

export class FabricCas {
  readonly limits: Readonly<FabricCasLimits>;
  private readonly prefix: string;
  constructor(private readonly options: { storage: FabricObjectStorage; accountId: string; limits?: Partial<FabricCasLimits>; now?: () => number; verificationCache?: FabricVerificationCache }) {
    if (typeof options.accountId !== "string" || !options.accountId.trim() || options.accountId.length > 512 || /[\x00-\x1f\x7f]/.test(options.accountId)) error("invalid_namespace", "Trusted account scope is required");
    this.prefix = `fabric/v1/accounts/${digest(Buffer.from(options.accountId))}`;
    const limits = { ...DEFAULT_FABRIC_LIMITS, ...options.limits };
    for (const [key, value] of Object.entries(limits)) if (!Number.isSafeInteger(value) || value < 1 || value > DEFAULT_FABRIC_LIMITS[key as keyof FabricCasLimits] * (key === "maxConcurrency" ? 2 : 1)) error("invalid_limits", "Fabric limits may only reduce defaults (concurrency at most eight)");
    this.limits = Object.freeze(limits);
  }
  private now(): number { return integer((this.options.now || Date.now)()); }
  private objectKey(reference: FabricObjectRef): string { return `${this.prefix}/objects/sha256/${reference.digest}`; }
  private leaseKey(id: string): string { return `${this.prefix}/staging/${validLeaseId(id)}.json`; }
  private rootKey(id: string): string { return `${this.prefix}/roots/${validRootId(id)}.json`; }
  private reference(value: unknown): FabricObjectRef { const ref = validateFabricObjectRef(value, this.limits.maxObjectBytes); if (ref.kind !== "blob" && ref.size_bytes > this.limits.maxMetadataBytes) error("too_large", "Metadata exceeds byte limit", 413); return ref; }
  private async readLease(id: string): Promise<{ lease: FabricLease; etag: string }> {
    const stored = await this.options.storage.get(this.leaseKey(id), 4096);
    if (!stored) error("lease_missing", "Staging lease is unavailable", 409);
    const raw = exact(parseCanonical(stored!.body), ["format_version", "lease_id", "expires_at_ms"]); version(raw.format_version); if (raw.lease_id !== id) error("invalid_lease", "Stored lease identity mismatch", 503);
    const lease: FabricLease = { format_version: 1, lease_id: id, expires_at_ms: integer(raw.expires_at_ms) };
    if (lease.expires_at_ms <= this.now()) error("lease_expired", "Staging lease expired", 409);
    return { lease, etag: stored!.etag };
  }
  async beginStaging(ttlMs = 300_000): Promise<FabricLease> {
    const lease: FabricLease = { format_version: 1, lease_id: randomUUID(), expires_at_ms: this.now() + ttl(ttlMs) };
    if (!await this.options.storage.putIfAbsent(this.leaseKey(lease.lease_id), canonicalFabricMetadata(lease))) error("lease_conflict", "Could not allocate staging lease", 409);
    return lease;
  }
  async renewStaging(id: string, ttlMs = 300_000): Promise<FabricLease> {
    ttl(ttlMs);
    for (let attempt = 0; attempt < 3; attempt++) {
      const current = await this.readLease(id); const lease = { ...current.lease, expires_at_ms: Math.max(current.lease.expires_at_ms, this.now() + ttlMs) };
      if (await this.options.storage.replaceIfMatch(this.leaseKey(id), current.etag, canonicalFabricMetadata(lease))) return lease;
    }
    return error("lease_conflict", "Concurrent lease renewal requires retry", 409);
  }
  /** Account-scoped and idempotent; immutable objects and roots are untouched. */
  async abandonStaging(id: string): Promise<void> {
    await this.options.storage.delete(this.leaseKey(id));
  }
  async getObject(value: unknown): Promise<Buffer> {
    const reference = this.reference(value);
    try {
      const stored = await this.options.storage.get(this.objectKey(reference), reference.size_bytes);
      if (!stored) error("object_missing", "Referenced object is unavailable", 404);
      const children = fabricReferences(reference, stored!.body, this.limits.maxMetadataBytes);
      proofRemember(this.options.verificationCache, this.options.storage, this.prefix, reference, children, stored!.body);
      return stored!.body;
    } catch (failure) { proofForget(this.options.verificationCache, this.options.storage, this.prefix, reference); throw failure; }
  }
  /** Bytes may reach a reader before final integrity verification. A reader
   * must verify them before publishing locally; failed streams are closed. */
  async withBlob(value: unknown, consume: (body: AsyncIterable<Uint8Array>) => Promise<void>): Promise<void> {
    const reference = this.reference(value);
    if (reference.kind !== "blob") error("invalid_reference", "Streaming requires a raw blob");
    try {
      if (this.options.storage.readStream) {
        const found = await this.options.storage.readStream(this.objectKey(reference), reference.size_bytes,
          (body) => checkedBlobStream(reference, body, consume));
        if (!found) error("object_missing", "Referenced object is unavailable", 404);
      } else {
        const bytes = await this.getObject(reference);
        await checkedBlobStream(reference, (async function* () { yield bytes; })(), consume);
      }
      proofRemember(this.options.verificationCache, this.options.storage, this.prefix, reference, []);
    } catch (failure) { proofForget(this.options.verificationCache, this.options.storage, this.prefix, reference); throw failure; }
  }
  private async verifyObject(reference: FabricObjectRef): Promise<readonly FabricObjectRef[]> {
    if (reference.kind === "blob") {
      await this.withBlob(reference, async (body) => { for await (const _chunk of body) { /* hash without buffering */ } });
      return [];
    }
    return fabricReferences(reference, await this.getObject(reference), this.limits.maxMetadataBytes);
  }
  async putBlobStream(leaseId: string, value: unknown, body: AsyncIterable<Uint8Array>): Promise<{ stored: boolean }> {
    const reference = this.reference(value);
    if (reference.kind !== "blob") error("invalid_reference", "Streaming requires a raw blob");
    await this.readLease(leaseId);
    return withVerifiedSpool(reference, body, async (file) => {
      // The body deadline and a slow client cannot outlive staging authority.
      await this.readLease(leaseId);
      const stored = this.options.storage.putFileIfAbsent
        ? await this.options.storage.putFileIfAbsent(this.objectKey(reference), file, reference)
        : await this.options.storage.putIfAbsent(this.objectKey(reference), await readFile(file));
      if (!stored) await this.verifyObject(reference);
      proofRemember(this.options.verificationCache, this.options.storage, this.prefix, reference, []);
      return { stored };
    });
  }
  async putObject(leaseId: string, value: unknown, bytes: Buffer): Promise<{ stored: boolean }> {
    const payload = Buffer.from(bytes);
    const reference = this.reference(value); await this.readLease(leaseId);
    if (payload.length > this.limits.maxObjectBytes) error("too_large", "Object exceeds byte limit", 413);
    const children = fabricReferences(reference, payload, this.limits.maxMetadataBytes);
    const stored = await this.options.storage.putIfAbsent(this.objectKey(reference), payload);
    if (!stored) { const existing = await this.getObject(reference); if (!existing.equals(payload)) error("checksum_mismatch", "Existing immutable object differs", 422); }
    else proofRemember(this.options.verificationCache, this.options.storage, this.prefix, reference, children, payload);
    return { stored };
  }
  async missing(values: unknown[]): Promise<FabricObjectRef[]> {
    if (!Array.isArray(values) || values.length > this.limits.maxMissingBatch) error("too_large", "Missing-object batch exceeds the limit", 413);
    const refs = values.map((value) => this.reference(value));
    const missing: FabricObjectRef[] = [];
    await parallel(refs, this.limits.maxConcurrency, async (reference) => { if (proofRead(this.options.verificationCache, this.options.storage, this.prefix, reference) !== undefined) return; try { await this.verifyObject(reference); } catch (failure) { if (failure instanceof FabricCasError && failure.code === "object_missing") missing.push(reference); else throw failure; } });
    return missing.sort((a, b) => identity(a).localeCompare(identity(b)));
  }
  async closure(value: unknown): Promise<{ objects: FabricObjectRef[]; total_bytes: number }> {
    const controller = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    try { return await Promise.race([this.verifyClosure(value, controller.signal), new Promise<never>((_, reject) => { timer = setTimeout(() => { controller.abort(); reject(new FabricCasError("verification_timeout", "Root closure verification exceeded its deadline", 503)); }, this.limits.closureTimeoutMs); })]); }
    finally { if (timer) clearTimeout(timer); }
  }
  private async semanticMetadata(reference: FabricObjectRef, signal: AbortSignal): Promise<Buffer> {
    if (signal.aborted) error("verification_timeout", "Root closure verification exceeded its deadline", 503);
    return proofReadEntry(this.options.verificationCache, this.options.storage, this.prefix, reference)?.metadata ?? this.getObject(reference);
  }
  private async verifySemanticEdges(reference: FabricObjectRef, signal: AbortSignal): Promise<void> {
    if (reference.kind !== "action_result" && reference.kind !== "runnable_root" && reference.kind !== "file" && reference.kind !== "directory_shard" && !(reference.kind === "directory" && reference.size_bytes <= 4096)) return;
    const bytes = await this.semanticMetadata(reference, signal);
    const value = parseCanonical(bytes);
    if (reference.kind === "action_result") {
      if (contentFormat(() => partialCheckpointDataReferences(value, metadataCodec)) === null) return;
      const current = () => { if (signal.aborted) error("verification_timeout", "Root closure verification exceeded its deadline", 503); };
      try {
        await validatePartialCheckpointDataChildren(value, metadataCodec, async (child, maxBytes) => {
          current();
          const actual = this.reference(child);
          if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 4 * 1024 * 1024 || actual.size_bytes > Math.min(maxBytes, this.limits.maxMetadataBytes)) error("too_large", "Checkpoint proof metadata exceeds its byte limit", 413);
          const body = await this.semanticMetadata(actual, signal);
          current();
          return body;
        }, async (rootId) => {
          current();
          const base = await this.getRoot(rootId);
          current();
          return base ? validateContentReference(base.root) : null;
        });
      } catch (failure) {
        if (failure instanceof ContentFormatError) error("invalid_metadata", failure.message);
        throw failure;
      }
    } else if (reference.kind === "runnable_root") {
      const refs = contentFormat(() => snapshotReferences("runnable_root", value, metadataCodec));
      await parallel(refs.filter((child) => child.kind === "source_root" || child.kind === "environment_root").map((child) => this.reference(child)), this.limits.maxConcurrency, async (child) => {
        const actual = await this.semanticMetadata(child, signal);
        contentFormat(() => validateRunnableChild(value, child, actual, metadataCodec));
      });
    } else if (reference.kind === "file") {
      const parent = contentFormat(() => decodeFileNode(bytes, metadataCodec));
      if (parent.level === 0) return;
      await parallel(parent.children.map((child, index) => ({ child: this.reference(child), index })), this.limits.maxConcurrency, async ({ child, index }) => {
        const actual = parseCanonical(await this.semanticMetadata(child, signal));
        contentFormat(() => validateFileChild(parent, index, actual));
      });
    } else {
      if (reference.kind === "directory" && record(value).format_version !== 2n) return;
      const kind = reference.kind as "directory" | "directory_shard";
      const children = contentFormat(() => directoryReferences(kind, value, metadataCodec));
      const unique = new Map(children.filter((child) => child.kind === "file" || child.kind === "directory_shard").map((child) => { const ref = this.reference(child); return [identity(ref), ref] as const; }));
      await parallel([...unique.values()], this.limits.maxConcurrency, async (child) => {
        const actual = parseCanonical(await this.semanticMetadata(child, signal));
        contentFormat(() => validateDirectoryChild(kind, value, child, actual, metadataCodec));
      });
    }
  }
  private async verifyClosure(value: unknown, signal: AbortSignal): Promise<{ objects: FabricObjectRef[]; total_bytes: number }> {
    const root = this.reference(value); const pending = [root]; const objects = new Map<string, FabricObjectRef>(); const edges = new Map<string, string[]>(); let total = 0, edgeCount = 0;
    while (pending.length) {
      if (signal.aborted) error("verification_timeout", "Root closure verification exceeded its deadline", 503);
      const batch: FabricObjectRef[] = [];
      while (pending.length && batch.length < this.limits.maxConcurrency) {
        const next = this.reference(pending.pop()!); const key = identity(next); if (objects.has(key)) continue;
        objects.set(key, next); total += next.size_bytes;
        if (objects.size > this.limits.maxClosureObjects || total > this.limits.maxClosureBytes) error("too_large", "Root closure exceeds object or byte limits", 413);
        batch.push(next);
      }
      const children = await Promise.all(batch.map(async (reference) => {
        const known = proofRead(this.options.verificationCache, this.options.storage, this.prefix, reference);
        if (known !== undefined) return [reference, known] as const;
        return [reference, await this.verifyObject(reference)] as const;
      }));
      if (signal.aborted) error("verification_timeout", "Root closure verification exceeded its deadline", 503);
      for (const [reference, refs] of children) {
        edgeCount += refs.length; if (edgeCount > this.limits.maxClosureEdges) error("too_large", "Root closure exceeds edge limit", 413);
        edges.set(identity(reference), refs.map(identity)); pending.push(...refs);
      }
    }
    // Topological validation detects cycles and computes the longest path, not
    // merely the first/shortest traversal of a shared subtree.
    const indegree = new Map([...objects.keys()].map((key) => [key, 0]));
    for (const targets of edges.values()) for (const key of targets) indegree.set(key, (indegree.get(key) || 0) + 1);
    const ready = [...indegree].filter(([, degree]) => degree === 0).map(([key]) => key); const depths = new Map([[identity(root), 0]]); let visited = 0;
    while (ready.length) { const key = ready.pop()!; visited++; const depth = depths.get(key) || 0; if (depth > this.limits.maxClosureDepth) error("too_large", "Root closure exceeds graph depth limit", 413); for (const child of edges.get(key) || []) { depths.set(child, Math.max(depths.get(child) || 0, depth + 1)); indegree.set(child, indegree.get(child)! - 1); if (indegree.get(child) === 0) ready.push(child); } }
    if (visited !== objects.size) error("invalid_metadata", "Cyclic object closure is unsupported");
    // Admit/count/hash the graph before any additional semantic child reads.
    // Each parent's bounded child batch owns the same global concurrency budget.
    for (const reference of objects.values()) await this.verifySemanticEdges(reference, signal);
    return { objects: [...objects.values()].sort((a, b) => identity(a).localeCompare(identity(b))), total_bytes: total };
  }
  async getRoot(rootId: string): Promise<FabricRootReceipt | null> {
    const stored = await this.options.storage.get(this.rootKey(rootId), 4096); if (!stored) return null;
    const object = exact(parseCanonical(stored.body), ["format_version", "root_id", "root", "durability", "published_at_ms", "object_count", "total_bytes"]); version(object.format_version);
    if (object.root_id !== rootId || object.durability !== "replicated" || integer(object.object_count) < 1 || integer(object.object_count) > DEFAULT_FABRIC_LIMITS.maxClosureObjects || integer(object.total_bytes) > DEFAULT_FABRIC_LIMITS.maxClosureBytes) error("invalid_receipt", "Stored publication receipt is invalid", 503);
    return { format_version: 1, root_id: rootId, root: this.reference(object.root), durability: "replicated", published_at_ms: integer(object.published_at_ms), object_count: integer(object.object_count), total_bytes: integer(object.total_bytes) };
  }
  async publishRoot(rootId: string, leaseId: string, value: unknown): Promise<FabricRootReceipt> {
    const root = this.reference(value); const previous = await this.getRoot(rootId);
    if (previous) { if (!sameRef(previous.root, root)) error("root_conflict", "Root identifier already refers to different content", 409); return previous; }
    // Another identical publisher can commit and clean up this lease while we
    // verify. Recheck its immutable receipt before surfacing lease loss.
    const checkLease = async (): Promise<FabricRootReceipt | null> => {
      try { await this.readLease(leaseId); return null; }
      catch (failure) { const winner = await this.getRoot(rootId); if (!winner) throw failure; if (!sameRef(winner.root, root)) error("root_conflict", "Root identifier already refers to different content", 409); return winner; }
    };
    const raced = await checkLease(); if (raced) return raced;
    const closure = await this.closure(root); const completed = await checkLease(); if (completed) return completed;
    const receipt: FabricRootReceipt = { format_version: 1, root_id: validRootId(rootId), root, durability: "replicated", published_at_ms: this.now(), object_count: closure.objects.length, total_bytes: closure.total_bytes };
    if (!await this.options.storage.putIfAbsent(this.rootKey(rootId), canonicalFabricMetadata(receipt))) {
      const winner = await this.getRoot(rootId); if (!winner) error("storage_unavailable", "Conditional publication requires retry", 503); if (!sameRef(winner!.root, root)) error("root_conflict", "Root identifier already refers to different content", 409); return winner!;
    }
    // A cleanup failure cannot turn a committed publication into a failure or
    // remove its data. Expired/orphaned leases are retained; no sweeper exists.
    await this.options.storage.delete(this.leaseKey(leaseId)).catch(() => { });
    return receipt;
  }
}

async function parallel<T>(values: T[], concurrency: number, action: (value: T) => Promise<void>): Promise<void> {
  let cursor = 0; let failure: unknown;
  await Promise.all(Array.from({ length: Math.min(concurrency, values.length) }, async () => { while (cursor < values.length && !failure) { const value = values[cursor++]; try { await action(value); } catch (error) { failure = error; } } }));
  if (failure) throw failure;
}

/** In-memory test adapter; conditional operations are atomic in one JS turn. */
export class MemoryFabricObjectStorage implements FabricObjectStorage {
  private readonly objects = new Map<string, FabricStoredObject>();
  async get(key: string, maxBytes: number): Promise<FabricStoredObject | null> { const value = this.objects.get(key); if (!value) return null; if (value.body.length > maxBytes) error("too_large", "Stored object exceeds read limit", 413); return { body: Buffer.from(value.body), etag: value.etag }; }
  async putIfAbsent(key: string, bytes: Buffer): Promise<boolean> { if (this.objects.has(key)) return false; this.objects.set(key, { body: Buffer.from(bytes), etag: digest(bytes) }); return true; }
  async replaceIfMatch(key: string, etag: string, bytes: Buffer): Promise<boolean> { if (this.objects.get(key)?.etag !== etag) return false; this.objects.set(key, { body: Buffer.from(bytes), etag: digest(bytes) }); return true; }
  async delete(key: string): Promise<void> { this.objects.delete(key); }
}

/** AWS S3 general-purpose bucket adapter. Conditional writes are required; a
 * provider which ignores If-Match/If-None-Match cannot satisfy this contract. */
export class S3FabricObjectStorage implements FabricObjectStorage {
  private active = 0;
  constructor(private readonly options: { client: S3Client; bucket: string; timeoutMs?: number; maxConcurrency?: number }) {
    if (!options.bucket.trim() || !Number.isInteger(options.maxConcurrency ?? 16) || (options.maxConcurrency ?? 16) < 1 || (options.maxConcurrency ?? 16) > 32 || !Number.isInteger(options.timeoutMs ?? 20_000) || (options.timeoutMs ?? 20_000) < 1 || (options.timeoutMs ?? 20_000) > 120_000) error("invalid_storage", "Invalid S3 storage configuration");
    // Backend identity is part of a retained proof's scope. Do not let mutation
    // of the caller's constructor object silently switch its bucket later.
    this.options = Object.freeze({ ...options });
  }
  private async bounded<T>(action: (signal: AbortSignal) => Promise<T>): Promise<T> {
    if (this.active >= (this.options.maxConcurrency ?? 16)) error("storage_busy", "Object storage concurrency limit reached", 429);
    this.active++; const controller = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    const work = Promise.resolve().then(() => action(controller.signal)).finally(() => { this.active--; });
    try { return await Promise.race([work, new Promise<never>((_, reject) => { timer = setTimeout(() => { controller.abort(); reject(new FabricCasError("storage_timeout", "Object storage request timed out", 503)); }, this.options.timeoutMs ?? 20_000); })]); }
    catch (failure) { if (failure instanceof FabricCasError) throw failure; return error("storage_unavailable", "Object storage request failed", 503); }
    finally { if (timer) clearTimeout(timer); }
  }
  async get(key: string, maxBytes: number): Promise<FabricStoredObject | null> {
    return this.bounded(async (signal) => {
      let response; try { response = await this.options.client.send(new GetObjectCommand({ Bucket: this.options.bucket, Key: key }), { abortSignal: signal }); } catch (failure) { if (awsStatus(failure) === 404) return null; throw failure; }
      if ((response.ContentLength ?? 0) > maxBytes) { (response.Body as { destroy?: () => void })?.destroy?.(); error("too_large", "Stored object exceeds byte limit", 413); }
      if (!response.Body || !response.ETag) { (response.Body as { destroy?: () => void })?.destroy?.(); error("storage_unavailable", "Object storage returned an incomplete response", 503); }
      const body = response.Body!;
      const abort = () => (body as { destroy?: (error?: Error) => void }).destroy?.(new FabricCasError("storage_timeout", "Metadata read exceeded its deadline", 503));
      signal.addEventListener("abort", abort, { once: true });
      try {
        if (signal.aborted) abort();
        const chunks: Buffer[] = []; let bytes = 0;
        for await (const chunk of body as AsyncIterable<Uint8Array>) { const part = Buffer.from(chunk); bytes += part.length; if (bytes > maxBytes) error("too_large", "Stored object exceeds byte limit", 413); chunks.push(part); }
        return { body: Buffer.concat(chunks), etag: response.ETag! };
      } finally { signal.removeEventListener("abort", abort); (body as { destroy?: () => void }).destroy?.(); }
    });
  }
  async readStream(key: string, maxBytes: number, consume: (body: AsyncIterable<Uint8Array>) => Promise<void>): Promise<boolean> {
    return this.bounded(async (signal) => {
      let response;
      try { response = await this.options.client.send(new GetObjectCommand({ Bucket: this.options.bucket, Key: key }), { abortSignal: signal }); }
      catch (failure) { if (awsStatus(failure) === 404) return false; throw failure; }
      if (!response.Body || !response.ETag) { (response.Body as { destroy?: () => void })?.destroy?.(); error("storage_unavailable", "Object storage returned an incomplete response", 503); }
      const body = response.Body!;
      const abort = () => (body as { destroy?: (error?: Error) => void }).destroy?.(new FabricCasError("storage_timeout", "Blob read exceeded its deadline", 503));
      signal.addEventListener("abort", abort, { once: true });
      try {
        if (signal.aborted) abort();
        if (response.ContentLength !== undefined && response.ContentLength > maxBytes) error("too_large", "Stored blob exceeds byte limit", 413);
        await consume(body as AsyncIterable<Uint8Array>);
        return true;
      } finally { signal.removeEventListener("abort", abort); (body as { destroy?: () => void }).destroy?.(); }
    });
  }
  async putFileIfAbsent(key: string, file: string, reference: FabricObjectRef): Promise<boolean> {
    return this.bounded(async (signal) => {
      const body = blobFileStream(file);
      try {
        await this.options.client.send(new PutObjectCommand({ Bucket: this.options.bucket, Key: key,
          Body: body, ContentLength: reference.size_bytes, ChecksumSHA256: Buffer.from(reference.digest, "hex").toString("base64"),
          ContentType: "application/octet-stream", ServerSideEncryption: "AES256", IfNoneMatch: "*" }), { abortSignal: signal });
        return true;
      } catch (failure) { if ([409, 412].includes(awsStatus(failure))) return false; throw failure; }
      finally { body.destroy(); }
    });
  }
  private async put(key: string, bytes: Buffer, condition: { IfNoneMatch: "*" } | { IfMatch: string }): Promise<boolean> {
    return this.bounded(async (signal) => {
      try { await this.options.client.send(new PutObjectCommand({ Bucket: this.options.bucket, Key: key, Body: bytes, ContentLength: bytes.length, ContentType: "application/octet-stream", ChecksumSHA256: createHash("sha256").update(bytes).digest("base64"), ServerSideEncryption: "AES256", ...condition }), { abortSignal: signal }); return true; }
      catch (failure) { if ([409, 412].includes(awsStatus(failure))) return false; throw failure; }
    });
  }
  putIfAbsent(key: string, bytes: Buffer): Promise<boolean> { return this.put(key, bytes, { IfNoneMatch: "*" }); }
  replaceIfMatch(key: string, etag: string, bytes: Buffer): Promise<boolean> { return this.put(key, bytes, { IfMatch: etag }); }
  async delete(key: string): Promise<void> { await this.bounded(async (signal) => { await this.options.client.send(new DeleteObjectCommand({ Bucket: this.options.bucket, Key: key }), { abortSignal: signal }); }); }
}
function awsStatus(error: unknown): number { return Number((error as { $metadata?: { httpStatusCode?: number } })?.$metadata?.httpStatusCode || ((error as { name?: string })?.name === "NoSuchKey" ? 404 : 0)); }
