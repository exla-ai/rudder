/** Authenticated bounded CAS transfer. The source adapter must pin the root
 * durably and return a NEW owned pin identifier, never a caller's existing pin.
 * This client does not mount filesystems or run an agent during transfer.
 */
import { createHash } from "node:crypto";

export type FabricObjectKind = "blob" | "file" | "directory" | "directory_shard" | "source_root" | "environment_root" | "checkpoint" | "runnable_root" | "action_key" | "action_result";
export interface FabricObjectRef { kind: FabricObjectKind; algorithm: "sha256"; digest: string; size_bytes: number }
export interface FabricLease { format_version: 1; lease_id: string; expires_at_ms: number }
export interface FabricRootReceipt { format_version: 1; root_id: string; root: FabricObjectRef; durability: "replicated"; published_at_ms: number; object_count: number; total_bytes: number }
export interface FabricTransferSource {
  pin(root: FabricObjectRef): Promise<string>;
  unpin(ownedPinId: string): Promise<void>;
  closure(root: FabricObjectRef): Promise<FabricObjectRef[]>;
  read(reference: FabricObjectRef): Promise<Uint8Array>;
}
export interface FabricScope { provider_id: string; tenant_id: string }
export interface FabricPartialInventory {
  format_version: 1; owned_pin: string; plan: FabricObjectRef;
  directory: FabricObjectRef; hardlinks: FabricObjectRef;
  base: { scope: FabricScope; root_id: string; root: FabricObjectRef };
  objects: FabricObjectRef[];
}
export interface FabricPartialSource {
  /** Keep the native receipt opaque: its generations may exceed JS integers. */
  pinPartial(receiptJson: string): Promise<FabricPartialInventory>;
  unpin(ownedPinId: string): Promise<void>;
  read(reference: FabricObjectRef): Promise<Uint8Array>;
}
/** This inventory retains a checkpoint DATA bundle. Its ActionResult root is
 * not a RunnableRoot receipt, a complete local pin or accepted source history. */
export interface FabricPartialCheckpointInventory {
  format_version: 1; owned_pin: string; publication_id: string;
  publication: FabricObjectRef; q: FabricObjectRef; source: FabricObjectRef;
  runnable: FabricObjectRef; plan: FabricObjectRef; freeze_intent: FabricObjectRef;
  drain_record: FabricObjectRef; base: FabricPartialInventory["base"];
  objects: FabricObjectRef[];
}
export interface FabricPartialCheckpointSource {
  pinPartialCheckpoint(receiptJson: string): Promise<FabricPartialCheckpointInventory>;
  unpin(ownedPinId: string): Promise<void>;
  read(reference: FabricObjectRef): Promise<Uint8Array>;
}
export interface FabricPartialCheckpointUploadResult {
  format_version: 1; kind: "partial_checkpoint_data";
  source_role: "quiesced_source_complement_data"; publication_id: string;
  publication: FabricObjectRef; runnable: FabricObjectRef; q: FabricObjectRef;
  bundle: FabricRootReceipt; uploaded_objects: number; uploaded_bytes: number;
  cleanup_warning?: string;
}
export interface FabricTransferResult { receipt: FabricRootReceipt; uploaded_objects: number; uploaded_bytes: number; cleanup_warning?: string }
export interface FabricLocalReceipt { format_version: 1; root: FabricObjectRef; pin: string; generation: number; durability: "local" }
export interface FabricTransferTarget {
  beginStaging(ttlMs: number): Promise<FabricLease>;
  renewStaging(leaseId: string, ttlMs: number): Promise<FabricLease>;
  /** Must verify raw checksum, canonical typed schema, and the live lease. */
  receive(leaseId: string, object: FabricObjectRef, bytes: Uint8Array): Promise<FabricObjectRef[]>;
  /** Verify/sync the full reachable closure and atomically retain its root. */
  publish(leaseId: string, pin: string, root: FabricObjectRef): Promise<FabricLocalReceipt>;
  abandonStaging(leaseId: string): Promise<void>;
}
export interface FabricDownloadResult { receipt: FabricRootReceipt; local: FabricLocalReceipt; downloaded_objects: number; downloaded_bytes: number }
export class FabricRemoteError extends Error {
  constructor(public readonly code: string, message: string, public readonly status?: number) { super(message); this.name = "FabricRemoteError"; }
}
const fail = (code: string, message: string): never => { throw new FabricRemoteError(code, message); };
const kinds = new Set(["blob", "file", "directory", "directory_shard", "source_root", "environment_root", "checkpoint", "runnable_root", "action_key", "action_result"]);
const identity = (object: FabricObjectRef) => `${object.kind}:${object.digest}:${object.size_bytes}`;
function scope(value: unknown): FabricScope {
  const item = value as FabricScope;
  if (!item || Object.keys(item).length !== 2 || [item.provider_id, item.tenant_id].some((part) => typeof part !== "string" || !/^[A-Za-z0-9._:-]{1,128}$/.test(part))) fail("invalid_scope", "Invalid provider and tenant binding");
  return { provider_id: item.provider_id, tenant_id: item.tenant_id };
}
export function validateFabricPartialInventory(value: unknown): FabricPartialInventory {
  const item = value as FabricPartialInventory;
  if (!item || Object.keys(item).length !== 7 || item.format_version !== 1 || typeof item.owned_pin !== "string" || !/^transfer:partial:rpc:[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(item.owned_pin) || !item.base || Object.keys(item.base).length !== 3 || !Array.isArray(item.objects) || item.objects.length < 1 || item.objects.length > 16_384) fail("invalid_source", "Invalid partial source inventory");
  const plan = validateFabricReference(item.plan), directory = validateFabricReference(item.directory), hardlinks = validateFabricReference(item.hardlinks);
  const base = { scope: scope(item.base.scope), root_id: rootId(item.base.root_id), root: validateFabricReference(item.base.root) };
  if (plan.kind !== "blob" || directory.kind !== "directory" || hardlinks.kind !== "blob" || base.root.kind !== "runnable_root") fail("invalid_source", "Invalid partial source object kinds");
  const objects = item.objects.map(validateFabricReference); const seen = new Set<string>(); let total = 0;
  for (const object of objects) { const key = identity(object); if (seen.has(key)) fail("invalid_source", "Partial inventory repeats an object"); seen.add(key); total += object.size_bytes; }
  if (!seen.has(identity(plan)) || total > 4 * 1024 ** 3) fail("invalid_source", "Partial inventory is incomplete or exceeds byte limit");
  return { format_version: 1, owned_pin: item.owned_pin, plan, directory, hardlinks, base, objects };
}
const partialCheckpointPin = /^transfer:partial-checkpoint:rpc:[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/;
function partialCheckpointId(value: string): string {
  if (typeof value !== "string" || !/^pcd1-[a-f0-9]{64}$/.test(value)) fail("invalid_source", "Invalid checkpoint data publication identity");
  return value;
}
export function validateFabricPartialCheckpointInventory(value: unknown): FabricPartialCheckpointInventory {
  const keys = ["format_version", "owned_pin", "publication_id", "publication", "q", "source", "runnable", "plan", "freeze_intent", "drain_record", "base", "objects"];
  const item = value as FabricPartialCheckpointInventory;
  if (!item || typeof item !== "object" || Array.isArray(item) || Object.keys(item).length !== keys.length
    || keys.some((key) => !Object.hasOwn(item, key)) || item.format_version !== 1
    || typeof item.owned_pin !== "string" || !partialCheckpointPin.test(item.owned_pin)
    || !item.base || typeof item.base !== "object" || Array.isArray(item.base) || Object.keys(item.base).length !== 3
    || ["scope", "root_id", "root"].some((key) => !Object.hasOwn(item.base, key))
    || !Array.isArray(item.objects) || item.objects.length < 1 || item.objects.length > 16_384) fail("invalid_source", "Invalid checkpoint data transfer inventory");
  const publication_id = partialCheckpointId(item.publication_id);
  const publication = validateFabricReference(item.publication), q = validateFabricReference(item.q);
  const source = validateFabricReference(item.source), runnable = validateFabricReference(item.runnable);
  const plan = validateFabricReference(item.plan), freeze_intent = validateFabricReference(item.freeze_intent), drain_record = validateFabricReference(item.drain_record);
  const base = { scope: scope(item.base.scope), root_id: rootId(item.base.root_id), root: validateFabricReference(item.base.root) };
  if (publication.kind !== "action_result" || publication.size_bytes > 64 * 1024 || source.kind !== "source_root"
    || runnable.kind !== "runnable_root" || base.root.kind !== "runnable_root"
    || q.kind !== "blob" || q.size_bytes > 32 * 1024 || plan.kind !== "blob" || plan.size_bytes > 4 * 1024 * 1024
    || freeze_intent.kind !== "blob" || freeze_intent.size_bytes > 64 * 1024
    || drain_record.kind !== "blob" || drain_record.size_bytes > 32 * 1024) fail("invalid_source", "Invalid checkpoint data object kinds or bounds");
  const objects = item.objects.map(validateFabricReference), seen = new Set<string>(); let total = 0;
  for (const object of objects) { const key = identity(object); if (seen.has(key)) fail("invalid_source", "Checkpoint data inventory repeats an object"); seen.add(key); total += object.size_bytes; }
  if (total > 4 * 1024 ** 3 || [publication, q, source, runnable, plan, freeze_intent, drain_record].some((object) => !seen.has(identity(object)))) fail("invalid_source", "Checkpoint data inventory omits retained proof or exceeds its byte limit");
  return { format_version: 1, owned_pin: item.owned_pin, publication_id, publication, q, source, runnable, plan, freeze_intent, drain_record, base, objects };
}
export function validateFabricReference(value: unknown): FabricObjectRef {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail("invalid_reference", "Invalid object reference");
  const object = value as Record<string, unknown>;
  if (Object.keys(object).length !== 4 || typeof object.kind !== "string" || !kinds.has(object.kind) || object.algorithm !== "sha256" || typeof object.digest !== "string" || !/^[a-f0-9]{64}$/.test(object.digest) || !Number.isSafeInteger(object.size_bytes) || Number(object.size_bytes) < 0 || Number(object.size_bytes) > 256 * 1024 * 1024 || (object.kind !== "blob" && Number(object.size_bytes) > 4 * 1024 * 1024)) fail("invalid_reference", "Invalid typed SHA-256 object reference");
  if ((object.kind === "file" || object.kind === "directory_shard" || object.kind === "runnable_root") && Number(object.size_bytes) > 64 * 1024) fail("invalid_reference", "Content descriptor exceeds its format byte limit");
  return { kind: object.kind as FabricObjectKind, algorithm: "sha256", digest: object.digest as string, size_bytes: object.size_bytes as number };
}
export function verifyFabricObject(reference: FabricObjectRef, bytes: Uint8Array): void {
  validateFabricReference(reference);
  if (bytes.byteLength !== reference.size_bytes || createHash("sha256").update(bytes).digest("hex") !== reference.digest) fail("checksum_mismatch", "Transferred object failed checksum verification");
}
function rootId(value: string): string { if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(value)) fail("invalid_root_id", "Invalid root publication identifier"); return value; }
function lease(value: unknown): FabricLease {
  const item = value as Partial<FabricLease>;
  if (!item || Object.keys(item).length !== 3 || item.format_version !== 1 || typeof item.lease_id !== "string" || !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(item.lease_id) || !Number.isSafeInteger(item.expires_at_ms) || item.expires_at_ms! <= 0) fail("invalid_response", "Server returned an invalid staging lease");
  return item as FabricLease;
}
function receipt(value: unknown, expectedId: string, expectedRoot?: FabricObjectRef): FabricRootReceipt {
  const item = value as Partial<FabricRootReceipt>;
  if (!item || Object.keys(item).length !== 7 || item.format_version !== 1 || item.root_id !== expectedId || item.durability !== "replicated" || !Number.isSafeInteger(item.published_at_ms) || item.published_at_ms! < 0 || !Number.isSafeInteger(item.object_count) || item.object_count! < 1 || item.object_count! > 100_000 || !Number.isSafeInteger(item.total_bytes) || item.total_bytes! < 0 || item.total_bytes! > 4 * 1024 ** 3) fail("invalid_response", "Server returned an invalid publication receipt");
  const root = validateFabricReference(item.root);
  if (expectedRoot && identity(root) !== identity(expectedRoot)) fail("invalid_response", "Publication receipt refers to another root");
  return { ...item, root } as FabricRootReceipt;
}

/** A single client cannot hold several large objects at once, even when the
 * caller raises small-object concurrency. FIFO admission prevents starvation. */
interface ByteReservation { release(): void; retain(): () => void }
interface ByteWaiter { bytes: number; resolve: (reservation: ByteReservation) => void; cleanup: () => void }
class ByteBudget {
  private available = 256 * 1024 * 1024;
  private queue: ByteWaiter[] = [];
  acquire(bytes: number, timeoutMs: number, signal: AbortSignal): Promise<ByteReservation> {
    if (signal.aborted) return Promise.reject(new FabricRemoteError("transfer_cancelled", "Transfer batch has stopped"));
    if (this.queue.length >= 32) return Promise.reject(new FabricRemoteError("buffer_busy", "Transfer buffer admission is busy"));
    return new Promise((resolve, reject) => {
      const cancel = (error: FabricRemoteError) => {
        const index = this.queue.indexOf(item); if (index < 0) return;
        this.queue.splice(index, 1); item.cleanup(); reject(error); this.drain();
      };
      const aborted = () => cancel(new FabricRemoteError("transfer_cancelled", "Transfer batch has stopped"));
      const timer = setTimeout(() => cancel(new FabricRemoteError("request_timeout", "Transfer buffer admission exceeded its deadline")), timeoutMs);
      const item: ByteWaiter = { bytes: Math.max(1, bytes), resolve, cleanup: () => { clearTimeout(timer); signal.removeEventListener("abort", aborted); } };
      signal.addEventListener("abort", aborted, { once: true });
      this.queue.push(item); this.drain();
    });
  }
  private drain(): void {
    while (this.queue.length && this.queue[0].bytes <= this.available) {
      const item = this.queue.shift()!; item.cleanup(); this.available -= item.bytes;
      let owners = 1;
      const releaseOwner = () => {
        let released = false;
        return () => { if (!released) { released = true; if (--owners === 0) { this.available += item.bytes; this.drain(); } } };
      };
      item.resolve({ release: releaseOwner(), retain: () => {
        if (!owners) fail("invalid_source", "Transfer buffer was already released");
        owners++; return releaseOwner();
      } });
    }
  }
}

export class FabricRemoteClient {
  private readonly base: URL;
  private readonly concurrency: number;
  private readonly requestTimeoutMs: number;
  private readonly leaseTtlMs: number;
  private readonly buffered = new ByteBudget();
  private inFlight = 0;
  private readonly scope: FabricScope | undefined;
  constructor(private readonly options: {
    /** Full authenticated endpoint base, ending /api/rudder/fabric. */
    baseUrl: string; authHeaders: () => Record<string, string> | Promise<Record<string, string>>;
    fetch?: typeof globalThis.fetch; concurrency?: number; requestTimeoutMs?: number; leaseTtlMs?: number;
    /** Trusted host configuration, never supplied by a saved receipt. */
    scope?: FabricScope;
  }) {
    this.base = new URL(options.baseUrl.endsWith("/") ? options.baseUrl : `${options.baseUrl}/`);
    if (!["http:", "https:"].includes(this.base.protocol) || this.base.username || this.base.password || this.base.search || this.base.hash) fail("invalid_endpoint", "Invalid fabric endpoint");
    this.concurrency = options.concurrency ?? 1;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 120_000;
    this.leaseTtlMs = options.leaseTtlMs ?? 300_000;
    this.scope = options.scope === undefined ? undefined : scope(options.scope);
    if (!Number.isInteger(this.concurrency) || this.concurrency < 1 || this.concurrency > 8 || !Number.isInteger(this.requestTimeoutMs) || this.requestTimeoutMs < 1 || this.requestTimeoutMs > 120_000 || !Number.isInteger(this.leaseTtlMs) || this.leaseTtlMs < 1000 || this.leaseTtlMs > 3_600_000) fail("invalid_limits", "Invalid transfer concurrency or deadline");
  }
  private async request(path: string, method: string, body?: unknown, binary?: Uint8Array, headers: Record<string, string> = {}, maxResponseBytes = 2 * 1024 * 1024, releaseBody?: () => void): Promise<Buffer> {
    // A deadline bounds the caller's wait, not an abort-ignoring transport's
    // lifetime. Keep both its request slot and any payload reservation until
    // authentication, fetch and response consumption have actually settled.
    if (this.inFlight >= this.concurrency) { releaseBody?.(); fail("request_busy", "Fabric request admission is busy"); }
    this.inFlight++;
    const controller = new AbortController(); let timer: ReturnType<typeof setTimeout> | undefined;
    const work = (async () => {
      const authHeaders = await this.options.authHeaders();
      if (controller.signal.aborted) fail("request_timeout", "Fabric request exceeded its deadline");
      const response = await (this.options.fetch ?? globalThis.fetch)(new URL(path, this.base), {
        method, redirect: "error", signal: controller.signal,
        headers: { ...authHeaders, ...(body !== undefined ? { "Content-Type": "application/json" } : binary ? { "Content-Type": "application/octet-stream" } : {}), ...headers },
        body: binary ? Buffer.isBuffer(binary) ? binary : Buffer.from(binary.buffer, binary.byteOffset, binary.byteLength) : body !== undefined ? JSON.stringify(body) : undefined,
      });
      if (!response.ok) {
        await response.body?.cancel().catch(() => { });
        // Never echo response bodies, auth headers, bucket keys or request data.
        throw new FabricRemoteError("remote_rejected", `Fabric request failed (HTTP ${response.status})`, response.status);
      }
      const length = response.headers.get("content-length");
      if (length !== null && (!/^\d+$/.test(length) || Number(length) > maxResponseBytes)) { await response.body?.cancel().catch(() => { }); fail("too_large", "Fabric response exceeds byte limit"); }
      if (!response.body) return Buffer.alloc(0);
      const reader = response.body.getReader(); const chunks: Buffer[] = [];
      // Known lengths use one bounded destination; retaining chunks and then
      // concatenating would briefly double a 200 MiB compiler response.
      const destination = length === null ? undefined : Buffer.allocUnsafe(Number(length)); let bytes = 0;
      try { while (true) { const part = await reader.read(); if (part.done) break; const start = bytes; bytes += part.value.byteLength; if (bytes > maxResponseBytes || (destination && bytes > destination.length)) fail("too_large", "Fabric response exceeds byte limit"); if (destination) destination.set(part.value, start); else chunks.push(Buffer.from(part.value)); } }
      finally { await reader.cancel().catch(() => { }); }
      return destination ? destination.subarray(0, bytes) : Buffer.concat(chunks);
    })().finally(() => { this.inFlight--; releaseBody?.(); });
    try {
      return await Promise.race([work, new Promise<never>((_, reject) => { timer = setTimeout(() => { controller.abort(); reject(new FabricRemoteError("request_timeout", "Fabric request exceeded its deadline")); }, this.requestTimeoutMs); })]);
    } catch (failure) {
      if (failure instanceof FabricRemoteError) throw failure;
      throw new FabricRemoteError("request_failed", "Fabric request failed");
    } finally { if (timer) clearTimeout(timer); }
  }
  private async json(path: string, method: string, body?: unknown): Promise<unknown> {
    const bytes = await this.request(path, method, body);
    try { return JSON.parse(bytes.toString("utf8")); } catch { return fail("invalid_response", "Fabric returned invalid JSON"); }
  }
  async beginStaging(): Promise<FabricLease> { return lease(await this.json("staging", "POST", { format_version: 1, ttl_ms: this.leaseTtlMs })); }
  async renewStaging(leaseId: string): Promise<FabricLease> {
    lease({ format_version: 1, lease_id: leaseId, expires_at_ms: 1 });
    return lease(await this.json(`staging/${leaseId}/renew`, "POST", { format_version: 1, ttl_ms: this.leaseTtlMs }));
  }
  async abandonStaging(leaseId: string): Promise<void> {
    lease({ format_version: 1, lease_id: leaseId, expires_at_ms: 1 });
    const response = await this.json(`staging/${leaseId}`, "DELETE", { format_version: 1 }) as { format_version?: number };
    if (!response || Object.keys(response).length !== 1 || response.format_version !== 1) fail("invalid_response", "Invalid staging cleanup acknowledgment");
  }
  async missing(objects: FabricObjectRef[]): Promise<FabricObjectRef[]> {
    if (objects.length > 256) fail("too_large", "Missing-object request exceeds batch limit");
    objects.forEach(validateFabricReference);
    const reply = await this.json("missing", "POST", { format_version: 1, objects }) as { format_version?: number; missing?: unknown };
    if (!reply || reply.format_version !== 1 || !Array.isArray(reply.missing) || reply.missing.length > objects.length) fail("invalid_response", "Invalid missing-object response");
    const allowed = new Set(objects.map(identity)); const seen = new Set<string>();
    return (reply.missing as unknown[]).map((value) => { const reference = validateFabricReference(value); const key = identity(reference); if (!allowed.has(key) || seen.has(key)) fail("invalid_response", "Server requested an undeclared or duplicate object"); seen.add(key); return reference; });
  }
  async putObject(leaseId: string, reference: FabricObjectRef, bytes: Uint8Array): Promise<void> {
    await this.putRetainedObject(leaseId, reference, bytes);
  }
  private async putRetainedObject(leaseId: string, reference: FabricObjectRef, bytes: Uint8Array, reservation?: ByteReservation): Promise<void> {
    verifyFabricObject(reference, bytes); lease({ format_version: 1, lease_id: leaseId, expires_at_ms: 1 });
    await this.request(`objects/${reference.kind}/${reference.digest}/${reference.size_bytes}`, "PUT", undefined, bytes, { "X-Rudder-Staging-Lease": leaseId }, 2 * 1024 * 1024, reservation?.retain());
  }
  async getObject(reference: FabricObjectRef): Promise<Buffer> {
    return this.getRetainedObject(reference);
  }
  private async getRetainedObject(reference: FabricObjectRef, reservation?: ByteReservation): Promise<Buffer> {
    validateFabricReference(reference);
    const bytes = await this.request(`objects/${reference.kind}/${reference.digest}/${reference.size_bytes}`, "GET", undefined, undefined, {}, reference.size_bytes, reservation?.retain());
    verifyFabricObject(reference, bytes); return bytes;
  }
  async publishRoot(root: string, leaseId: string, object: FabricObjectRef): Promise<FabricRootReceipt> {
    rootId(root); validateFabricReference(object); lease({ format_version: 1, lease_id: leaseId, expires_at_ms: 1 });
    return receipt(await this.json(`roots/${encodeURIComponent(root)}`, "POST", { format_version: 1, lease_id: leaseId, root: object }), root, object);
  }
  async getRoot(root: string): Promise<FabricRootReceipt> {
    rootId(root); return receipt(await this.json(`roots/${encodeURIComponent(root)}`, "GET"), root);
  }
  private async uploadInventory(root: string, object: FabricObjectRef, all: FabricObjectRef[], source: Pick<FabricTransferSource, "read">): Promise<FabricTransferResult> {
    let staging = await this.beginStaging(); let renewal: Promise<void> | undefined;
    let result: FabricTransferResult | undefined;
    const ensureLease = async (force = false) => {
      if (!force && staging.expires_at_ms > Date.now() + Math.max(70_000, this.leaseTtlMs / 3)) return;
      renewal ??= this.renewStaging(staging.lease_id).then((updated) => { if (updated.lease_id !== staging.lease_id) fail("invalid_response", "Lease renewal changed identity"); staging = updated; }).finally(() => { renewal = undefined; });
      await renewal;
    };
    try {
      let uploaded = 0, uploadedBytes = 0;
      for (let start = 0; start < all.length; start += 256) {
        await ensureLease();
        const missing = await this.missing(all.slice(start, start + 256));
        let cursor = 0; let workerFailure: unknown;
        const cancelled = new AbortController();
        // The local retention stays held until every reader has settled.
        await Promise.all(Array.from({ length: Math.min(this.concurrency, missing.length) }, async () => {
          while (cursor < missing.length && !workerFailure) {
            const reference = missing[cursor++];
            let reservation: ByteReservation | undefined;
            try {
              reservation = await this.buffered.acquire(reference.size_bytes, this.requestTimeoutMs, cancelled.signal);
              if (workerFailure) continue;
              await ensureLease(); if (workerFailure) continue;
              const bytes = await source.read(reference); if (workerFailure) continue;
              verifyFabricObject(reference, bytes);
              await ensureLease(); if (workerFailure) continue;
              await this.putRetainedObject(staging.lease_id, reference, bytes, reservation); uploaded++; uploadedBytes += bytes.byteLength;
            }
            catch (error) { workerFailure ||= error || new FabricRemoteError("source_failed", "Pinned source read failed"); cancelled.abort(); }
            finally { reservation?.release(); }
          }
        }));
        if (workerFailure) throw workerFailure;
      }
      await ensureLease(true);
      result = { receipt: await this.publishRoot(root, staging.lease_id, object), uploaded_objects: uploaded, uploaded_bytes: uploadedBytes };
      return result;
    } finally {
      // Publication also removes its lease; DELETE is idempotent after commit.
      // Failure keeps at most the bounded lease lifetime, never a success claim.
      try { await this.abandonStaging(staging.lease_id); }
      catch { if (result) result.cleanup_warning = "Remote publication committed; staging cleanup was not confirmed"; }
    }
  }

  /** Publishes the complete resulting directory in Cloud using only retained
   * new objects. This is data replication, not accepted source/quiescence or a
   * newly runnable checkpoint. The host must publish those separately. */
  async uploadPartialTree(root: string, receiptJson: string, source: FabricPartialSource): Promise<FabricTransferResult> {
    rootId(root);
    if (!this.scope) fail("scope_required", "Partial transfer requires a trusted provider and tenant binding");
    if (typeof receiptJson !== "string" || Buffer.byteLength(receiptJson) > 64 * 1024) fail("invalid_source", "Invalid opaque partial receipt");
    let pinned: FabricPartialInventory;
    try { pinned = await source.pinPartial(receiptJson); }
    catch (error) { throw error instanceof FabricRemoteError ? error : new FabricRemoteError("source_failed", "Could not retain the partial transfer source"); }
    const issuedPin = pinned?.owned_pin;
    if (typeof issuedPin !== "string" || !/^transfer:partial:rpc:[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(issuedPin)) fail("invalid_source", "Partial source did not return a private retention identity");
    let result: FabricTransferResult | undefined; let failure: unknown;
    try {
      const inventory = validateFabricPartialInventory(pinned);
      if (inventory.owned_pin !== issuedPin) fail("invalid_source", "Partial source changed its issued retention identity");
      if (inventory.base.scope.provider_id !== this.scope!.provider_id || inventory.base.scope.tenant_id !== this.scope!.tenant_id) fail("scope_mismatch", "Partial source belongs to another provider or tenant");
      const base = await this.getRoot(inventory.base.root_id);
      if (identity(base.root) !== identity(inventory.base.root)) fail("base_mismatch", "Partial source base no longer matches its immutable Cloud publication");
      result = await this.uploadInventory(root, inventory.directory, inventory.objects, source);
    } catch (error) { failure = error instanceof FabricRemoteError ? error : new FabricRemoteError("source_failed", "Partial source transfer failed"); }
    finally {
      try { await source.unpin(issuedPin); }
      catch { if (result) result.cleanup_warning = "Remote publication committed; its temporary local retention needs cleanup"; else failure ??= new FabricRemoteError("source_cleanup_failed", "Temporary local retention needs cleanup"); }
    }
    if (failure) throw failure;
    return result!;
  }
  /** Clone the producer's exact retained DATA and publish its whole proof
   * closure. Repeated calls first reconcile the same immutable publication ID;
   * a transport failure never automatically retries an uncertain POST. */
  async uploadPartialCheckpoint(publicationId: string, receiptJson: string, source: FabricPartialCheckpointSource): Promise<FabricPartialCheckpointUploadResult> {
    partialCheckpointId(publicationId);
    const trustedScope = this.scope;
    if (!trustedScope) return fail("scope_required", "Checkpoint transfer requires a trusted provider and tenant binding");
    if (typeof receiptJson !== "string" || Buffer.byteLength(receiptJson) > 64 * 1024) fail("invalid_source", "Invalid opaque checkpoint data receipt");
    let pinned: FabricPartialCheckpointInventory;
    try { pinned = await source.pinPartialCheckpoint(receiptJson); }
    catch (error) { throw error instanceof FabricRemoteError ? error : new FabricRemoteError("source_failed", "Could not retain the checkpoint data transfer source"); }
    const issuedPin = pinned?.owned_pin;
    if (typeof issuedPin !== "string" || !partialCheckpointPin.test(issuedPin)) fail("invalid_source", "Checkpoint source did not return a private retention identity");
    let result: FabricPartialCheckpointUploadResult | undefined; let failure: unknown;
    try {
      const inventory = validateFabricPartialCheckpointInventory(pinned);
      if (inventory.owned_pin !== issuedPin) fail("invalid_source", "Checkpoint source changed its issued retention identity");
      if (inventory.publication_id !== publicationId) fail("publication_mismatch", "Checkpoint data belongs to another immutable publication");
      if (inventory.base.scope.provider_id !== trustedScope.provider_id || inventory.base.scope.tenant_id !== trustedScope.tenant_id) fail("scope_mismatch", "Checkpoint data belongs to another provider or tenant");
      const base = await this.getRoot(inventory.base.root_id);
      if (identity(base.root) !== identity(inventory.base.root)) fail("base_mismatch", "Checkpoint base differs from its immutable Cloud publication");
      let previous: FabricRootReceipt | undefined;
      try { previous = await this.getRoot(publicationId); }
      catch (error) { if (!(error instanceof FabricRemoteError) || error.status !== 404) throw error; }
      if (previous && identity(previous.root) !== identity(inventory.publication)) fail("publication_mismatch", "Checkpoint publication already refers to different content");
      const transfer = previous ? { receipt: previous, uploaded_objects: 0, uploaded_bytes: 0 } : await this.uploadInventory(publicationId, inventory.publication, inventory.objects, source);
      result = { format_version: 1, kind: "partial_checkpoint_data", source_role: "quiesced_source_complement_data", publication_id: publicationId,
        publication: inventory.publication, runnable: inventory.runnable, q: inventory.q, bundle: transfer.receipt,
        uploaded_objects: transfer.uploaded_objects, uploaded_bytes: transfer.uploaded_bytes,
        ...(transfer.cleanup_warning ? { cleanup_warning: transfer.cleanup_warning } : {}) };
    } catch (error) { failure = error instanceof FabricRemoteError ? error : new FabricRemoteError("source_failed", "Checkpoint data transfer failed"); }
    finally {
      try { await source.unpin(issuedPin); }
      catch { if (result) result.cleanup_warning = [result.cleanup_warning, "Checkpoint data published; temporary local retention needs cleanup"].filter(Boolean).join("; "); else failure ??= new FabricRemoteError("source_cleanup_failed", "Temporary checkpoint retention needs cleanup"); }
    }
    if (failure) throw failure;
    return result!;
  }
  async uploadRoot(root: string, object: FabricObjectRef, source: FabricTransferSource): Promise<FabricTransferResult> {
    rootId(root); validateFabricReference(object);
    let ownedPin: string;
    try { ownedPin = await source.pin(object); } catch { return fail("source_failed", "Could not pin the local transfer source"); }
    if (typeof ownedPin !== "string" || !ownedPin.trim()) fail("invalid_source", "Source did not return an owned retention pin");
    let result: FabricTransferResult | undefined;
    let failure: unknown;
    try {
      const inventory = await source.closure(object);
      if (!Array.isArray(inventory) || inventory.length > 100_000) fail("too_large", "Source inventory exceeds object limit");
      const unique = new Map<string, FabricObjectRef>(); let total = 0;
      for (const value of inventory) { const reference = validateFabricReference(value); if (!unique.has(identity(reference))) { unique.set(identity(reference), reference); total += reference.size_bytes; } }
      if (!unique.has(identity(object)) || total > 4 * 1024 ** 3) fail("invalid_source", "Source inventory is incomplete or exceeds byte limit");
      result = await this.uploadInventory(root, object, [...unique.values()], source);
    } catch (error) { failure = error instanceof FabricRemoteError ? error : new FabricRemoteError("source_failed", "Pinned source transfer failed"); }
    finally { try { await source.unpin(ownedPin); } catch { if (result) result.cleanup_warning = "Remote publication committed; its temporary local retention pin needs cleanup"; else failure ??= new FabricRemoteError("source_cleanup_failed", "Temporary local retention pin needs cleanup"); } }
    if (failure) throw failure;
    return result!;
  }

  async downloadRoot(root: string, target: FabricTransferTarget, pin: string): Promise<FabricDownloadResult> {
    rootId(root);
    if (typeof pin !== "string" || !pin.trim() || pin.length > 512 || /[\x00-\x1f\x7f]/.test(pin)) fail("invalid_pin", "A local retention pin is required");
    const published = await this.getRoot(root);
    let staging: FabricLease | undefined; let committed = false;
    try {
      staging = lease(await target.beginStaging(this.leaseTtlMs));
      let renewal: Promise<void> | undefined;
      const ensureLease = async (force = false) => {
        if (!force && staging!.expires_at_ms > Date.now() + Math.max(this.requestTimeoutMs + 5000, this.leaseTtlMs / 3)) return;
        renewal ??= target.renewStaging(staging!.lease_id, this.leaseTtlMs).then((value) => {
          const updated = lease(value); if (updated.lease_id !== staging!.lease_id) fail("invalid_target", "Local renewal changed lease identity"); staging = updated;
        }).finally(() => { renewal = undefined; });
        await renewal;
      };
      const queue: { object: FabricObjectRef; depth: number }[] = [];
      const seen = new Set<string>(); let total = 0, edges = 0, cursor = 0;
      const add = (value: unknown, depth: number) => {
        const object = validateFabricReference(value);
        if (depth > 1024) fail("too_large", "Downloaded closure exceeds depth limit");
        if (seen.has(identity(object))) return;
        total += object.size_bytes; seen.add(identity(object));
        if (seen.size > 100_000 || total > 4 * 1024 ** 3) fail("too_large", "Downloaded closure exceeds transfer limits");
        queue.push({ object, depth });
      };
      add(published.root, 0);
      let workerFailure: { error: unknown } | undefined;
      const cancelled = new AbortController();
      let active = 0;
      const waiting = new Set<() => void>();
      const wake = () => { for (const resolve of waiting) resolve(); waiting.clear(); };
      await Promise.all(Array.from({ length: this.concurrency }, async () => {
        while (!workerFailure) {
          if (cursor === queue.length) {
            // An in-flight metadata object can still reveal more work. Keep
            // idle workers available until the entire discovered graph drains.
            if (!active) return;
            await new Promise<void>((resolve) => { waiting.add(resolve); });
            continue;
          }
          const { object, depth } = queue[cursor++];
          active++;
          let reservation: ByteReservation | undefined;
          try {
            reservation = await this.buffered.acquire(object.size_bytes, this.requestTimeoutMs, cancelled.signal);
            if (workerFailure) continue;
            await ensureLease();
            if (workerFailure) continue;
            const bytes = await this.getRetainedObject(object, reservation);
            if (workerFailure) continue;
            await ensureLease(); const children = await target.receive(staging!.lease_id, object, bytes);
            if (!Array.isArray(children)) fail("invalid_target", "Local target returned invalid metadata references");
            edges += children.length; if (edges > 500_000 || (object.kind === "blob" && children.length)) fail("invalid_target", "Local target returned invalid metadata edges");
            for (const child of children) add(child, depth + 1);
          } catch (error) { workerFailure ??= { error }; cancelled.abort(); }
          finally { reservation?.release(); active--; wake(); }
        }
      }));
      if (workerFailure) throw workerFailure.error;
      if (seen.size !== published.object_count || total !== published.total_bytes) fail("invalid_response", "Published closure inventory differs from verified downloaded objects");
      await ensureLease(true);
      const local = await target.publish(staging.lease_id, pin, published.root);
      if (!local || local.format_version !== 1 || local.durability !== "local" || local.pin !== pin || identity(validateFabricReference(local.root)) !== identity(published.root) || !Number.isSafeInteger(local.generation) || local.generation < 1) fail("invalid_target", "Local target returned invalid retention evidence");
      committed = true;
      return { receipt: published, local, downloaded_objects: seen.size, downloaded_bytes: total };
    } catch (failure) {
      if (failure instanceof FabricRemoteError) throw failure;
      throw new FabricRemoteError("target_failed", "Local download did not confirm complete durable publication");
    } finally {
      if (staging && !committed) { try { await target.abandonStaging(staging.lease_id); } catch { /* No durable success is returned; its bounded staging lease may need cleanup. */ } }
    }
  }
}

/** Lazy loading keeps the native transport out of ordinary remote-client use. */
export async function runFabricTransferCommand(args: string[], options: import("./fabric-local.js").FabricTransferCommandOptions = {}): Promise<void> {
  return (await import("./fabric-local.js")).runFabricTransferCommand(args, options);
}
