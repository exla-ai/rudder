/** One bounded native CAS connection, using raw binary frames rather than a
 * process or hexadecimal JSON allocation per object. This is a trusted local
 * machine API; run-scoped agents use the separate capability-authenticated API.
 */
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { resolveNativeBinaryPath } from "./native-binary.js";
import { FabricRemoteClient, FabricRemoteError, validateFabricReference, validateFabricPartialInventory, validateFabricPartialCheckpointInventory, verifyFabricObject,
  type FabricLease, type FabricLocalReceipt, type FabricObjectRef,
  type FabricPartialInventory, type FabricPartialSource, type FabricPartialCheckpointInventory, type FabricPartialCheckpointSource, type FabricTransferSource, type FabricTransferTarget } from "./fabric-remote.js";

const MAX_HEADER = 32 * 1024 * 1024;
const MAX_REQUEST_HEADER = 256 * 1024;
const MAX_BODY = 256 * 1024 * 1024;
const same = (a: FabricObjectRef, b: FabricObjectRef) => a.kind === b.kind && a.digest === b.digest && a.size_bytes === b.size_bytes;
export class FabricLocalError extends Error {
  constructor(public readonly code: string, message: string) { super(message); this.name = "FabricLocalError"; }
}
export interface FabricLocalOptions { home?: string; nativeBinary?: string; env?: NodeJS.ProcessEnv; requestTimeoutMs?: number }
interface Reply { result: Record<string, unknown>; bytes: Buffer }
interface Pending { resolve: (reply: Reply) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> }

export class FabricLocalClient implements FabricTransferSource, FabricTransferTarget, FabricPartialSource, FabricPartialCheckpointSource {
  readonly pid: number | undefined;
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly deadline: number;
  private pending = new Map<string, Pending>();
  private writeTail: Promise<void> = Promise.resolve();
  private failure: FabricLocalError | undefined;
  private closing = false;
  private headerParts: Buffer[] = [];
  private headerLength = 0;
  private incoming: { header: Record<string, unknown>; bytes: Buffer; length: number; wanted: number } | undefined;
  private ownedPins = new Set<string>();
  private ownedLeases = new Set<string>();
  private readonly exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
  private constructor(options: FabricLocalOptions) {
    const binary = options.nativeBinary ?? resolveNativeBinaryPath();
    if (!binary) throw new FabricLocalError("native_unavailable", "Rebuild or reinstall the current Rudder native binary");
    this.deadline = options.requestTimeoutMs ?? 120_000;
    if (!Number.isInteger(this.deadline) || this.deadline < 1 || this.deadline > 120_000) throw new FabricLocalError("invalid_limits", "Invalid local CAS request deadline");
    this.child = spawn(binary, ["fabric", "rpc"], { stdio: ["pipe", "pipe", "pipe"], env: { ...(options.env ?? process.env), ...(options.home ? { RUDDER_HOME: path.resolve(options.home) } : {}) } });
    this.pid = this.child.pid;
    this.child.stderr.resume(); // Never reflect diagnostics containing local data.
    this.child.stdout.on("data", (chunk: Buffer) => { try { this.consume(chunk); } catch { this.fail("invalid_response", "Native CAS returned an invalid binary frame"); } });
    this.child.stdin.on("error", () => this.fail("local_unavailable", "Native CAS connection closed; inspect retention before retrying publication"));
    this.child.on("error", () => this.fail("native_unavailable", "Could not launch the current Rudder native CAS helper"));
    this.exited = new Promise((resolve) => {
      // close follows stdout drainage; exit can precede a final response frame.
      this.child.once("close", (code, signal) => {
        resolve({ code, signal });
        if (!this.closing || this.pending.size) this.fail("local_unavailable", "Native CAS exited; inspect retention before retrying publication");
      });
      this.child.once("error", () => resolve({ code: null, signal: null }));
    });
  }
  static async open(options: FabricLocalOptions = {}): Promise<FabricLocalClient> {
    const client = new FabricLocalClient(options);
    try { await client.request({ operation: "capabilities" }); return client; }
    catch (failure) { await client.close().catch(() => {}); throw failure; }
  }
  private fail(code: string, message: string): void {
    this.failure ??= new FabricLocalError(code, message);
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(this.failure); }
    this.pending.clear(); this.child.stdin.destroy(); this.child.stdout.destroy(); this.child.kill("SIGTERM");
  }
  private complete(header: Record<string, unknown>, bytes: Buffer): void {
    const id = String(header.id); const pending = this.pending.get(id);
    if (!pending) throw new Error("unknown response");
    clearTimeout(pending.timer); this.pending.delete(id);
    if (header.ok === false) pending.reject(new FabricLocalError("local_request_failed", "Local CAS rejected the request; no durable transfer success is implied"));
    else pending.resolve({ result: header.result as Record<string, unknown>, bytes });
  }
  private consume(chunk: Buffer): void {
    let offset = 0;
    while (offset < chunk.length) {
      if (this.incoming) {
        const body = this.incoming; const count = Math.min(body.wanted - body.length, chunk.length - offset);
        chunk.copy(body.bytes, body.length, offset, offset + count); body.length += count; offset += count;
        if (body.length === body.wanted) { this.incoming = undefined; this.complete(body.header, body.bytes); }
        continue;
      }
      const newline = chunk.indexOf(10, offset); const end = newline < 0 ? chunk.length : newline;
      this.headerLength += end - offset;
      if (this.headerLength > MAX_HEADER) throw new Error("header too large");
      this.headerParts.push(chunk.subarray(offset, end)); offset = end + (newline < 0 ? 0 : 1);
      if (newline < 0) continue;
      const header = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(this.headerParts, this.headerLength))) as Record<string, unknown>;
      this.headerParts = []; this.headerLength = 0;
      if (!header || header.format_version !== 1 || typeof header.id !== "string" || !this.pending.has(header.id) || typeof header.ok !== "boolean" || !Number.isSafeInteger(header.binary_bytes) || Number(header.binary_bytes) < 0 || Number(header.binary_bytes) > MAX_BODY || (!header.ok && header.binary_bytes !== 0) || (header.ok && (!header.result || typeof header.result !== "object" || Array.isArray(header.result)))) throw new Error("invalid response");
      if (header.binary_bytes === 0) this.complete(header, Buffer.alloc(0));
      else this.incoming = { header, bytes: Buffer.allocUnsafe(Number(header.binary_bytes)), length: 0, wanted: Number(header.binary_bytes) };
    }
  }
  private async rpc(request: Record<string, unknown>, binary?: Uint8Array): Promise<Reply> {
    if (this.failure) throw this.failure;
    if (this.closing) throw new FabricLocalError("local_closed", "Local CAS connection is closed");
    if (this.pending.size >= 16) throw new FabricLocalError("local_busy", "Too many pending local CAS requests");
    const id = randomUUID();
    const header = Buffer.from(JSON.stringify({ format_version: 1, id, request, binary_bytes: binary?.byteLength ?? 0 }) + "\n");
    if (header.length > MAX_REQUEST_HEADER || (binary?.byteLength ?? 0) > MAX_BODY) throw new FabricLocalError("too_large", "Local CAS frame exceeds byte limits");
    const result = new Promise<Reply>((resolve, reject) => {
      const timer = setTimeout(() => this.fail("local_timeout", "Local CAS request exceeded its deadline; inspect retention before retrying publication"), this.deadline);
      this.pending.set(id, { resolve, reject, timer });
    });
    const write = (bytes: Uint8Array) => new Promise<void>((resolve, reject) => this.child.stdin.write(bytes, (error) => error ? reject(error) : resolve()));
    this.writeTail = this.writeTail.then(async () => {
      if (this.failure) return;
      await write(header); if (binary?.byteLength) await write(binary);
    }).catch(() => this.fail("local_unavailable", "Local CAS frame could not be delivered; inspect retention before retrying publication"));
    return result;
  }
  async request(operation: Record<string, unknown>): Promise<Record<string, unknown>> {
    const response = await this.rpc({ operation: "store", request: operation });
    if (response.bytes.length) throw new FabricLocalError("invalid_response", "Unexpected local CAS binary body");
    return response.result;
  }
  async pin(root: FabricObjectRef): Promise<string> {
    validateFabricReference(root);
    const { result } = await this.rpc({ operation: "pin_source", root });
    if (typeof result.owned_pin !== "string" || !/^transfer:rpc:[a-f0-9-]{36}$/.test(result.owned_pin) || this.ownedPins.has(result.owned_pin)) throw new FabricLocalError("invalid_response", "Native CAS did not allocate a new private transfer pin");
    this.ownedPins.add(result.owned_pin); return result.owned_pin;
  }
  async pinPartial(receiptJson: string): Promise<FabricPartialInventory> {
    if (typeof receiptJson !== "string" || Buffer.byteLength(receiptJson) > 64 * 1024) throw new FabricLocalError("invalid_receipt", "Partial receipt exceeds its byte limit");
    const { result } = await this.rpc({ operation: "pin_partial_source", receipt_json: receiptJson });
    return this.acceptPartialInventory(result, /^transfer:partial:rpc:[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/, validateFabricPartialInventory);
  }
  async pinPartialCheckpoint(receiptJson: string): Promise<FabricPartialCheckpointInventory> {
    if (typeof receiptJson !== "string" || Buffer.byteLength(receiptJson) > 64 * 1024) throw new FabricLocalError("invalid_receipt", "Checkpoint data receipt exceeds its byte limit");
    const { result } = await this.rpc({ operation: "pin_partial_checkpoint", receipt_json: receiptJson });
    return this.acceptPartialInventory(result, /^transfer:partial-checkpoint:rpc:[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/, validateFabricPartialCheckpointInventory);
  }
  private async acceptPartialInventory<T extends { owned_pin: string }>(result: Record<string, unknown>, pattern: RegExp, validate: (value: unknown) => T): Promise<T> {
    const issuedPin = result.owned_pin;
    if (typeof issuedPin !== "string" || !pattern.test(issuedPin) || this.ownedPins.has(issuedPin)) throw new FabricLocalError("invalid_response", "Native CAS did not allocate a new partial transfer pin");
    // A successful reply can already have committed retention. Track that
    // exact new identity before validating the remainder of its inventory.
    this.ownedPins.add(issuedPin);
    try {
      const inventory = validate(result);
      if (inventory.owned_pin !== issuedPin) throw new FabricLocalError("invalid_response", "Native CAS changed its partial transfer pin");
      return inventory;
    } catch (error) {
      try { await this.unpin(issuedPin); }
      catch { throw new FabricLocalError("source_cleanup_pending", "Invalid partial inventory; temporary retention still needs connection cleanup"); }
      throw error;
    }
  }
  async unpin(pin: string): Promise<void> {
    if (!this.ownedPins.has(pin)) throw new FabricLocalError("invalid_pin", "Pin is outside this transfer connection");
    await this.rpc({ operation: "unpin_source", pin }); this.ownedPins.delete(pin);
  }
  async closure(root: FabricObjectRef): Promise<FabricObjectRef[]> {
    validateFabricReference(root);
    const { result } = await this.rpc({ operation: "inventory", root });
    if (!Array.isArray(result.objects) || result.objects.length > 100_000) throw new FabricLocalError("invalid_response", "Invalid local source inventory");
    return result.objects.map(validateFabricReference);
  }
  async read(object: FabricObjectRef): Promise<Buffer> {
    validateFabricReference(object);
    const { result, bytes } = await this.rpc({ operation: "read_binary", object });
    if (!same(validateFabricReference(result.object), object)) throw new FabricLocalError("invalid_response", "Native CAS read returned another object");
    verifyFabricObject(object, bytes); return bytes;
  }
  private localLease(result: Record<string, unknown>): FabricLease {
    if (typeof result.id !== "string" || !/^[a-f0-9-]{36}$/.test(result.id) || !Number.isSafeInteger(result.expires_at_ms) || Number(result.expires_at_ms) <= 0) throw new FabricLocalError("invalid_response", "Invalid local staging lease");
    return { format_version: 1, lease_id: result.id, expires_at_ms: Number(result.expires_at_ms) };
  }
  async beginStaging(ttlMs: number): Promise<FabricLease> {
    const value = this.localLease(await this.request({ operation: "begin_staging", ttl_ms: ttlMs }));
    this.ownedLeases.add(value.lease_id); return value;
  }
  async renewStaging(leaseId: string, ttlMs: number): Promise<FabricLease> {
    if (!this.ownedLeases.has(leaseId)) throw new FabricLocalError("invalid_lease", "Lease is outside this transfer connection");
    return this.localLease(await this.request({ operation: "renew_staging", lease_id: leaseId, ttl_ms: ttlMs }));
  }
  async receive(leaseId: string, object: FabricObjectRef, bytes: Uint8Array): Promise<FabricObjectRef[]> {
    if (!this.ownedLeases.has(leaseId)) throw new FabricLocalError("invalid_lease", "Lease is outside this transfer connection");
    verifyFabricObject(object, bytes);
    const { result } = await this.rpc({ operation: "receive_binary", lease_id: leaseId, object }, bytes);
    if (!same(validateFabricReference(result.object), object) || !Array.isArray(result.children)) throw new FabricLocalError("invalid_response", "Invalid local metadata acknowledgment");
    return result.children.map(validateFabricReference);
  }
  async publish(leaseId: string, pin: string, root: FabricObjectRef): Promise<FabricLocalReceipt> {
    if (!this.ownedLeases.has(leaseId)) throw new FabricLocalError("invalid_lease", "Lease is outside this transfer connection");
    validateFabricReference(root);
    const result = await this.request({ operation: "publish", lease_id: leaseId, pin, root });
    if (!Number.isSafeInteger(result.generation) || Number(result.generation) < 1) throw new FabricLocalError("invalid_response", "Invalid local durable retention acknowledgment");
    this.ownedLeases.delete(leaseId);
    return { format_version: 1, root, pin, generation: Number(result.generation), durability: "local" };
  }
  async abandonStaging(leaseId: string): Promise<void> {
    if (!this.ownedLeases.has(leaseId)) throw new FabricLocalError("invalid_lease", "Lease is outside this transfer connection");
    await this.request({ operation: "abandon_staging", lease_id: leaseId }); this.ownedLeases.delete(leaseId);
  }
  async close(): Promise<void> {
    if (!this.closing) { this.closing = true; await this.writeTail; this.child.stdin.end(); }
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const status = await Promise.race([this.exited, new Promise<never>((_, reject) => { timer = setTimeout(() => { this.fail("local_close_timeout", "Local helper cleanup was not confirmed; inspect transfer retention"); reject(this.failure); }, 5_000); })]);
      if (status.code !== 0 || this.failure) throw this.failure ?? new FabricLocalError("local_cleanup_failed", "Native helper cleanup was not confirmed; inspect transfer retention");
    } finally { if (timer) clearTimeout(timer); }
  }
}

export interface FabricTransferCommandOptions extends FabricLocalOptions {
  stdout?: (line: string) => void;
  authHeaders?: () => Record<string, string> | Promise<Record<string, string>>;
  fetch?: typeof globalThis.fetch;
}
export async function runFabricTransferCommand(args: string[], options: FabricTransferCommandOptions = {}): Promise<void> {
  const [action, rootId, ...rest] = args;
  if (!["upload", "download"].includes(action) || !rootId) throw new Error("Usage: rudder engine api upload|download ROOT_ID [--root JSON|--root-file PATH] [--pin NAME] [--home PATH] [--url CLOUD_URL]");
  const flags = new Map<string, string>();
  for (let i = 0; i < rest.length; i += 2) {
    const key = rest[i]; const value = rest[i + 1];
    if (!["--root", "--root-file", "--pin", "--home", "--url"].includes(key) || value === undefined || flags.has(key)) throw new Error("Invalid fabric transfer argument");
    flags.set(key, value);
  }
  if (action === "upload" && flags.has("--pin")) throw new Error("--pin selects local retention for download");
  if (action === "download" && (flags.has("--root") || flags.has("--root-file"))) throw new Error("Download resolves the immutable published root id");
  const env = options.env ?? process.env;
  const base = new URL(flags.get("--url") ?? env.RUDDER_CLOUD_URL?.trim() ?? "https://rudder-cloud-control.fly.dev");
  if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.search || base.hash || (base.protocol === "http:" && !["127.0.0.1", "localhost", "[::1]"].includes(base.hostname) && env.RUDDER_CLOUD_ALLOW_HTTP !== "1")) throw new Error("Fabric transfers require a trusted HTTPS endpoint or loopback HTTP");
  const cloudUrl = base.toString().replace(/\/$/, "");
  const auth = options.authHeaders ?? (async (): Promise<Record<string, string>> => {
    const worker = env.RUDDER_WORKER_TOKEN?.trim(); const workspace = env.RUDDER_WORKSPACE_ID?.trim();
    if (worker || workspace) {
      if (!worker || !workspace) throw new FabricRemoteError("worker_identity_incomplete", "A worker transfer requires its paired workspace identity and worker credential");
      return { Authorization: `Bearer ${worker}`, "X-Rudder-Workspace-Id": workspace };
    }
    const configured = env.RUDDER_CLOUD_TOKEN?.trim(); if (configured) return { Authorization: `Bearer ${configured}` };
    const file = path.join(path.resolve(env.RUDDER_HOME?.trim() || path.join(os.homedir(), ".rudder")), "cloud.json");
    let state: { version?: number; cloudUrl?: string; token?: string } | undefined;
    try { state = JSON.parse(await fs.readFile(file, "utf8")); } catch { /* Missing login is handled below. */ }
    if (state?.version !== 1 || typeof state.cloudUrl !== "string" || state.cloudUrl.replace(/\/$/, "") !== cloudUrl || typeof state.token !== "string" || !state.token.trim()) throw new FabricRemoteError("not_logged_in", "Not logged in to this Rudder Cloud endpoint; run rudder login");
    return { Authorization: `Bearer ${state.token}` };
  });
  const remote = new FabricRemoteClient({ baseUrl: `${cloudUrl}/api/rudder/fabric`, authHeaders: auth, fetch: options.fetch });
  let root: FabricObjectRef | undefined;
  if (action === "upload") {
    if (flags.has("--root") === flags.has("--root-file")) throw new Error("Upload requires exactly one of --root JSON or --root-file PATH");
    let raw = flags.get("--root");
    if (!raw) { const file = await fs.open(flags.get("--root-file")!, "r"); try { const bytes = Buffer.alloc(4097); const { bytesRead } = await file.read(bytes); if (bytesRead > 4096) throw new Error("Root reference file exceeds byte limit"); raw = bytes.subarray(0, bytesRead).toString("utf8"); } finally { await file.close(); } }
    try { root = validateFabricReference(JSON.parse(raw!)); } catch { throw new Error("Upload requires a valid typed root reference"); }
  } else if (!flags.get("--pin")) throw new Error("Download requires --pin NAME for durable local retention");
  const local = await FabricLocalClient.open({ ...options, home: flags.get("--home") ?? options.home });
  let result: Record<string, unknown> | undefined;
  try { result = (action === "upload" ? await remote.uploadRoot(rootId, root!, local) : await remote.downloadRoot(rootId, local, flags.get("--pin")!)) as unknown as Record<string, unknown>; }
  finally { try { await local.close(); } catch { if (result) result.cleanup_warning = "Publication committed; local helper cleanup needs inspection"; } }
  (options.stdout ?? ((line) => process.stdout.write(line)))(JSON.stringify({ format_version: 1, operation: action, ...result }) + "\n");
}
