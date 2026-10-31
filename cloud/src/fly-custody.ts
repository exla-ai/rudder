import { createHash, timingSafeEqual } from "node:crypto";
import { inspect } from "node:util";

/** Machine-lease observations only. No app routing, worker boot/NET, ingress or
 * managed-egress permission is established by this module.
 * Fly wire contract: https://docs.fly.io/machines/api/machines-resource
 * Explicit renewal: https://fly.io/mcp/ (create_machine_lease).
 */
export type FlyCustodySample = Readonly<{ wallMs: number; boottimeMs: number }>;
export interface FlyCustodyClock {
  /** Trusted process-local, suspend-aware time. Restart/resume uncertainty must
   * fence the session; an arbitrary saved timestamp is not an implementation. */
  sample(): FlyCustodySample;
}
export type FlyCustodyRequest = Readonly<{
  method: "GET" | "POST" | "DELETE";
  path: string;
  headers: Readonly<Record<string, string>>;
  body?: string;
  signal: AbortSignal;
}>;
export type FlyCustodyReply = Readonly<{ status: number; body: unknown }>;
export interface FlyCustodyTransport {
  /** TRUSTED authenticated Fly HTTP adapter, retained for this session. Pin the
   * API origin/app, bound response bytes, preserve the nonce header, never retry
   * or log headers/bodies, and settle only when the actual request has settled.
   * Aborting is a request to stop, never evidence that Fly did not commit it. */
  request(request: FlyCustodyRequest): Promise<FlyCustodyReply>;
}
export type FlyMachineExpectation = Readonly<{
  app: string;
  machineId: string;
  instanceId: string;
  configSha256: string;
  /** Fresh 256-bit correlation supplied by the trusted controller. The lease
   * description echoes it; that is NOT an authenticated worker/NET challenge. */
  challenge: string;
}>;
export type FlyMachineMutation =
  | Readonly<{ action: "update"; config: unknown }>
  | Readonly<{ action: "start" | "stop" | "suspend" | "cordon" | "uncordon" }>
  | Readonly<{ action: "destroy"; force: boolean }>;
type Phase = "new" | "acquiring" | "held" | "retired" | "uncertain" | "released";
type OperationName = "acquire" | "observe" | "renew" | "mutate" | "release" | "reconcile";
type End = Readonly<{ wallMs: number; boottimeMs: number }>;
type Lease = { expiresAtMs: number; owner: string; end: End; serial: number };
type Operation = { name: OperationName; start: FlyCustodySample; end: End;
  abort: AbortController; timer?: ReturnType<typeof setTimeout>; failed: boolean };
export type FlyCustodyErrorCode = "invalid_input" | "busy" | "not_current" | "uncertain"
  | "expired" | "clock" | "identity_changed" | "lease_changed" | "rejected"
  | "nonce_unavailable" | "release_already_attempted";
const ERROR_CODES = new Set<string>(["invalid_input", "busy", "not_current", "uncertain", "expired", "clock",
  "identity_changed", "lease_changed", "rejected", "nonce_unavailable", "release_already_attempted"]);
export class FlyCustodyError extends Error {
  readonly code: FlyCustodyErrorCode;
  constructor(code: FlyCustodyErrorCode) {
    const safe = ERROR_CODES.has(code) ? code : "uncertain";
    super(`Fly Machine lease session: ${safe}`); this.name = "FlyCustodyError"; this.code = safe;
  }
}
export type FlyMachineLeaseObservation = Readonly<{
  kind: "fly-machine-lease-observation-v1";
  scope: "machine-lease-only";
  app: string; machineId: string; instanceId: string; configSha256: string;
  challengeSha256: string; serial: number;
  /** Fixed acquisition-start cap, independent of each provider lease TTL. */
  sessionEndWallMs: number; sessionEndBoottimeMs: number;
  originalEndWallMs: number; originalEndBoottimeMs: number;
  observedAtWallMs: number; observedAtBoottimeMs: number;
}>;
export type FlyLeaseReconciliation = Readonly<{
  kind: "fly-machine-lease-reconciliation-v1";
  observation: "machine_absent" | "machine_mismatch" | "lease_absent"
    | "held_nonce" | "matching_challenge_without_acquisition_ack" | "different_lease";
  custodyRestored: false;
}>;
export interface FlyMachineLeaseSession {
  acquire(): Promise<FlyMachineLeaseObservation>;
  observe(): Promise<FlyMachineLeaseObservation>;
  /** Explicit only, before the old deadline. Never extends the original session
   * cap, resurrects an expired session, or changes an already returned record. */
  renew(ttlSeconds: number): Promise<FlyMachineLeaseObservation>;
  /** Invalidates this generation BEFORE dispatch. An HTTP ACK is not lifecycle
   * completion or a new identity binding. A new generation needs a new session. */
  mutate(mutation: FlyMachineMutation): Promise<Readonly<{ kind: "machine-mutation-http-ack"; action: FlyMachineMutation["action"] }>>;
  release(): Promise<Readonly<{ kind: "machine-lease-release-http-ack" }>>;
  /** Read-only observations; never acquires a nonce from readback or unfences. */
  reconcile(): Promise<FlyLeaseReconciliation>;
  /** Synchronous invalidation; in-flight work remains charged until it settles. */
  fence(): void;
  status(): Readonly<Record<string, unknown>>;
}
export type FlyMachineLeaseOptions = Readonly<{
  transport: FlyCustodyTransport; clock: FlyCustodyClock;
  expected: FlyMachineExpectation;
  ttlSeconds: number;
  /** Finite duration from the ORIGINAL acquisition request start, 1 second to
   * 24 hours. Default preserves the initial provider TTL cap. No absolute saved
   * deadline is accepted, and explicit renewal can never move this cap. */
  sessionLifetimeMs?: number;
  requestTimeoutMs?: number;
}>;

const MAX_CONFIG_BYTES = 256 * 1024;
const MAX_SESSION_LIFETIME_MS = 24 * 60 * 60 * 1000;
const MARGIN_MS = 100;
function fail(code: FlyCustodyErrorCode): never { throw new FlyCustodyError(code); }
const hash = (text: string) => createHash("sha256").update(text).digest("hex");
function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)
    || ![Object.prototype, null].includes(Object.getPrototypeOf(value))) fail("rejected");
  return value as Record<string, unknown>;
}
function text(value: unknown, max = 256): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= max && !/[\x00-\x1f\x7f]/.test(value);
}
function ttl(value: number): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > 60) fail("invalid_input");
  return value;
}
function canonicalConfig(value: unknown): string {
  let nodes = 0, size = 0;
  const encode = (input: unknown, depth: number): string => {
    if (++nodes > 10_000 || depth > 32) fail("invalid_input");
    let result: string;
    if (input === null || typeof input === "boolean" || typeof input === "string"
      || typeof input === "number" && Number.isFinite(input)) {
      if (typeof input === "string" && input.length > MAX_CONFIG_BYTES) fail("invalid_input");
      result = JSON.stringify(input);
    } else if (Array.isArray(input)) {
      const keys = Object.keys(input);
      if (input.length > 10_000 || keys.length !== input.length
        || keys.some((key, index) => key !== String(index))) fail("invalid_input");
      result = `[${Array.from({ length: input.length }, (_, index) => {
        const descriptor = Object.getOwnPropertyDescriptor(input, String(index));
        if (!descriptor || !("value" in descriptor)) fail("invalid_input");
        return encode(descriptor.value, depth + 1);
      }).join(",")}]`;
    } else {
      const record = object(input), keys = Object.keys(record).sort();
      if (keys.length > 10_000 || Object.getOwnPropertySymbols(record).length) fail("invalid_input");
      result = `{${keys.map(key => {
        const descriptor = Object.getOwnPropertyDescriptor(record, key);
        if (key.length > 1024 || !descriptor || !("value" in descriptor)) fail("invalid_input");
        return `${JSON.stringify(key)}:${encode(descriptor.value, depth + 1)}`;
      }).join(",")}}`;
    }
    size += result.length;
    if (size > MAX_CONFIG_BYTES * 33 || Buffer.byteLength(result) > MAX_CONFIG_BYTES) fail("invalid_input");
    return result;
  };
  try { object(value); return encode(value, 0); } catch { return fail("invalid_input"); }
}
/** Canonical exact config DATA, including all services/env/metadata fields. */
export function flyMachineConfigDigest(config: unknown): string { return hash(canonicalConfig(config)); }
function minEnd(a: End, b: End): End {
  return Object.freeze({ wallMs: Math.min(a.wallMs, b.wallMs), boottimeMs: Math.min(a.boottimeMs, b.boottimeMs) });
}
function addEnd(at: FlyCustodySample, ms: number): End {
  if (!Number.isSafeInteger(at.wallMs + ms) || !Number.isSafeInteger(at.boottimeMs + ms)) fail("clock");
  return Object.freeze({ wallMs: at.wallMs + ms, boottimeMs: at.boottimeMs + ms });
}
function nonceEqual(value: unknown, held: Buffer): boolean {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]{1,256}$/.test(value)) return false;
  const candidate = Buffer.from(value);
  try { return candidate.length === held.length && timingSafeEqual(candidate, held); }
  finally { candidate.fill(0); }
}

class MachineLeaseSession implements FlyMachineLeaseSession {
  #transport: FlyCustodyTransport; #clock: FlyCustodyClock; #expected: FlyMachineExpectation;
  #timeout: number; #ttl: number; #sessionLifetime: number; #description: string; #challengeHash: string;
  #phase: Phase = "new"; #uncertain = false; #closing = false;
  #last?: FlyCustodySample; #cap?: End; #lease?: Lease; #nonce?: Buffer;
  #pending?: Operation; #releaseAttempted = false; #releaseAck = false;
  constructor(options: FlyMachineLeaseOptions) {
    this.#transport = options.transport; this.#clock = options.clock;
    this.#expected = Object.freeze({ ...options.expected }); this.#ttl = ttl(options.ttlSeconds);
    this.#sessionLifetime = options.sessionLifetimeMs ?? this.#ttl * 1000;
    this.#timeout = options.requestTimeoutMs ?? 1500;
    const e = this.#expected;
    if (!e || !/^[a-z0-9][a-z0-9-]{0,62}$/.test(e.app)
      || !/^[a-z0-9_-]{1,64}$/.test(e.machineId) || !/^[A-Za-z0-9_-]{1,128}$/.test(e.instanceId)
      || !/^[0-9a-f]{64}$/.test(e.configSha256) || !/^[0-9a-f]{64}$/.test(e.challenge)
      || !Number.isSafeInteger(this.#timeout) || this.#timeout < 1 || this.#timeout > 5000
      || !Number.isSafeInteger(this.#sessionLifetime) || this.#sessionLifetime < 1000 || this.#sessionLifetime > MAX_SESSION_LIFETIME_MS
      || typeof this.#transport?.request !== "function" || typeof this.#clock?.sample !== "function") fail("invalid_input");
    this.#challengeHash = hash(e.challenge); this.#description = `rudder-machine-custody-v1:${e.challenge}`;
  }
  #invalidate(): void {
    this.#uncertain = true; this.#phase = "uncertain";
    if (this.#pending) { this.#pending.failed = true; this.#pending.abort.abort(); }
  }
  fence(): void { this.#invalidate(); }
  #sample(): FlyCustodySample {
    let at: FlyCustodySample;
    try { at = this.#clock.sample(); } catch { this.#invalidate(); return fail("clock"); }
    if (!Number.isSafeInteger(at?.wallMs) || !Number.isSafeInteger(at?.boottimeMs)
      || at.wallMs < 0 || at.boottimeMs < 0
      || this.#last && (at.wallMs < this.#last.wallMs || at.boottimeMs < this.#last.boottimeMs)) {
      this.#invalidate(); return fail("clock");
    }
    this.#last = Object.freeze({ wallMs: at.wallMs, boottimeMs: at.boottimeMs }); return this.#last;
  }
  #before(end: End, at: FlyCustodySample): void {
    if (at.wallMs >= end.wallMs || at.boottimeMs >= end.boottimeMs) { this.#invalidate(); fail("expired"); }
  }
  #begin(name: OperationName): Operation {
    const at = this.#sample(); // Original bounds precede any contention/IO.
    if (this.#pending) fail("busy");
    const cleanup = name === "release" || name === "reconcile";
    if (!cleanup && name !== "acquire" && (this.#phase !== "held" || this.#closing)) fail("not_current");
    if (name === "acquire" && this.#phase !== "new") fail("not_current");
    let end = addEnd(at, this.#timeout);
    if (!cleanup) {
      if (name === "acquire") this.#cap = addEnd(at, this.#sessionLifetime - MARGIN_MS);
      const cap = this.#cap;
      if (!cap) fail("not_current");
      end = minEnd(end, cap);
      if (this.#lease) end = minEnd(end, this.#lease.end);
    }
    this.#before(end, at);
    const op: Operation = { name, start: at, end, abort: new AbortController(), failed: false };
    this.#pending = op;
    op.timer = setTimeout(() => { op.failed = true; this.#invalidate(); },
      Math.min(end.wallMs - at.wallMs, end.boottimeMs - at.boottimeMs));
    op.timer.unref(); return op;
  }
  #guard(op: Operation): FlyCustodySample {
    if (this.#pending !== op || op.failed) fail("uncertain");
    if (this.#closing && op.name !== "release" && op.name !== "reconcile") { this.#invalidate(); fail("uncertain"); }
    const at = this.#sample(); this.#before(op.end, at);
    if (this.#lease && op.name !== "release" && op.name !== "reconcile") this.#before(this.#lease.end, at);
    return at;
  }
  #finish(op: Operation): void {
    clearTimeout(op.timer);
    if (this.#pending === op) this.#pending = undefined; // Only actual settlement frees this slot.
  }
  #path(suffix = ""): string { return `/v1/apps/${this.#expected.app}/machines/${this.#expected.machineId}${suffix}`; }
  async #request(op: Operation, method: FlyCustodyRequest["method"], suffix = "", body?: string, acquire = false): Promise<FlyCustodyReply> {
    this.#guard(op);
    const headers: Record<string, string> = { Accept: "application/json" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (this.#nonce) headers["fly-machine-lease-nonce"] = this.#nonce.toString();
    const redactedHeaders = () => ({ Accept: "application/json", ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...(this.#nonce ? { "fly-machine-lease-nonce": "[REDACTED]" } : {}) });
    Object.defineProperty(headers, "toJSON", { value: redactedHeaders });
    Object.defineProperty(headers, inspect.custom, { value: redactedHeaders });
    Object.freeze(headers);
    const request = { method, path: this.#path(suffix), headers, body, signal: op.abort.signal };
    const redacted = () => ({ method, path: request.path, headers: redactedHeaders(), ...(body === undefined ? {} : { body: "[REDACTED]" }) });
    Object.defineProperty(request, "toJSON", { value: redacted });
    Object.defineProperty(request, inspect.custom, { value: redacted }); Object.freeze(request);
    try {
      const reply = await this.#transport.request(request);
      // A DIRECT late acquisition ACK may retain cleanup custody, never validity.
      // GET/readback/serialized records can never populate this private nonce.
      if (acquire && reply?.status === 201) {
        const outer = object(reply.body), data = object(outer.data);
        if (outer.status === "success" && typeof data.nonce === "string" && /^[A-Za-z0-9_-]{1,256}$/.test(data.nonce)) {
          this.#nonce = Buffer.from(data.nonce);
        }
      }
      // A direct ACK of THIS conditional DELETE is a cleanup fact even if the
      // operation already expired/fenced. Record it before #guard can reject
      // timeliness; it never restores custody or frees the still-pending slot.
      if (op.name === "release" && this.#pending === op && method === "DELETE" && suffix === "/lease"
        && this.#releaseAttempted && this.#nonce && nonceEqual(headers["fly-machine-lease-nonce"], this.#nonce)
        && this.#successfulReleaseReply(reply)) {
        this.#releaseAck = true; this.#nonce.fill(0); this.#nonce = undefined;
      }
      this.#guard(op);
      if (!Number.isSafeInteger(reply?.status)) fail("rejected");
      return reply;
    } catch (error) {
      this.#invalidate();
      throw this.#error(error); // Never expose transport errors/response bodies/nonces.
    }
  }
  #successfulReleaseReply(reply: FlyCustodyReply): boolean {
    if (reply?.status !== 200) return false;
    try {
      const outer = object(reply.body), data = object(outer.data);
      return outer.status === "success" && data.ok === true;
    } catch { return false; }
  }
  #leaseReply(reply: FlyCustodyReply, status: number): { expiresAtMs: number; owner: string } {
    const nonce = this.#nonce;
    if (reply.status !== status || !nonce) fail("rejected");
    const outer = object(reply.body), data = object(outer.data);
    const owner = data.owner;
    if (outer.status !== "success" || !nonceEqual(data.nonce, nonce)
      || data.version !== this.#expected.instanceId || data.description !== this.#description
      || !text(owner) || !Number.isSafeInteger(data.expires_at)
      || (data.expires_at as number) <= 0 || !Number.isSafeInteger((data.expires_at as number) * 1000)) fail("lease_changed");
    return { expiresAtMs: (data.expires_at as number) * 1000, owner };
  }
  #machine(reply: FlyCustodyReply, expectedConfig = this.#expected.configSha256, sameVersion = true): void {
    if (reply.status !== 200) fail("identity_changed");
    const machine = object(reply.body);
    const version = machine.version ?? machine.instance_id;
    if (machine.id !== this.#expected.machineId || !text(version, 128)
      || machine.instance_id !== undefined && machine.instance_id !== version
      || sameVersion && version !== this.#expected.instanceId
      || flyMachineConfigDigest(machine.config) !== expectedConfig) fail("identity_changed");
  }
  #end(start: FlyCustodySample, ttlSeconds: number, expiresAtMs: number): End {
    const duration = Math.min(ttlSeconds * 1000, expiresAtMs - start.wallMs) - MARGIN_MS;
    const cap = this.#cap;
    if (!Number.isSafeInteger(duration) || duration <= 0 || !cap) fail("expired");
    return minEnd(cap, addEnd(start, duration));
  }
  async #verify(op: Operation): Promise<void> {
    const held = this.#lease;
    if (!held) fail("not_current");
    for (const machine of [false, true, false]) {
      if (machine) this.#machine(await this.#request(op, "GET"));
      else {
        const lease = this.#leaseReply(await this.#request(op, "GET", "/lease"), 200);
        if (lease.expiresAtMs !== held.expiresAtMs || lease.owner !== held.owner) fail("lease_changed");
      }
    }
  }
  #observation(op: Operation): FlyMachineLeaseObservation {
    const at = this.#guard(op);
    const lease = this.#lease, cap = this.#cap;
    if (this.#uncertain || this.#phase !== "held" || !lease || !cap) fail("not_current");
    this.#before(lease.end, at);
    return Object.freeze({ kind: "fly-machine-lease-observation-v1", scope: "machine-lease-only",
      app: this.#expected.app, machineId: this.#expected.machineId, instanceId: this.#expected.instanceId,
      configSha256: this.#expected.configSha256, challengeSha256: this.#challengeHash, serial: lease.serial,
      sessionEndWallMs: cap.wallMs, sessionEndBoottimeMs: cap.boottimeMs,
      originalEndWallMs: lease.end.wallMs, originalEndBoottimeMs: lease.end.boottimeMs,
      observedAtWallMs: at.wallMs, observedAtBoottimeMs: at.boottimeMs });
  }
  async acquire(): Promise<FlyMachineLeaseObservation> {
    const op = this.#begin("acquire"); this.#phase = "acquiring";
    try {
      const lease = this.#leaseReply(await this.#request(op, "POST", "/lease",
        JSON.stringify({ description: this.#description, ttl: this.#ttl }), true), 201);
      this.#lease = { ...lease, end: this.#end(op.start, this.#ttl, lease.expiresAtMs), serial: 1 };
      this.#before(this.#lease.end, this.#guard(op));
      await this.#verify(op); this.#phase = "held"; return this.#observation(op);
    } catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  async observe(): Promise<FlyMachineLeaseObservation> {
    const op = this.#begin("observe");
    try { await this.#verify(op); return this.#observation(op); }
    catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  async renew(ttlSeconds: number): Promise<FlyMachineLeaseObservation> {
    ttl(ttlSeconds); const op = this.#begin("renew");
    try {
      await this.#verify(op);
      const lease = this.#leaseReply(await this.#request(op, "POST", "/lease",
        JSON.stringify({ description: this.#description, ttl: ttlSeconds })), 201);
      const previous = this.#lease;
      if (!previous || lease.owner !== previous.owner) fail("lease_changed");
      this.#lease = { ...lease, end: this.#end(op.start, ttlSeconds, lease.expiresAtMs), serial: previous.serial + 1 };
      await this.#verify(op); return this.#observation(op);
    } catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  async mutate(mutation: FlyMachineMutation): Promise<Readonly<{ kind: "machine-mutation-http-ack"; action: FlyMachineMutation["action"] }>> {
    let method: FlyCustodyRequest["method"] = "POST", suffix: string, body: string | undefined, configHash: string | undefined;
    const action = mutation?.action;
    if (mutation?.action === "update") {
      const config = canonicalConfig(mutation.config); configHash = hash(config); suffix = "";
      body = `{"config":${config},"current_version":${JSON.stringify(this.#expected.instanceId)}}`;
    } else if (["start", "stop", "suspend", "cordon", "uncordon"].includes(action)) suffix = `/${action}`;
    else if (mutation?.action === "destroy" && typeof mutation.force === "boolean") {
      method = "DELETE"; suffix = `?force=${mutation.force}`;
    } else return fail("invalid_input");
    const op = this.#begin("mutate");
    try {
      await this.#verify(op); this.#phase = "retired"; // Before any mutation dispatch.
      const reply = await this.#request(op, method, suffix, body);
      if (reply.status !== 200) fail("rejected");
      if (action === "update") this.#machine(reply, configHash, false);
      return Object.freeze({ kind: "machine-mutation-http-ack", action });
    } catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  async release(): Promise<Readonly<{ kind: "machine-lease-release-http-ack" }>> {
    this.#closing = true; // Release intent prevents a concurrent success/unfence.
    if (this.#pending) {
      if (this.#pending.name !== "release") this.#invalidate();
      return fail("busy");
    }
    if (this.#releaseAck) return Object.freeze({ kind: "machine-lease-release-http-ack" });
    if (this.#releaseAttempted) return fail("release_already_attempted");
    if (!this.#nonce) { this.#invalidate(); return fail("nonce_unavailable"); }
    const op = this.#begin("release"); this.#releaseAttempted = true;
    if (!this.#uncertain) this.#phase = "retired";
    try {
      await this.#request(op, "DELETE", "/lease");
      if (!this.#releaseAck) fail("rejected");
      if (!this.#uncertain) this.#phase = "released";
      return Object.freeze({ kind: "machine-lease-release-http-ack" });
    } catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  async reconcile(): Promise<FlyLeaseReconciliation> {
    if (this.#phase === "new" || this.#phase === "held") return fail("not_current");
    const op = this.#begin("reconcile");
    const result = (observation: FlyLeaseReconciliation["observation"]): FlyLeaseReconciliation =>
      Object.freeze({ kind: "fly-machine-lease-reconciliation-v1", observation, custodyRestored: false });
    try {
      const machine = await this.#request(op, "GET");
      if (machine.status === 404) return result("machine_absent");
      if (machine.status !== 200) fail("rejected");
      try { this.#machine(machine); } catch { return result("machine_mismatch"); }
      const reply = await this.#request(op, "GET", "/lease");
      if (reply.status === 404) return result("lease_absent");
      const outer = object(reply.body), data = object(outer.data);
      if (reply.status !== 200 || outer.status !== "success") fail("rejected");
      if (typeof data.nonce !== "string" || !/^[A-Za-z0-9_-]{1,256}$/.test(data.nonce)
        || !Number.isSafeInteger(data.expires_at) || !text(data.version, 128)) fail("rejected");
      if (this.#nonce && nonceEqual(data.nonce, this.#nonce) && data.version === this.#expected.instanceId) return result("held_nonce");
      return result(!this.#nonce && data.description === this.#description
        ? "matching_challenge_without_acquisition_ack" : "different_lease");
    } catch (error) { this.#invalidate(); throw this.#error(error); }
    finally { this.#finish(op); }
  }
  #error(error: unknown): FlyCustodyError { return new FlyCustodyError(error instanceof FlyCustodyError ? error.code : "uncertain"); }
  #diagnostic(): Readonly<Record<string, unknown>> {
    return Object.freeze({ kind: "fly-machine-lease-session-v1", scope: "machine-lease-only", state: this.#phase,
      app: this.#expected.app, machineId: this.#expected.machineId, instanceId: this.#expected.instanceId,
      configSha256: this.#expected.configSha256, challengeSha256: this.#challengeHash,
      inFlight: this.#pending?.name ?? null, releaseAttempted: this.#releaseAttempted, releaseAcknowledged: this.#releaseAck });
  }
  status(): Readonly<Record<string, unknown>> {
    try {
      const at = this.#sample();
      if (this.#pending) this.#before(this.#pending.end, at);
      if (this.#phase === "held" && this.#lease) this.#before(this.#lease.end, at);
    } catch { /* Absorbing state is reported, never a raw clock error. */ }
    return this.#diagnostic();
  }
  toJSON(): Readonly<Record<string, unknown>> { return this.#diagnostic(); }
  [inspect.custom](): Readonly<Record<string, unknown>> { return this.#diagnostic(); }
}

/** Real session factory through an explicitly trusted injected HTTP transport.
 * No saved JSON/nonces/readback constructor exists. Retain it until all actual
 * request promises settle AND owned lease cleanup is acknowledged/reconciled.
 * Data observations cannot reconstruct this owner or grant managed egress. */
export function createFlyMachineLeaseSession(options: FlyMachineLeaseOptions): FlyMachineLeaseSession {
  try { return Object.freeze(new MachineLeaseSession(options)); }
  catch { return fail("invalid_input"); }
}
