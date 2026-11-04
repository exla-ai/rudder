/** Bounded blob I/O. Large compiler blobs are spooled to a private temporary
 * file, verified, then streamed to S3. They never occupy a full control-plane
 * heap buffer. No publication is acknowledged before the checksum is checked.
 */
import { createHash } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { lstat, mkdtemp, opendir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { FabricCasError, type FabricObjectRef } from "./fabric-cas.js";

/** Only remove this service's private spools whose owning process is gone.
 * Live owners and ambiguous permissions remain untouched. Fly replicas have
 * separate temporary filesystems; this is local crash cleanup, not remote GC.
 */
export async function cleanupFabricUploadSpools(): Promise<number> {
  let removed = 0;
  const directory = await opendir(tmpdir());
  for await (const entry of directory) {
    const match = /^rudder-fabric-upload-([1-9][0-9]*)-[A-Za-z0-9]+$/.exec(entry.name);
    if (!match || !entry.isDirectory()) continue;
    const pid = Number(match[1]);
    if (!Number.isSafeInteger(pid) || pid > 0x7fffffff) continue;
    const location = path.join(tmpdir(), entry.name);
    const stat = await lstat(location).catch(() => null);
    if (!stat?.isDirectory() || (stat.mode & 0o777) !== 0o700 || process.getuid && stat.uid !== process.getuid()) continue;
    try { process.kill(pid, 0); continue; }
    catch (failure) { if ((failure as NodeJS.ErrnoException).code !== "ESRCH") continue; }
    await rm(location, { recursive: true, force: true });
    removed++;
  }
  return removed;
}

export async function withVerifiedSpool<T>(
  reference: FabricObjectRef, source: AsyncIterable<Uint8Array>,
  consume: (file: string) => Promise<T>, deadlineMs = 30_000,
): Promise<T> {
  const directory = await mkdtemp(path.join(tmpdir(), `rudder-fabric-upload-${process.pid}-`));
  const file = path.join(directory, "content");
  const hash = createHash("sha256");
  let bytes = 0;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), deadlineMs);
  try {
    const verify = new Transform({
      transform(chunk: Buffer, _encoding, callback) {
        bytes += chunk.length;
        if (bytes > reference.size_bytes) return callback(new FabricCasError("too_large", "Blob exceeds its declared size", 413));
        hash.update(chunk);
        callback(null, chunk);
      },
    });
    // Preserve a real HTTP/Node stream so abort destroys the original socket.
    // Wrapping a stalled Readable in an async iterator can wait forever for its
    // outstanding next() during iterator teardown.
    const readable = source instanceof Readable ? source : Readable.from(source, { objectMode: false });
    await pipeline(readable, verify, createWriteStream(file, { flags: "wx", mode: 0o600 }), { signal: controller.signal });
    clearTimeout(timer);
    if (bytes !== reference.size_bytes || hash.digest("hex") !== reference.digest) throw new FabricCasError("checksum_mismatch", "Transferred blob failed checksum verification", 422);
    return await consume(file);
  } catch (failure) {
    if (controller.signal.aborted) throw new FabricCasError("body_timeout", "Blob upload exceeded its deadline", 408);
    throw failure;
  } finally {
    clearTimeout(timer);
    await rm(directory, { recursive: true, force: true });
  }
}

/** The consumer must exhaust the stream. Interrupted/partial reads never
 * create retained verification evidence. Checks run after the last yield.
 */
export async function checkedBlobStream(
  reference: FabricObjectRef, source: AsyncIterable<Uint8Array>,
  consume: (body: AsyncIterable<Uint8Array>) => Promise<void>,
): Promise<void> {
  let complete = false;
  async function* checked() {
    let bytes = 0;
    const hash = createHash("sha256");
    for await (const chunk of source) {
      bytes += chunk.byteLength;
      if (bytes > reference.size_bytes) throw new FabricCasError("checksum_mismatch", "Stored blob exceeds its declared size", 422);
      hash.update(chunk);
      yield chunk;
    }
    if (bytes !== reference.size_bytes || hash.digest("hex") !== reference.digest) throw new FabricCasError("checksum_mismatch", "Stored blob failed checksum verification", 422);
    complete = true;
  }
  await consume(checked());
  if (!complete) throw new FabricCasError("incomplete_read", "Blob consumer did not complete verification", 503);
}

export const blobFileStream = (file: string) => createReadStream(file, { highWaterMark: 64 * 1024 });
