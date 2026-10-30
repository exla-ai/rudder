export type Decimal = string;
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type FaultCode = "inactive" | "invalid_request" | "forbidden" | "not_found" | "conflict"
  | "stale_fence" | "head_conflict" | "topic_conflict" | "handoff_required" | "counter_exhausted"
  | "busy" | "deadline_exceeded" | "unavailable" | "role_rejected" | "migration_conflict";

/** Deliberately excludes driver messages, SQL, parameters, connection details and causes. */
export class AuthorityError extends Error {
  constructor(readonly code: FaultCode) { super(`Project authority: ${code}`); this.name = "AuthorityError"; }
  toJSON(): { code: FaultCode } { return { code: this.code }; }
}
export function fail(code: FaultCode): never { throw new AuthorityError(code); }

export type Outcome<T> = { status: "committed"; value: T }
  | { status: "uncertain"; reconcile: "retry_same_operation" };
export interface Accepted<T = Json> {
  formatVersion: 1;
  authorityId: string;
  projectId: string;
  operationId: string;
  sequence: Decimal;
  kind: string;
  data: T;
}
/** Only an authenticated server/engine ingress constructs this context. Never deserialize it from an operation body. */
export interface TrustedContext {
  accountId: string;
  principalId: string;
  credentialId: string;
  credentialSha256: string;
  engine?: { instanceId: string; bootId: string; credentialId: string; credentialSha256: string };
}
export interface OwnerFence { ownerEpoch: Decimal; leaseId: string; }
export interface OwnerGrant extends OwnerFence {
  formatVersion: 1;
  authorityId: string;
  accountId: string;
  projectId: string;
  engineInstanceId: string;
  machineBootId: string;
  renewalSerial: Decimal;
  requestId: string;
  issuedAt: string;
  leaseUntil: string;
  durationMs: number;
}
export interface RootAdmission {
  rootId: string;
  kind: "source" | "environment" | "gate-evidence" | "build-contract" | "handoff";
  digest: string;
  sizeBytes: Decimal;
  retentionReceipt: Json;
}
export interface HandoffAdmission {
  projectId: string;
  witnessId: string;
  rootId: string;
  previousOwnerEpoch: Decimal;
  previousEngineInstanceId: string | null;
  legacyOrigin: string;
  processesQuiesced: true;
  effectsReconciled: true;
  launchersDisabled: true;
  credentialsFenced: true;
}
export interface BootstrapRequest {
  projectId: string;
  projectKey: string;
  idempotencyKey: string;
  witnessId: string;
  head: string;
  sourceRoot: string;
  environmentRoot: string;
  topics: string[];
}
export interface AcquireRequest {
  projectId: string;
  idempotencyKey: string;
  expectedOwnerEpoch: Decimal;
  witnessId: string;
}
export interface RenewRequest extends OwnerFence {
  projectId: string;
  requestId: string;
  expectedSerial: Decimal;
}
export interface ReleaseRequest extends OwnerFence {
  projectId: string;
  idempotencyKey: string;
  witnessId: string;
}
export interface DecisionRequest extends OwnerFence {
  projectId: string;
  idempotencyKey: string;
  topic: string;
  expectedVersion: Decimal;
  statement: Json;
}
export interface VerificationAdmission extends OwnerFence {
  projectId: string;
  verificationId: string;
  baseVersion: Decimal;
  baseHead: string;
  testedHead: string;
  sourceRoot: string;
  environmentRoot: string;
  gateReceiptRoot: string;
  buildContractRoot: string;
  topics: Record<string, Decimal>;
}
export interface IntegrationRequest extends OwnerFence {
  projectId: string;
  idempotencyKey: string;
  expectedVersion: Decimal;
  expectedHead: string;
  newHead: string;
  sourceRoot: string;
  environmentRoot: string;
  verificationId: string;
}

export const MAX_COUNTER = 9223372036854775807n;
export function counter(value: unknown, positive = false): Decimal {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]{0,18})$/.test(value)) fail("invalid_request");
  const parsed = BigInt(value);
  if (parsed > MAX_COUNTER || (positive && parsed === 0n)) fail("invalid_request");
  return value;
}
export function next(value: Decimal): Decimal {
  const parsed = BigInt(counter(value));
  if (parsed === MAX_COUNTER) fail("counter_exhausted");
  return String(parsed + 1n);
}
export function text(value: unknown, max = 180): string {
  if (typeof value !== "string" || value.length === 0 || Buffer.byteLength(value) > max || value.includes("\0")) fail("invalid_request");
  return value;
}
export function uuid(value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)) fail("invalid_request");
  return value;
}
export function hash(value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f]{64}$/.test(value)) fail("invalid_request");
  return value;
}
export function exact(value: object, keys: readonly string[]): void {
  if (!value || Object.getPrototypeOf(value) !== Object.prototype || Object.keys(value).some(key => !keys.includes(key))) fail("invalid_request");
}
export function canonical(value: unknown, max = 256 * 1024): Buffer {
  if (!Number.isSafeInteger(max) || max < 1 || max > 1024 * 1024) fail("invalid_request");
  const seen = new Set<object>();
  const chunks: string[] = []; let bytes = 0; let nodes = 0; const maxNodes = 16384;
  function append(part: string): void {
    const size = Buffer.byteLength(part); if (size > max - bytes) fail("invalid_request");
    bytes += size; chunks.push(part);
  }
  function quoted(part: string): void {
    // UTF-16 length is a lower bound on encoded JSON bytes, bounding stringify's allocation.
    if (part.length > max - bytes - 2) fail("invalid_request");
    append(JSON.stringify(part));
  }
  function visit(item: unknown, depth: number): void {
    if (depth > 24 || ++nodes > maxNodes) fail("invalid_request");
    if (item === null || typeof item === "boolean") { append(JSON.stringify(item)); return; }
    if (typeof item === "string") { quoted(item); return; }
    if (typeof item === "number") { if (!Number.isSafeInteger(item)) fail("invalid_request"); append(String(item)); return; }
    if (typeof item !== "object" || seen.has(item)) fail("invalid_request");
    seen.add(item);
    if (Array.isArray(item)) {
      if (Object.getPrototypeOf(item) !== Array.prototype || item.length > maxNodes - nodes) fail("invalid_request");
      for (const key in item) if (!/^(0|[1-9][0-9]*)$/.test(key) || Number(key) >= item.length) fail("invalid_request");
      append("[");
      for (let i = 0; i < item.length; i++) {
        const descriptor = Object.getOwnPropertyDescriptor(item, String(i));
        if (!descriptor || !("value" in descriptor)) fail("invalid_request");
        if (i) append(","); visit(descriptor.value, depth + 1);
      }
      append("]");
    }
    else {
      if (Object.getPrototypeOf(item) !== Object.prototype) fail("invalid_request");
      const record = item as Record<string, unknown>;
      const keys: string[] = [];
      for (const key in record) {
        if (!Object.hasOwn(record, key)) continue;
        if (keys.length >= maxNodes - nodes) fail("invalid_request"); keys.push(key);
      }
      keys.sort(); append("{");
      for (let i = 0; i < keys.length; i++) {
        const key = keys[i]; const descriptor = Object.getOwnPropertyDescriptor(record, key)!;
        if (!("value" in descriptor)) fail("invalid_request");
        if (i) append(","); quoted(key); append(":"); visit(descriptor.value, depth + 1);
      }
      append("}");
    }
    seen.delete(item);
  }
  visit(value, 0); return Buffer.from(chunks.join(""));
}
