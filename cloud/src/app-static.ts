// Static files for the web app with content negotiation. A file with
// precompressed copies next to it (name.br / name.gz, written by the build)
// is sent from those; other text files are compressed on first request and
// kept in a small in-memory cache keyed by path and mtime.
import crypto from "node:crypto";
import fs from "node:fs/promises";
import type { Stats } from "node:fs";
import type { IncomingMessage, ServerResponse } from "node:http";
import { promisify } from "node:util";
import zlib from "node:zlib";

type Coding = "br" | "gzip";
const CODINGS: Coding[] = ["br", "gzip"];
const SIDECAR: Record<Coding, string> = { br: ".br", gzip: ".gz" };
const ETAG_SUFFIX: Record<Coding, string> = { br: "-br", gzip: "-gz" };

// Below this a compressed body saves less than the headers cost.
const MIN_BYTES = 1024;
const CACHE_BYTES = 8 * 1024 * 1024;
const COMPRESSIBLE = /^(text\/|application\/(json|manifest\+json)|image\/svg\+xml)/;

const brotli = promisify(zlib.brotliCompress);
const gzip = promisify(zlib.gzip);

function compress(coding: Coding, body: Buffer): Promise<Buffer> {
  if (coding === "br") {
    return brotli(body, {
      params: {
        [zlib.constants.BROTLI_PARAM_MODE]: zlib.constants.BROTLI_MODE_TEXT,
        // 9 is within a few percent of 11 at a tenth of the time, which keeps
        // the first request after a start fast.
        [zlib.constants.BROTLI_PARAM_QUALITY]: 9,
        [zlib.constants.BROTLI_PARAM_SIZE_HINT]: body.length,
      },
    });
  }
  return gzip(body, { level: 9 });
}

// Codings the client accepts, best first. q=0 refuses a coding; "*" covers
// codings not named.
export function acceptedCodings(header: string | string[] | undefined): Coding[] {
  const value = Array.isArray(header) ? header.join(",") : header ?? "";
  const q = new Map<string, number>();
  for (const part of value.split(",")) {
    const [name, ...params] = part.trim().toLowerCase().split(";");
    if (!name) continue;
    const qParam = params.map((p) => p.trim()).find((p) => p.startsWith("q="));
    q.set(name === "x-gzip" ? "gzip" : name, qParam ? Number(qParam.slice(2)) || 0 : 1);
  }
  return CODINGS.filter((coding) => (q.get(coding) ?? q.get("*") ?? 0) > 0);
}

// State and transcript are fetched every few seconds. Use fast compression for
// these private, fresh responses instead of the static asset compression level.
// With `etag` the app sends it back as If-None-Match and an unchanged poll is
// a 304 (sendAppNotModified). The app keeps its own copy: nothing is stored.
export async function sendAppPollJson(req: IncomingMessage, res: ServerResponse, body: unknown, etag?: string): Promise<void> {
  if (res.destroyed) return;
  const json = Buffer.from(JSON.stringify(body));
  const compressed = json.length >= MIN_BYTES && acceptedCodings(req.headers["accept-encoding"]).includes("gzip");
  const payload = compressed ? await gzip(json, { level: zlib.constants.Z_BEST_SPEED }) : json;
  if (res.destroyed) return;
  res.writeHead(200, {
    "content-type": "application/json",
    "cache-control": "private, no-store",
    vary: "Accept-Encoding",
    "content-length": String(payload.length),
    ...(etag ? { etag: `"${etag}"` } : {}),
    ...(compressed ? { "content-encoding": "gzip" } : {}),
  });
  res.end(payload);
}

/** A poll whose answer the app already has (its If-None-Match). */
export function sendAppNotModified(res: ServerResponse, etag: string): void {
  if (res.destroyed) return;
  res.writeHead(304, { "cache-control": "private, no-store", etag: `"${etag}"` });
  res.end();
}

/** The ETag a poll's If-None-Match names, without quotes, or null. */
export function pollIfNoneMatch(req: IncomingMessage): string | null {
  const header = req.headers["if-none-match"];
  const match = /^\s*(?:W\/)?"([^"]+)"\s*$/.exec(Array.isArray(header) ? header[0] ?? "" : header ?? "");
  return match ? match[1] : null;
}

/** A content ETag for a poll body Cloud builds itself (the app's state). */
export function pollEtag(json: string): string {
  return crypto.createHash("sha1").update(json).digest("base64url").slice(0, 20);
}

/** A poll answer with its ETag: a 304 when the app has it already. */
export async function sendAppPoll(req: IncomingMessage, res: ServerResponse, body: unknown): Promise<void> {
  const etag = pollEtag(JSON.stringify(body));
  if (pollIfNoneMatch(req) === etag) {
    sendAppNotModified(res, etag);
    return;
  }
  await sendAppPollJson(req, res, body, etag);
}

function etagMatches(header: string | undefined, etag: string): boolean {
  if (!header) return false;
  if (header.trim() === "*") return true;
  return header.split(",").some((tag) => tag.trim().replace(/^W\//, "") === etag);
}

type Cached = { mtimeMs: number; size: number; body: Promise<Buffer>; bytes: number };
const cache = new Map<string, Cached>();
let cachedBytes = 0;

function dropCached(key: string): void {
  const entry = cache.get(key);
  if (!entry) return;
  cache.delete(key);
  cachedBytes -= entry.bytes;
}

function compressedBody(file: string, stat: Stats, coding: Coding): Promise<Buffer> {
  const key = `${coding}:${file}`;
  const hit = cache.get(key);
  if (hit && hit.mtimeMs === stat.mtimeMs && hit.size === stat.size) {
    cache.delete(key);
    cache.set(key, hit);
    return hit.body;
  }
  dropCached(key);
  const entry: Cached = { mtimeMs: stat.mtimeMs, size: stat.size, bytes: 0, body: fs.readFile(file).then((body) => compress(coding, body)) };
  cache.set(key, entry);
  entry.body.then(
    (body) => {
      if (cache.get(key) !== entry) return;
      entry.bytes = body.length;
      cachedBytes += body.length;
      for (const [oldest] of cache) {
        if (cachedBytes <= CACHE_BYTES) break;
        dropCached(oldest);
      }
    },
    () => {
      if (cache.get(key) === entry) cache.delete(key);
    },
  );
  return entry.body;
}

async function sidecars(file: string, stat: Stats): Promise<Partial<Record<Coding, string>>> {
  const found: Partial<Record<Coding, string>> = {};
  for (const coding of CODINGS) {
    const sidecar = `${file}${SIDECAR[coding]}`;
    const sidecarStat = await fs.stat(sidecar).catch((error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return null;
      throw error;
    });
    if (!sidecarStat) continue;
    if (sidecarStat.mtimeMs < stat.mtimeMs) {
      throw new Error(`${sidecar} is older than ${file}; rebuild the precompressed copies`);
    }
    found[coding] = sidecar;
  }
  return found;
}

// Sends `file` with `headers` (content-type, cache-control, ...), picking the
// best encoding the client accepts and answering a matching If-None-Match
// with 304.
export async function sendStaticFile(
  req: IncomingMessage,
  res: ServerResponse,
  file: string,
  headers: Record<string, string>,
): Promise<void> {
  const stat = await fs.stat(file);
  const accepted = acceptedCodings(req.headers["accept-encoding"]);
  const precompressed = await sidecars(file, stat);
  const hasSidecar = Object.keys(precompressed).length > 0;
  const negotiable = hasSidecar || (COMPRESSIBLE.test(headers["content-type"] ?? "") && stat.size >= MIN_BYTES);
  const coding = !negotiable ? undefined : hasSidecar ? accepted.find((c) => precompressed[c]) : accepted[0];
  const etag = `"${stat.size.toString(36)}-${Math.trunc(stat.mtimeMs).toString(36)}${coding ? ETAG_SUFFIX[coding] : ""}"`;

  const common: Record<string, string> = { ...headers, etag };
  if (negotiable) {
    common.vary = "Accept-Encoding";
  }
  if (etagMatches(req.headers["if-none-match"], etag)) {
    delete common["content-type"];
    res.writeHead(304, common);
    res.end();
    return;
  }
  const sidecar = coding && precompressed[coding];
  const body = sidecar
    ? await fs.readFile(sidecar)
    : coding
      ? await compressedBody(file, stat, coding)
      : await fs.readFile(file);
  res.writeHead(200, { ...common, ...(coding ? { "content-encoding": coding } : {}), "content-length": String(body.length) });
  res.end(body);
}
