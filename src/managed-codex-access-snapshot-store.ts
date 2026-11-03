/** Explicit access-only import. This file is separate from auth-profiles.json:
 * no refresh credential, external lookup, renewal or writeback is supported. */
import { constants, type BigIntStats } from "node:fs";
import { lstat, open, realpath, type FileHandle } from "node:fs/promises";
import path from "node:path";
import { rudderHome } from "./util.js";
import type { AuthProfileCredential } from "./types.js";
import { codexOAuthAccessSnapshot, CodexAccessSnapshotError, MAX_CODEX_ACCESS_SNAPSHOT_BYTES,
  type CodexOAuthAccessSnapshotV1 } from "./managed-codex-access-snapshot.js";

export const CODEX_ACCESS_SNAPSHOT_FILENAME = "managed-codex-access-snapshot-v1.json";
export const CODEX_ACCESS_SNAPSHOT_IMPORTED_ACK = "CODEX_OAUTH_ACCESS_SNAPSHOT_IMPORTED_V1\n";
const fail = (): never => { throw new CodexAccessSnapshotError(); };

/** Canonical UTF8 JSON, without a line terminator. Duplicate/unknown fields,
 * mismatched claims, noncanonical encodings and expired access all refuse.
 * Claims are selection/deadline DATA; the original backend authenticates access. */
export function validateCodexOAuthAccessSnapshot(raw: Uint8Array, nowMs = Date.now()): CodexOAuthAccessSnapshotV1 {
  try {
    if (raw.byteLength === 0 || raw.byteLength > MAX_CODEX_ACCESS_SNAPSHOT_BYTES) return fail();
    const text = new TextDecoder("utf-8", { fatal: true }).decode(raw);
    const value: unknown = JSON.parse(text);
    if (!value || typeof value !== "object" || Array.isArray(value)) return fail();
    const v = value as Record<string, unknown>;
    // The existing validator only reads these four access-only fields. This
    // structural view creates no refresh/expires value and is never persisted
    // as a normal OAuthCredential (whose refresh field is intentionally absent).
    const accessOnly = { type: "oauth", provider: v.provider, access: v.accessToken,
      accountId: v.chatgptAccountId } as AuthProfileCredential;
    const snapshot = codexOAuthAccessSnapshot(v.profileId as string, accessOnly, nowMs);
    const canonical = Buffer.from(JSON.stringify(snapshot), "utf8");
    try { if (!canonical.equals(raw)) return fail(); }
    finally { canonical.fill(0); }
    return snapshot;
  } catch { return fail(); }
}

const same = (a: BigIntStats, b: BigIntStats) => a.dev === b.dev && a.ino === b.ino
  && a.uid === b.uid && a.gid === b.gid && a.mode === b.mode && a.nlink === b.nlink;
const unchanged = (a: BigIntStats, b: BigIntStats) => same(a, b) && a.size === b.size
  && a.mtimeNs === b.mtimeNs && a.ctimeNs === b.ctimeNs;
const privateFile = (s: BigIntStats) => s.isFile() && s.uid === BigInt(process.geteuid!())
  && (s.mode & 0o7777n) === 0o600n && s.nlink === 1n;
type Home = { name: string; handle: FileHandle; original: BigIntStats };
async function privateHome(): Promise<Home> {
  if (typeof process.geteuid !== "function") return fail();
  const name = rudderHome();
  // Canonical spelling prevents symlink aliases anywhere in the home path.
  if (await realpath(name) !== name) return fail();
  const original = await lstat(name, { bigint: true });
  if (!original.isDirectory() || original.uid !== BigInt(process.geteuid())
    || (original.mode & 0o7777n) !== 0o700n) return fail();
  const handle = await open(name, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
  try {
    if (!unchanged(original, await handle.stat({ bigint: true }))) return fail();
    return { name, handle, original };
  } catch (error) { await handle.close(); throw error; }
}
async function checkHome(home: Home): Promise<void> {
  if (await realpath(home.name) !== home.name
    || !same(home.original, await lstat(home.name, { bigint: true }))
    || !same(home.original, await home.handle.stat({ bigint: true }))) return fail();
}
async function checkFile(name: string, handle: FileHandle, before: BigIntStats): Promise<void> {
  if (!privateFile(before) || !unchanged(before, await handle.stat({ bigint: true }))
    || !unchanged(before, await lstat(name, { bigint: true }))) return fail();
}
async function closePrivate(file: FileHandle | undefined, home: Home | undefined): Promise<void> {
  let failed = false;
  try { await file?.close(); } catch { failed = true; }
  try { await home?.handle.close(); } catch { failed = true; }
  if (failed) return fail();
}

/** Only actual absence selects the normal profile store. A present invalid,
 * foreign-profile or stale file is an explicit refusal, never a fallback. */
export async function loadImportedCodexOAuthAccessSnapshot(): Promise<CodexOAuthAccessSnapshotV1 | undefined> {
  const filename = path.join(rudderHome(), CODEX_ACCESS_SNAPSHOT_FILENAME);
  try { await lstat(filename); }
  catch (error) { if ((error as NodeJS.ErrnoException)?.code === "ENOENT") return undefined; return fail(); }
  let home: Home | undefined, file: FileHandle | undefined, raw: Buffer | undefined;
  try {
    home = await privateHome();
    file = await open(filename, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
    const before = await file.stat({ bigint: true });
    if (!privateFile(before) || before.size < 2n || before.size > BigInt(MAX_CODEX_ACCESS_SNAPSHOT_BYTES + 1)) return fail();
    raw = Buffer.alloc(Number(before.size) + 1);
    let offset = 0;
    while (offset < raw.length) {
      const { bytesRead } = await file.read(raw, offset, raw.length - offset, null);
      if (bytesRead === 0) break;
      offset += bytesRead;
    }
    if (offset !== Number(before.size) || raw[offset - 1] !== 10) return fail();
    await checkFile(filename, file, before); await checkHome(home);
    return validateCodexOAuthAccessSnapshot(raw.subarray(0, offset - 1));
  } catch { return fail(); }
  finally { raw?.fill(0); await closePrivate(file, home); }
}

/** One exclusive durable publication into an existing private home. Failure
 * retains any partial file; a subsequent import cannot overwrite or replay it.
 * Same-UID code is trusted: path/FD checks bound races, not a sandbox against
 * another process with the owner's authority to rename the whole home. */
export async function importCodexOAuthAccessSnapshot(raw: Uint8Array): Promise<void> {
  let home: Home | undefined, file: FileHandle | undefined, body: Buffer | undefined;
  try {
    validateCodexOAuthAccessSnapshot(raw);
    body = Buffer.alloc(raw.byteLength + 1); body.set(raw); body[body.length - 1] = 10;
    home = await privateHome(); await checkHome(home);
    const filename = path.join(home.name, CODEX_ACCESS_SNAPSHOT_FILENAME);
    file = await open(filename, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
    const created = await file.stat({ bigint: true });
    if (!privateFile(created) || created.size !== 0n) return fail();
    let offset = 0;
    while (offset < body.length) {
      const { bytesWritten } = await file.write(body, offset, body.length - offset, null);
      if (!Number.isSafeInteger(bytesWritten) || bytesWritten <= 0 || bytesWritten > body.length - offset) return fail();
      offset += bytesWritten;
    }
    await file.sync();
    const written = await file.stat({ bigint: true });
    if (!same(created, written) || written.size !== BigInt(body.length)) return fail();
    await checkFile(filename, file, written); await checkHome(home);
    await home.handle.sync();
    await checkFile(filename, file, written); await checkHome(home);
    // Recheck the actual token deadline after publication, before success ACK.
    validateCodexOAuthAccessSnapshot(raw);
  } catch { return fail(); }
  finally { body?.fill(0); await closePrivate(file, home); }
}

/** Raw canonical JSON then EOF, capped during the original stdin read. No
 * token arguments, log output, unbounded accumulation or second input read. */
export async function importCodexOAuthAccessSnapshotStdin(input: AsyncIterable<Uint8Array>): Promise<void> {
  const raw = Buffer.alloc(MAX_CODEX_ACCESS_SNAPSHOT_BYTES + 1);
  let offset = 0;
  try {
    for await (const chunk of input) {
      if (!(chunk instanceof Uint8Array) || chunk.byteLength > MAX_CODEX_ACCESS_SNAPSHOT_BYTES - offset) return fail();
      raw.set(chunk, offset); offset += chunk.byteLength;
    }
    await importCodexOAuthAccessSnapshot(raw.subarray(0, offset));
  } catch { return fail(); }
  finally { raw.fill(0); }
}
