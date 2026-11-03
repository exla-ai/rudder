/** Trusted one-shot, off-App profile selection. No CLI route, refresh, environment
 * credential lookup, provider request or console output. Without an explicit
 * access-only import, JSON loading reads the entire normal private store. Only
 * the exact selected API key or Codex access-only snapshot may leave. */
import { constants, fstat, close, write, type BigIntStats } from "node:fs";
import { readFile, readlink } from "node:fs/promises";
import { loadAuthStore } from "./state.js";
import type { AuthProfileCredential } from "./types.js";
import { performance } from "node:perf_hooks";
import { codexOAuthAccessSnapshot, MAX_CODEX_ACCESS_SNAPSHOT_BYTES } from "./managed-codex-access-snapshot.js";
import type { CodexOAuthAccessSnapshotV1 } from "./managed-codex-access-snapshot.js";
import { loadImportedCodexOAuthAccessSnapshot } from "./managed-codex-access-snapshot-store.js";

export type ManagedTaskProfileSelection = { profileId: string; backend: "codex" | "claude" };
export type ManagedTaskProfileMetadata = ManagedTaskProfileSelection & {
  formatVersion: 1; credentialKind: "api_key" | "oauth" | "token";
  provider: "openai" | "openai-codex" | "anthropic";
};
const messages = {
  INVALID_SELECTION: "An exact supported backend and private profile ID are required.",
  PROFILE_UNAVAILABLE: "The exact selected private profile is unavailable.",
  PROFILE_BACKEND_MISMATCH: "The selected private profile belongs to another backend.",
  STORE_UNAVAILABLE: "The normal private profile store could not be loaded.",
  UNSUPPORTED_OAUTH: "Selected OAuth is unsupported; no key or billing fallback is permitted.",
  UNSUPPORTED_TOKEN: "Selected token profiles are unsupported for managed API-key handoff.",
  INVALID_KEY: "The selected API key is invalid or exceeds the private pipe frame bound.",
  PRIVATE_PIPE_REQUIRED: "A private inherited nonblocking anonymous pipe writer is required.",
  HANDOFF_FAILED: "The one-shot credential write failed; do not retry this handoff.",
  HANDOFF_UNCERTAIN: "The credential write was partial; discard the frame and do not retry.",
  PIPE_CLOSE_UNCERTAIN: "Private pipe close failed; retain and reap the helper before recovery.",
  ALREADY_USED: "This process has already attempted its one-shot credential handoff.",
  INVALID_OAUTH_ACCESS_SNAPSHOT: "The selected Codex OAuth access snapshot is unavailable or expired.",
} as const;
export type ManagedTaskProfileErrorCode = keyof typeof messages;
type Failure = { ok: false; error: { code: ManagedTaskProfileErrorCode; message: string } };
export type ManagedTaskProfileResult = { ok: true; metadata: ManagedTaskProfileMetadata } | Failure;
type Selected = { ok: true; metadata: ManagedTaskProfileMetadata; credential: AuthProfileCredential; snapshot?: never }
  | { ok: true; metadata: ManagedTaskProfileMetadata; snapshot: CodexOAuthAccessSnapshotV1; credential?: never } | Failure;
function failure(code: ManagedTaskProfileErrorCode): Failure { return { ok: false, error: { code, message: messages[code] } }; }
function selection(value: unknown): ManagedTaskProfileSelection | undefined {
  if (!value || typeof value !== "object" || Object.keys(value).length !== 2) return;
  const v = value as Record<string, unknown>;
  if (!Object.hasOwn(v, "profileId") || !Object.hasOwn(v, "backend") || typeof v.profileId !== "string"
    || !v.profileId || Buffer.byteLength(v.profileId, "utf8") > 256 || /[\p{Cc}\uD800-\uDFFF]/u.test(v.profileId)
    || (v.backend !== "codex" && v.backend !== "claude")) return;
  return { profileId: v.profileId, backend: v.backend };
}
async function select(value: unknown): Promise<Selected> {
  const requested = selection(value);
  if (!requested) return failure("INVALID_SELECTION");
  let snapshot: CodexOAuthAccessSnapshotV1 | undefined;
  try { snapshot = await loadImportedCodexOAuthAccessSnapshot(); }
  catch { return failure("INVALID_OAUTH_ACCESS_SNAPSHOT"); }
  if (snapshot) {
    if (requested.backend !== "codex" || snapshot.profileId !== requested.profileId)
      return failure("PROFILE_BACKEND_MISMATCH");
    return { ok: true, snapshot, metadata: { ...requested, formatVersion: 1,
      credentialKind: "oauth", provider: "openai-codex" } };
  }
  const store = await loadAuthStore(); // Reuse normal legacy mode/apiKey normalization, never syncExternalCredentials.
  if (!Object.hasOwn(store.profiles, requested.profileId)) return failure("PROFILE_UNAVAILABLE");
  const credential = store.profiles[requested.profileId];
  const provider = credential.provider;
  const matches = requested.backend === "claude" ? provider === "anthropic"
    : provider === "openai" || (provider === "openai-codex" && credential.type === "oauth");
  if (!matches) return failure("PROFILE_BACKEND_MISMATCH");
  return { ok: true, credential, metadata: { ...requested, formatVersion: 1,
    credentialKind: credential.type, provider: provider as ManagedTaskProfileMetadata["provider"] } };
}
/** Metadata only. This does not grant launch or retain a credential snapshot.
 * Handoff re-reads the exact profile and refuses any changed/unsupported kind. */
export async function inspectManagedTaskProfile(value: unknown): Promise<ManagedTaskProfileResult> {
  try { const selected = await select(value); return selected.ok ? { ok: true, metadata: selected.metadata } : selected; }
  catch { return failure("STORE_UNAVAILABLE"); } // Never preserve a secret-bearing cause chain.
}
function stat(fd: number): Promise<BigIntStats> {
  return new Promise((resolve, reject) => fstat(fd, { bigint: true }, (error, value) => error ? reject(error) : resolve(value)));
}
async function privateWriter(fd: number): Promise<boolean> {
  if (process.platform !== "linux" || typeof process.geteuid !== "function") return false;
  const original = await stat(fd);
  const valid = (s: BigIntStats) => s.isFIFO() && s.uid === BigInt(process.geteuid!())
    && (s.mode & 0o777n) === 0o600n && s.nlink === 1n;
  if (!valid(original)) return false;
  const link = await readlink(`/proc/self/fd/${fd}`);
  const info = await readFile(`/proc/self/fdinfo/${fd}`, "utf8");
  const flagsText = /^flags:\s*([0-7]+)$/m.exec(info)?.[1];
  const inode = /^ino:\s*(\d+)$/m.exec(info)?.[1];
  if (info.length > 4096 || !flagsText || inode !== original.ino.toString() || link !== `pipe:[${original.ino}]`) return false;
  const flags = BigInt(`0o${flagsText}`);
  if ((flags & 3n) !== BigInt(constants.O_WRONLY) || (flags & BigInt(constants.O_NONBLOCK)) === 0n) return false;
  const current = await stat(fd);
  return valid(current) && current.dev === original.dev && current.ino === original.ino;
}
function writeFrame(fd: number, frame: Buffer): Promise<number> {
  return new Promise((resolve, reject) => write(fd, frame, 0, frame.length, null,
    (error, written) => error ? reject(error) : resolve(written)));
}
function closeWriter(fd: number): Promise<boolean> {
  return new Promise((resolve) => close(fd, (error) => resolve(!error || error.code === "EBADF")));
}
export const MAX_MANAGED_TASK_PIPE_KEY_BYTES = 4092; // 4-byte length + key <= Linux PIPE_BUF 4096.
let handoffAttempted = false; // Absorbing process-local one-shot gate, including failed/uncertain attempts.

/** Takes sole ownership of fd>=3 for one attempt and awaits the actual write and
 * close callbacks. Caller provides a fresh empty pipe2(O_NONBLOCK|O_CLOEXEC),
 * closes its duplicate writer, drains the reader, and retains/reaps this helper.
 * No descriptor/path/env fallback, retry, detached write or timeout fiction. */
export async function handoffManagedTaskApiKey(value: unknown, inheritedWriteFd: number): Promise<ManagedTaskProfileResult> {
  if (handoffAttempted) return failure("ALREADY_USED");
  handoffAttempted = true;
  if (!Number.isSafeInteger(inheritedWriteFd) || inheritedWriteFd < 3 || inheritedWriteFd > 0x7fffffff) return failure("PRIVATE_PIPE_REQUIRED");
  let result: ManagedTaskProfileResult = failure("PRIVATE_PIPE_REQUIRED");
  let frame: Buffer | undefined;
  try {
    if (await privateWriter(inheritedWriteFd)) {
      try {
        const selected = await select(value);
        if (!selected.ok) result = selected;
        else if (selected.snapshot || selected.credential.type === "oauth") result = failure("UNSUPPORTED_OAUTH");
        else if (selected.credential.type !== "api_key") result = failure("UNSUPPORTED_TOKEN");
        else {
          const key = selected.credential.key;
          const size = typeof key === "string" ? Buffer.byteLength(key, "utf8") : 0;
          if (typeof key !== "string" || size === 0 || size > MAX_MANAGED_TASK_PIPE_KEY_BYTES || /[\0\r\n\uD800-\uDFFF]/u.test(key)) result = failure("INVALID_KEY");
          else {
            frame = Buffer.alloc(4 + size); frame.writeUInt32BE(size); frame.write(key, 4, size, "utf8");
            try { result = await writeFrame(inheritedWriteFd, frame) === frame.length
              ? { ok: true, metadata: selected.metadata } : failure("HANDOFF_UNCERTAIN"); }
            catch { result = failure("HANDOFF_FAILED"); }
          }
        }
      } catch { result = failure("STORE_UNAVAILABLE"); }
    }
  } catch { result = failure("PRIVATE_PIPE_REQUIRED"); }
  // Even rejected profiles/invalid pipe types consume and close the inherited fd.
  // A close error returns failure; Root still owns actual process reap/recovery.
  const closed = await closeWriter(inheritedWriteFd);
  frame?.fill(0); // JS store/key strings are immutable; do not claim full heap erasure.
  return closed ? result : failure("PIPE_CLOSE_UNCERTAIN");
}

/** A distinct access-only opt-in. One bounded frame, concurrent native drain,
 * partial-write progress only, no whole-frame replay or renewed handoff clock.
 * Native retains the original helper through actual wait and all three EOFs. */
export async function handoffManagedTaskCodexOAuthAccessSnapshot(value: unknown,
  inheritedWriteFd: number): Promise<ManagedTaskProfileResult> {
  if (handoffAttempted) return failure("ALREADY_USED");
  handoffAttempted = true;
  if (!Number.isSafeInteger(inheritedWriteFd) || inheritedWriteFd < 3 || inheritedWriteFd > 0x7fffffff)
    return failure("PRIVATE_PIPE_REQUIRED");
  const monotonicStart = performance.now(), wallStart = Date.now();
  let result: ManagedTaskProfileResult = failure("PRIVATE_PIPE_REQUIRED");
  let frame: Buffer | undefined;
  try {
    if (await privateWriter(inheritedWriteFd)) {
      const selected = await select(value);
      if (!selected.ok) result = selected;
      else if (selected.metadata.backend !== "codex" || (!selected.snapshot && (selected.credential.type !== "oauth"
        || selected.credential.provider !== "openai-codex"))) result = failure("UNSUPPORTED_OAUTH");
      else {
        try {
          const snapshot = selected.snapshot ?? codexOAuthAccessSnapshot(selected.metadata.profileId, selected.credential);
          const body = Buffer.from(JSON.stringify(snapshot), "utf8");
          try {
            if (body.length === 0 || body.length > MAX_CODEX_ACCESS_SNAPSHOT_BYTES) throw new Error();
            frame = Buffer.alloc(4 + body.length); frame.writeUInt32BE(body.length); body.copy(frame, 4);
          } finally { body.fill(0); }
          let offset = 0;
          while (offset < frame.length) {
            const elapsed = performance.now() - monotonicStart, wall = Date.now();
            if (elapsed < 0 || elapsed >= 2000 || wall < wallStart || wall - wallStart >= 2000
              || wall >= snapshot.expiresAtMs) throw new Error();
            const tail = frame.subarray(offset);
            try {
              const written = await writeFrame(inheritedWriteFd, tail);
              if (!Number.isSafeInteger(written) || written <= 0 || written > tail.length) throw new Error();
              offset += written;
            } catch (error) {
              if ((error as NodeJS.ErrnoException)?.code !== "EAGAIN"
                && (error as NodeJS.ErrnoException)?.code !== "EWOULDBLOCK") throw error;
              await new Promise<void>(resolve => setTimeout(resolve, 5));
            }
          }
          // Recheck after the actual last callback. Native rejects expired or
          // failed metadata even if some or all secret bytes reached its pipe.
          if (performance.now() - monotonicStart >= 2000 || Date.now() < wallStart
            || Date.now() >= Math.min(wallStart + 2000, snapshot.expiresAtMs)) throw new Error();
          result = { ok: true, metadata: selected.metadata };
        } catch { result = failure("INVALID_OAUTH_ACCESS_SNAPSHOT"); }
      }
    }
  } catch { result = failure("STORE_UNAVAILABLE"); }
  const closed = await closeWriter(inheritedWriteFd);
  frame?.fill(0); // Immutable JS strings are not claimed erased.
  return closed ? result : failure("PIPE_CLOSE_UNCERTAIN");
}
