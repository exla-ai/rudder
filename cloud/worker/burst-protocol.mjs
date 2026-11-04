// Wire format shared by the builder (builder.mjs) and `rudder-burst`
// (burst.mjs). Exec output streams as frames: one type byte, a 4-byte
// big-endian length, then the payload. The last frame is the exit status.

import { createHash, timingSafeEqual } from "node:crypto";
import path from "node:path";

export const FRAME = { stdout: 1, stderr: 2, exit: 3 };
export const RSYNC_UPGRADE = "rudder-rsync";
export const MAX_FRAME = 1024 * 1024;

export function encodeFrame(type, payload) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(String(payload));
  const header = Buffer.alloc(5);
  header.writeUInt8(type, 0);
  header.writeUInt32BE(body.length, 1);
  return Buffer.concat([header, body]);
}

/** Returns a push(chunk) that calls onFrame(type, payload) for every whole frame. */
export function frameDecoder(onFrame) {
  let pending = Buffer.alloc(0);
  return (chunk) => {
    pending = pending.length ? Buffer.concat([pending, chunk]) : chunk;
    while (pending.length >= 5) {
      const length = pending.readUInt32BE(1);
      if (length > MAX_FRAME) throw new Error("builder sent an oversized frame");
      if (pending.length < 5 + length) break;
      onFrame(pending[0], pending.subarray(5, 5 + length));
      pending = pending.subarray(5 + length);
    }
  };
}

export function sha256Hex(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function tokenMatches(presented, expectedSha256) {
  if (typeof presented !== "string" || !/^[0-9a-f]{64}$/.test(expectedSha256 || "")) return false;
  const actual = Buffer.from(sha256Hex(presented), "hex");
  const expected = Buffer.from(expectedSha256, "hex");
  return timingSafeEqual(actual, expected);
}

/** A build directory name: letters, digits, dot, dash, underscore. */
export function safeKey(value) {
  const key = String(value || "").replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^[.-]+/, "").slice(0, 80);
  if (!key) throw new Error("empty build key");
  return key;
}

/** root/key/rel, refusing anything that climbs out of root/key. */
export function buildDir(root, key, rel = "") {
  const base = path.resolve(root, safeKey(key));
  const dir = path.resolve(base, rel || ".");
  if (dir !== base && !dir.startsWith(base + path.sep)) throw new Error("path leaves the build directory");
  return dir;
}

/** Quote argv for `bash -c`, so `rudder-burst cargo test -- --nocapture` keeps its words. */
export function shellJoin(argv) {
  return argv.map((arg) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, `'\\''`)}'`)).join(" ");
}
