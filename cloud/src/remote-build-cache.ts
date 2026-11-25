import { createHash } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import {
  DeleteObjectCommand,
  GetObjectCommand,
  HeadObjectCommand,
  PutObjectCommand,
  type S3Client,
} from "@aws-sdk/client-s3";

const PREFIX = "/api/rudder/cache";
const DEFAULT_MAX_OBJECT_BYTES = 256 * 1024 * 1024;
const MAX_EVENTS_BODY_BYTES = 256 * 1024;
const MAX_PROPFIND_BODY_BYTES = 64 * 1024;
const MAX_MKCOL_BODY_BYTES = 16 * 1024;
const MAX_KEY_LENGTH = 512;
const MAX_ARTIFACT_TAG_BYTES = 512;
const MAX_INFLIGHT_TOTAL = 32;
const MAX_INFLIGHT_PER_ACCOUNT = 16;
const ACCOUNT_RECENT_LIMIT = 30;
const GLOBAL_RECENT_LIMIT = 50;
const UPLOAD_TIMEOUT_MS = 60_000;
const SMALL_BODY_TIMEOUT_MS = 5_000;
const TURBO_HASH_RE = /^[A-Za-z0-9._-]{8,128}$/;

export interface RemoteCacheObject {
  body: Buffer;
  contentType?: string;
  etag?: string;
  lastModified?: Date;
  metadata?: Record<string, string>;
}

export interface RemoteCacheHead {
  size: number;
  contentType?: string;
  etag?: string;
  lastModified?: Date;
  metadata?: Record<string, string>;
}

export interface RemoteCacheStorage {
  get(key: string, maxBytes: number): Promise<RemoteCacheObject | null>;
  head(key: string): Promise<RemoteCacheHead | null>;
  put(
    key: string,
    body: Buffer,
    options?: { contentType?: string; metadata?: Record<string, string> },
  ): Promise<{ etag?: string }>;
  delete(key: string): Promise<void>;
}

export type RemoteCacheToolStats = {
  hits: number;
  misses: number;
  writes: number;
  errors: number;
  bytesRead: number;
  bytesWritten: number;
  lastActivityAt: number | null;
};

type RemoteCacheTool = "sccache" | "turbo" | "buck2";

export type RemoteCacheEvent = {
  at: number;
  tool: RemoteCacheTool;
  op: "hit" | "miss" | "put" | "error";
  key: string;
  bytes: number;
  ms: number;
};

export type RemoteCacheAccountStats = {
  enabled: boolean;
  bucket: string | null;
  sccache: RemoteCacheToolStats;
  turbo: RemoteCacheToolStats;
  buck2: RemoteCacheToolStats;
  recent: RemoteCacheEvent[];
};

export type RemoteBuildCacheHandler = ((
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
) => Promise<void>) & {
  statsForAccount(accountId: string): RemoteCacheAccountStats;
  stats(): {
    enabled: boolean;
    bucket: string | null;
    accounts: number;
    sccache: RemoteCacheToolStats;
    turbo: RemoteCacheToolStats;
    buck2: RemoteCacheToolStats;
    recent: RemoteCacheEvent[];
  };
};

export class RemoteCacheError extends Error {
  constructor(
    public readonly code: string,
    message: string,
    public readonly status = 400,
  ) {
    super(message);
    this.name = "RemoteCacheError";
  }
}

function fail(code: string, message: string, status = 400): never {
  throw new RemoteCacheError(code, message, status);
}

function sha256Hex(value: string | Buffer): string {
  return createHash("sha256").update(value).digest("hex");
}

function isNotFoundError(error: unknown): boolean {
  const err = error as {
    name?: string;
    Code?: string;
    code?: string;
    $metadata?: { httpStatusCode?: number };
  } | null;
  if (!err || typeof err !== "object") return false;
  if (err.$metadata?.httpStatusCode === 404) return true;
  const name = err.name ?? err.Code ?? err.code;
  return name === "NoSuchKey" || name === "NotFound";
}

function xmlEscape(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&apos;");
}

function emptyToolStats(): RemoteCacheToolStats {
  return {
    hits: 0,
    misses: 0,
    writes: 0,
    errors: 0,
    bytesRead: 0,
    bytesWritten: 0,
    lastActivityAt: null,
  };
}

function cloneToolStats(stats: RemoteCacheToolStats): RemoteCacheToolStats {
  return { ...stats };
}

function pushBounded<T>(list: T[], item: T, limit: number): void {
  list.push(item);
  if (list.length > limit) {
    list.splice(0, list.length - limit);
  }
}

export class S3RemoteCacheStorage implements RemoteCacheStorage {
  private readonly client: S3Client;
  private readonly bucket: string;
  private readonly timeoutMs: number;

  constructor(options: { client: S3Client; bucket: string; timeoutMs?: number }) {
    const bucket = options.bucket.trim();
    const timeoutMs = options.timeoutMs ?? 20_000;
    if (!bucket || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 120_000) {
      fail("invalid_storage", "Invalid S3 remote cache configuration", 500);
    }
    this.client = options.client;
    this.bucket = bucket;
    this.timeoutMs = timeoutMs;
  }

  async get(key: string, maxBytes: number): Promise<RemoteCacheObject | null> {
    const signal = AbortSignal.timeout(this.timeoutMs);
    let response;
    try {
      response = await this.client.send(
        new GetObjectCommand({ Bucket: this.bucket, Key: key }),
        { abortSignal: signal },
      );
    } catch (error) {
      if (isNotFoundError(error)) return null;
      throw error;
    }

    if (response.ContentLength !== undefined && response.ContentLength > maxBytes) {
      (response.Body as { destroy?: () => void } | undefined)?.destroy?.();
      fail("too_large", "Cached object exceeds byte limit", 413);
    }

    if (!response.Body) {
      return {
        body: Buffer.alloc(0),
        contentType: response.ContentType,
        etag: response.ETag,
        lastModified: response.LastModified,
        metadata: response.Metadata ? { ...response.Metadata } : undefined,
      };
    }

    const bodyStream = response.Body as AsyncIterable<Uint8Array> & {
      destroy?: (error?: Error) => void;
    };
    const onAbort = () => {
      bodyStream.destroy?.(new RemoteCacheError("storage_timeout", "Cache read exceeded deadline", 503));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    try {
      if (signal.aborted) onAbort();
      const chunks: Buffer[] = [];
      let total = 0;
      for await (const chunk of bodyStream) {
        const part = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
        total += part.length;
        if (total > maxBytes) {
          fail("too_large", "Cached object exceeds byte limit", 413);
        }
        chunks.push(part);
      }
      return {
        body: Buffer.concat(chunks, total),
        contentType: response.ContentType,
        etag: response.ETag,
        lastModified: response.LastModified,
        metadata: response.Metadata ? { ...response.Metadata } : undefined,
      };
    } finally {
      signal.removeEventListener("abort", onAbort);
      bodyStream.destroy?.();
    }
  }

  async head(key: string): Promise<RemoteCacheHead | null> {
    try {
      const response = await this.client.send(
        new HeadObjectCommand({ Bucket: this.bucket, Key: key }),
        { abortSignal: AbortSignal.timeout(this.timeoutMs) },
      );
      return {
        size: response.ContentLength ?? 0,
        contentType: response.ContentType,
        etag: response.ETag,
        lastModified: response.LastModified,
        metadata: response.Metadata ? { ...response.Metadata } : undefined,
      };
    } catch (error) {
      if (isNotFoundError(error)) return null;
      throw error;
    }
  }

  async put(
    key: string,
    body: Buffer,
    options?: { contentType?: string; metadata?: Record<string, string> },
  ): Promise<{ etag?: string }> {
    const response = await this.client.send(
      new PutObjectCommand({
        Bucket: this.bucket,
        Key: key,
        Body: body,
        ContentLength: body.length,
        ContentType: options?.contentType ?? "application/octet-stream",
        Metadata: options?.metadata,
        ServerSideEncryption: "AES256",
      }),
      { abortSignal: AbortSignal.timeout(this.timeoutMs) },
    );
    return { etag: response.ETag };
  }

  async delete(key: string): Promise<void> {
    try {
      await this.client.send(
        new DeleteObjectCommand({ Bucket: this.bucket, Key: key }),
        { abortSignal: AbortSignal.timeout(this.timeoutMs) },
      );
    } catch (error) {
      if (isNotFoundError(error)) return;
      throw error;
    }
  }
}

interface MemoryEntry {
  body: Buffer;
  contentType?: string;
  etag: string;
  lastModified: Date;
  metadata?: Record<string, string>;
}

export class MemoryRemoteCacheStorage implements RemoteCacheStorage {
  private readonly entries = new Map<string, MemoryEntry>();
  private readonly maxBytes: number | null;
  private totalBytes = 0;

  constructor(options: { maxBytes?: number } = {}) {
    if (options.maxBytes !== undefined) {
      if (!Number.isSafeInteger(options.maxBytes) || options.maxBytes < 1) {
        fail("invalid_storage", "Invalid memory cache byte limit", 500);
      }
      this.maxBytes = options.maxBytes;
    } else {
      this.maxBytes = null;
    }
  }

  keys(): string[] {
    return [...this.entries.keys()];
  }

  async get(key: string, maxBytes: number): Promise<RemoteCacheObject | null> {
    const entry = this.entries.get(key);
    if (!entry) return null;
    if (entry.body.length > maxBytes) {
      fail("too_large", "Cached object exceeds byte limit", 413);
    }
    this.entries.delete(key);
    this.entries.set(key, entry);
    return {
      body: Buffer.from(entry.body),
      contentType: entry.contentType,
      etag: entry.etag,
      lastModified: entry.lastModified,
      metadata: entry.metadata ? { ...entry.metadata } : undefined,
    };
  }

  async head(key: string): Promise<RemoteCacheHead | null> {
    const entry = this.entries.get(key);
    if (!entry) return null;
    return {
      size: entry.body.length,
      contentType: entry.contentType,
      etag: entry.etag,
      lastModified: entry.lastModified,
      metadata: entry.metadata ? { ...entry.metadata } : undefined,
    };
  }

  async put(
    key: string,
    body: Buffer,
    options?: { contentType?: string; metadata?: Record<string, string> },
  ): Promise<{ etag?: string }> {
    if (this.maxBytes !== null && body.length > this.maxBytes) {
      fail("too_large", "Object exceeds memory cache capacity", 413);
    }
    const existing = this.entries.get(key);
    if (existing) {
      this.totalBytes -= existing.body.length;
      this.entries.delete(key);
    }
    if (this.maxBytes !== null) {
      while (this.totalBytes + body.length > this.maxBytes && this.entries.size > 0) {
        const oldestKey = this.entries.keys().next().value!;
        const oldest = this.entries.get(oldestKey)!;
        this.totalBytes -= oldest.body.length;
        this.entries.delete(oldestKey);
      }
    }
    const copy = Buffer.from(body);
    const etag = `"${sha256Hex(copy)}"`;
    this.entries.set(key, {
      body: copy,
      contentType: options?.contentType ?? "application/octet-stream",
      etag,
      lastModified: new Date(),
      metadata: options?.metadata ? { ...options.metadata } : undefined,
    });
    this.totalBytes += copy.length;
    return { etag };
  }

  async delete(key: string): Promise<void> {
    const existing = this.entries.get(key);
    if (!existing) return;
    this.totalBytes -= existing.body.length;
    this.entries.delete(key);
  }
}

async function readBody(req: IncomingMessage, maxBytes: number, deadlineMs: number): Promise<Buffer> {
  const lengthHeader = req.headers["content-length"];
  if (lengthHeader !== undefined) {
    if (typeof lengthHeader !== "string" || !/^\d+$/.test(lengthHeader)) {
      fail("invalid_body", "Invalid Content-Length header", 400);
    }
    if (Number(lengthHeader) > maxBytes) {
      fail("too_large", "Cache request body exceeds byte limit", 413);
    }
  }
  return new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    let total = 0;
    const cleanup = () => {
      clearTimeout(timer);
      req.off("data", onData);
      req.off("end", onEnd);
      req.off("error", onError);
      req.off("aborted", onAborted);
    };
    const failed = (error: Error) => {
      cleanup();
      req.pause();
      reject(error);
    };
    const onData = (chunk: Buffer) => {
      total += chunk.length;
      if (total > maxBytes) {
        failed(new RemoteCacheError("too_large", "Cache request body exceeds byte limit", 413));
      } else {
        chunks.push(chunk);
      }
    };
    const onEnd = () => {
      cleanup();
      resolve(Buffer.concat(chunks, total));
    };
    const onError = () => failed(new RemoteCacheError("invalid_body", "Cache request body was interrupted", 400));
    const onAborted = () => failed(new RemoteCacheError("invalid_body", "Cache request body was interrupted", 400));
    const timer = setTimeout(
      () => failed(new RemoteCacheError("body_timeout", "Cache request body exceeded deadline", 408)),
      deadlineMs,
    );
    req.on("data", onData);
    req.once("end", onEnd);
    req.once("error", onError);
    req.once("aborted", onAborted);
  });
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = Buffer.from(JSON.stringify(body), "utf8");
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": String(payload.length),
    "Cache-Control": "no-store",
  });
  res.end(payload);
}

function rawPathname(req: IncomingMessage, url: URL): string {
  const raw = req.url;
  if (!raw) return url.pathname;
  let pathPart = raw;
  if (pathPart.startsWith("http://") || pathPart.startsWith("https://")) {
    const slash = pathPart.indexOf("/", pathPart.indexOf("://") + 3);
    pathPart = slash === -1 ? "/" : pathPart.slice(slash);
  }
  const queryIndex = pathPart.search(/[?#]/);
  return queryIndex === -1 ? pathPart : pathPart.slice(0, queryIndex);
}

function assertNoTraversalInRawPath(rawPath: string): void {
  if (/[\x00-\x1f\x7f\\]/.test(rawPath)) {
    fail("invalid_key", "Cache path contains forbidden characters", 400);
  }
  for (const rawSegment of rawPath.split("/")) {
    if (!rawSegment) continue;
    let decoded: string;
    try {
      decoded = decodeURIComponent(rawSegment);
    } catch {
      fail("invalid_key", "Cache path contains invalid percent-encoding", 400);
    }
    if (
      decoded === "." ||
      decoded === ".." ||
      decoded.includes("/") ||
      decoded.includes("\\") ||
      /[\x00-\x1f\x7f]/.test(decoded)
    ) {
      fail("invalid_key", "Cache path traversal or unsafe segment is not allowed", 400);
    }
  }
}

function normalizeWebDavKey(rawRel: string): { key: string; isCollection: boolean } {
  if (rawRel.length > MAX_KEY_LENGTH) {
    fail("invalid_key", "Cache key exceeds maximum length", 400);
  }
  if (/[\x00-\x1f\x7f\\]/.test(rawRel)) {
    fail("invalid_key", "Cache key contains forbidden characters", 400);
  }
  const isCollection = rawRel === "" || rawRel === "/" || rawRel.endsWith("/");
  const segments: string[] = [];
  for (const rawSegment of rawRel.split("/")) {
    if (!rawSegment) continue;
    let decoded: string;
    try {
      decoded = decodeURIComponent(rawSegment);
    } catch {
      fail("invalid_key", "Cache key contains invalid percent-encoding", 400);
    }
    if (
      decoded === "." ||
      decoded === ".." ||
      decoded.includes("/") ||
      decoded.includes("\\") ||
      /[\x00-\x1f\x7f]/.test(decoded)
    ) {
      fail("invalid_key", "Cache key contains unsafe path segment", 400);
    }
    segments.push(decoded);
  }
  const normalized = segments.join("/");
  if (normalized.length > MAX_KEY_LENGTH) {
    fail("invalid_key", "Cache key exceeds maximum length", 400);
  }
  return { key: normalized, isCollection };
}

function isSccacheProbeKey(key: string): boolean {
  return key === ".sccache_check" || key.endsWith("/.sccache_check");
}

function validateTurboHash(rawHash: string): string {
  let decoded: string;
  try {
    decoded = decodeURIComponent(rawHash);
  } catch {
    fail("invalid_hash", "Artifact hash has invalid percent-encoding", 400);
  }
  if (
    !TURBO_HASH_RE.test(decoded) ||
    decoded.includes("..") ||
    /[\x00-\x1f\x7f]/.test(decoded)
  ) {
    fail("invalid_hash", "Invalid Turborepo artifact hash", 400);
  }
  return decoded;
}

function parseArtifactTag(req: IncomingMessage): string | undefined {
  const raw = req.headers["x-artifact-tag"];
  if (raw === undefined) return undefined;
  if (typeof raw !== "string" || Buffer.byteLength(raw, "utf8") > MAX_ARTIFACT_TAG_BYTES || /[\x00-\x1f\x7f]/.test(raw)) {
    fail("invalid_tag", "Invalid x-artifact-tag header", 400);
  }
  return raw;
}

function buildCollectionPropfindXml(href: string, lastModified: Date): string {
  const normalizedHref = href.endsWith("/") ? href : `${href}/`;
  return [
    '<?xml version="1.0" encoding="utf-8"?>',
    '<D:multistatus xmlns:D="DAV:">',
    "  <D:response>",
    `    <D:href>${xmlEscape(normalizedHref)}</D:href>`,
    "    <D:propstat>",
    "      <D:prop>",
    "        <D:resourcetype><D:collection/></D:resourcetype>",
    `        <D:getlastmodified>${lastModified.toUTCString()}</D:getlastmodified>`,
    "      </D:prop>",
    "      <D:status>HTTP/1.1 200 OK</D:status>",
    "    </D:propstat>",
    "  </D:response>",
    "</D:multistatus>",
  ].join("\n");
}

function buildFilePropfindXml(href: string, head: RemoteCacheHead, fallbackDate: Date): string {
  const lastModified = (head.lastModified ?? fallbackDate).toUTCString();
  const etagTag = head.etag ? `<D:getetag>${xmlEscape(head.etag)}</D:getetag>` : "";
  return [
    '<?xml version="1.0" encoding="utf-8"?>',
    '<D:multistatus xmlns:D="DAV:">',
    "  <D:response>",
    `    <D:href>${xmlEscape(href)}</D:href>`,
    "    <D:propstat>",
    "      <D:prop>",
    `        <D:getcontentlength>${head.size}</D:getcontentlength>`,
    `        <D:getlastmodified>${lastModified}</D:getlastmodified>`,
    ...(etagTag ? [`        ${etagTag}`] : []),
    "        <D:resourcetype/>",
    "      </D:prop>",
    "      <D:status>HTTP/1.1 200 OK</D:status>",
    "    </D:propstat>",
    "  </D:response>",
    "</D:multistatus>",
  ].join("\n");
}

interface AccountStatsState {
  sccache: RemoteCacheToolStats;
  turbo: RemoteCacheToolStats;
  buck2: RemoteCacheToolStats;
  recent: RemoteCacheEvent[];
}

export function createRemoteBuildCacheHandler(options: {
  storage: RemoteCacheStorage;
  bucket?: string | null;
  authenticate: (req: IncomingMessage) => string | Promise<string>;
  maxObjectBytes?: number;
  now?: () => number;
}): RemoteBuildCacheHandler {
  const maxObjectBytes = options.maxObjectBytes ?? DEFAULT_MAX_OBJECT_BYTES;
  if (!Number.isSafeInteger(maxObjectBytes) || maxObjectBytes < 1) {
    fail("invalid_limits", "Invalid maxObjectBytes limit", 500);
  }
  const bucket = options.bucket?.trim() || null;
  const now = options.now ?? Date.now;

  let inflightTotal = 0;
  const inflightAccounts = new Map<string, number>();

  const globalTools = { sccache: emptyToolStats(), turbo: emptyToolStats(), buck2: emptyToolStats() };
  const globalRecent: RemoteCacheEvent[] = [];
  const accountStats = new Map<string, AccountStatsState>();

  function getOrCreateAccountState(accountId: string): AccountStatsState {
    let state = accountStats.get(accountId);
    if (!state) {
      state = {
        sccache: emptyToolStats(),
        turbo: emptyToolStats(),
        buck2: emptyToolStats(),
        recent: [],
      };
      accountStats.set(accountId, state);
    }
    return state;
  }

  function recordEvent(
    accountId: string | undefined,
    tool: RemoteCacheTool,
    op: "hit" | "miss" | "put" | "error",
    key: string,
    bytes: number,
    startedAt: number,
  ): void {
    const at = now();
    const ms = Math.max(0, at - startedAt);
    const event: RemoteCacheEvent = { at, tool, op, key, bytes, ms };
    applyEventToStats(globalTools[tool], op, bytes, at);
    pushBounded(globalRecent, event, GLOBAL_RECENT_LIMIT);

    if (accountId) {
      const acct = getOrCreateAccountState(accountId);
      applyEventToStats(acct[tool], op, bytes, at);
      pushBounded(acct.recent, event, ACCOUNT_RECENT_LIMIT);
    }
  }

  function applyEventToStats(
    stats: RemoteCacheToolStats,
    op: "hit" | "miss" | "put" | "error",
    bytes: number,
    at: number,
  ): void {
    if (op === "hit") {
      stats.hits++;
      stats.bytesRead += bytes;
    } else if (op === "miss") {
      stats.misses++;
    } else if (op === "put") {
      stats.writes++;
      stats.bytesWritten += bytes;
    } else {
      stats.errors++;
    }
    stats.lastActivityAt = at;
  }

  const handler = async (req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> => {
    const startedAt = now();
    let accountId: string | undefined;
    let admitted = false;
    let currentTool: RemoteCacheTool | undefined;
    let currentKey = "";
    let skipStats = false;

    try {
      accountId = await options.authenticate(req);
      if (
        typeof accountId !== "string" ||
        !accountId.trim() ||
        accountId.length > MAX_KEY_LENGTH ||
        /[\x00-\x1f\x7f]/.test(accountId)
      ) {
        fail("invalid_namespace", "Trusted account scope is required", 401);
      }

      if (url.pathname !== PREFIX && !url.pathname.startsWith(`${PREFIX}/`)) {
        fail("not_found", "Unknown cache endpoint", 404);
      }
      const subPath = url.pathname.slice(PREFIX.length);
      if (subPath === "/sccache" || subPath.startsWith("/sccache/")) {
        currentTool = "sccache";
      } else if (subPath === "/turbo" || subPath.startsWith("/turbo/")) {
        currentTool = "turbo";
      } else if (subPath === "/buck2" || subPath.startsWith("/buck2/")) {
        currentTool = "buck2";
      }

      if (
        inflightTotal >= MAX_INFLIGHT_TOTAL ||
        (inflightAccounts.get(accountId) ?? 0) >= MAX_INFLIGHT_PER_ACCOUNT
      ) {
        fail("busy", "Remote build cache transfer capacity is busy", 429);
      }
      inflightTotal++;
      inflightAccounts.set(accountId, (inflightAccounts.get(accountId) ?? 0) + 1);
      admitted = true;

      const rawPath = rawPathname(req, url);
      assertNoTraversalInRawPath(rawPath);

      const accountPrefix = `cache/v1/accounts/${sha256Hex(accountId)}`;
      const method = (req.method ?? "GET").toUpperCase();

      if (currentTool === "buck2") {
        // bazel-remote's uncompressed HTTP backend uses the action or blob SHA-256.
        const match = /^\/buck2\/(ac|cas)\/([a-f0-9]{64})$/.exec(subPath);
        if (!match) fail("invalid_key", "Expected a Buck2 ac or cas SHA-256 cache key");
        const [, kind, digest] = match;
        currentKey = `${kind}/${digest}`;
        const storageKey = `${accountPrefix}/buck2/${currentKey}`;

        if (method === "HEAD") {
          const head = await options.storage.head(storageKey);
          res.writeHead(head ? 200 : 404, { "Content-Length": String(head?.size ?? 0) });
          res.end();
          return;
        }
        if (method === "GET") {
          const object = await options.storage.get(storageKey, maxObjectBytes);
          if (!object) {
            recordEvent(accountId, "buck2", "miss", currentKey, 0, startedAt);
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          if (kind === "cas" && sha256Hex(object.body) !== digest) {
            fail("corrupt_object", "Stored Buck2 CAS object failed SHA-256 verification", 503);
          }
          recordEvent(accountId, "buck2", "hit", currentKey, object.body.length, startedAt);
          res.writeHead(200, {
            "Content-Type": "application/octet-stream",
            "Content-Length": String(object.body.length),
            "Cache-Control": kind === "cas" ? "private, immutable" : "private, no-cache",
          });
          res.end(object.body);
          return;
        }
        if (method === "PUT") {
          const body = await readBody(req, maxObjectBytes, UPLOAD_TIMEOUT_MS);
          if (kind === "cas" && sha256Hex(body) !== digest) {
            fail("digest_mismatch", "Buck2 CAS upload failed SHA-256 verification");
          }
          await options.storage.put(storageKey, body, { contentType: "application/octet-stream" });
          recordEvent(accountId, "buck2", "put", currentKey, body.length, startedAt);
          res.writeHead(200, { "Content-Length": "0" });
          res.end();
          return;
        }
        res.setHeader("Allow", "HEAD, GET, PUT");
        fail("method_not_allowed", `Unsupported Buck2 cache method ${method}`, 405);
      }

      if (currentTool === "sccache") {
        const rawRel = subPath.slice("/sccache".length);
        const { key: normalizedKey, isCollection } = normalizeWebDavKey(rawRel);
        currentKey = normalizedKey;
        skipStats = isSccacheProbeKey(normalizedKey);
        const storageKey = `${accountPrefix}/sccache/${normalizedKey}`;

        if (method === "OPTIONS") {
          res.writeHead(200, {
            DAV: "1, 2",
            Allow: "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL",
            "Content-Length": "0",
          });
          res.end();
          return;
        }

        if (method === "MKCOL") {
          await readBody(req, MAX_MKCOL_BODY_BYTES, SMALL_BODY_TIMEOUT_MS);
          res.writeHead(201, { "Content-Length": "0" });
          res.end();
          return;
        }

        if (method === "PROPFIND") {
          await readBody(req, MAX_PROPFIND_BODY_BYTES, SMALL_BODY_TIMEOUT_MS);
          if (isCollection || normalizedKey === "") {
            const xml = buildCollectionPropfindXml(url.pathname, new Date(now()));
            const buf = Buffer.from(xml, "utf8");
            res.writeHead(207, {
              "Content-Type": "application/xml; charset=utf-8",
              "Content-Length": String(buf.length),
            });
            res.end(buf);
            return;
          }
          const head = await options.storage.head(storageKey);
          if (!head) {
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          const xml = buildFilePropfindXml(url.pathname, head, new Date(now()));
          const buf = Buffer.from(xml, "utf8");
          res.writeHead(207, {
            "Content-Type": "application/xml; charset=utf-8",
            "Content-Length": String(buf.length),
          });
          res.end(buf);
          return;
        }

        if (method === "HEAD") {
          if (isCollection || normalizedKey === "") {
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          const head = await options.storage.head(storageKey);
          if (!head) {
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          const headers: Record<string, string> = {
            "Content-Type": head.contentType ?? "application/octet-stream",
            "Content-Length": String(head.size),
          };
          if (head.etag) headers.ETag = head.etag;
          if (head.lastModified) headers["Last-Modified"] = head.lastModified.toUTCString();
          res.writeHead(200, headers);
          res.end();
          return;
        }

        if (method === "GET") {
          if (isCollection || normalizedKey === "") {
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          const object = await options.storage.get(storageKey, maxObjectBytes);
          if (!object) {
            if (!skipStats) {
              recordEvent(accountId, "sccache", "miss", normalizedKey, 0, startedAt);
            }
            res.writeHead(404, { "Content-Length": "0" });
            res.end();
            return;
          }
          if (!skipStats) {
            recordEvent(accountId, "sccache", "hit", normalizedKey, object.body.length, startedAt);
          }
          const headers: Record<string, string> = {
            "Content-Type": "application/octet-stream",
            "Content-Length": String(object.body.length),
            "Cache-Control": "private, immutable",
          };
          if (object.etag) headers.ETag = object.etag;
          if (object.lastModified) headers["Last-Modified"] = object.lastModified.toUTCString();
          res.writeHead(200, headers);
          res.end(object.body);
          return;
        }

        if (method === "PUT") {
          if (isCollection || normalizedKey === "") {
            fail("invalid_key", "Cannot write to a WebDAV collection path", 400);
          }
          const body = await readBody(req, maxObjectBytes, UPLOAD_TIMEOUT_MS);
          const stored = await options.storage.put(storageKey, body, {
            contentType: "application/octet-stream",
          });
          if (!skipStats) {
            recordEvent(accountId, "sccache", "put", normalizedKey, body.length, startedAt);
          }
          const headers: Record<string, string> = { "Content-Length": "0" };
          if (stored.etag) headers.ETag = stored.etag;
          res.writeHead(201, headers);
          res.end();
          return;
        }

        if (method === "DELETE") {
          if (isCollection || normalizedKey === "") {
            fail("invalid_key", "Cannot delete a WebDAV collection path", 400);
          }
          await options.storage.delete(storageKey);
          res.writeHead(204);
          res.end();
          return;
        }

        fail("method_not_allowed", `Unsupported WebDAV method ${method}`, 405);
      }

      if (currentTool === "turbo") {
        const turboPath = subPath.slice("/turbo".length);
        if (turboPath === "/v8/artifacts/status") {
          if (method !== "GET") {
            fail("method_not_allowed", "Status endpoint requires GET", 405);
          }
          sendJson(res, 200, { status: "enabled" });
          return;
        }

        if (turboPath === "/v8/artifacts/events") {
          if (method !== "POST") {
            fail("method_not_allowed", "Events endpoint requires POST", 405);
          }
          await readBody(req, MAX_EVENTS_BODY_BYTES, SMALL_BODY_TIMEOUT_MS);
          sendJson(res, 200, {});
          return;
        }

        const artifactPrefix = "/v8/artifacts/";
        if (turboPath.startsWith(artifactPrefix)) {
          const rawHash = turboPath.slice(artifactPrefix.length);
          if (!rawHash || rawHash.includes("/")) {
            fail("invalid_hash", "Invalid Turborepo artifact path", 400);
          }
          const hash = validateTurboHash(rawHash);
          currentKey = hash;
          const storageKey = `${accountPrefix}/turbo/${hash}`;

          if (method === "HEAD") {
            const head = await options.storage.head(storageKey);
            if (!head) {
              res.writeHead(404, { "Content-Length": "0" });
              res.end();
              return;
            }
            const headers: Record<string, string> = {
              "Content-Type": head.contentType ?? "application/octet-stream",
              "Content-Length": String(head.size),
            };
            const tag = head.metadata?.["artifact-tag"];
            if (tag !== undefined) headers["x-artifact-tag"] = tag;
            res.writeHead(200, headers);
            res.end();
            return;
          }

          if (method === "GET") {
            const object = await options.storage.get(storageKey, maxObjectBytes);
            if (!object) {
              recordEvent(accountId, "turbo", "miss", hash, 0, startedAt);
              sendJson(res, 404, { code: "not_found", message: "Artifact not found" });
              return;
            }
            recordEvent(accountId, "turbo", "hit", hash, object.body.length, startedAt);
            const headers: Record<string, string> = {
              "Content-Type": "application/octet-stream",
              "Content-Length": String(object.body.length),
              "Cache-Control": "private, immutable",
            };
            const tag = object.metadata?.["artifact-tag"];
            if (tag !== undefined) headers["x-artifact-tag"] = tag;
            res.writeHead(200, headers);
            res.end(object.body);
            return;
          }

          if (method === "PUT") {
            const tag = parseArtifactTag(req);
            const body = await readBody(req, maxObjectBytes, UPLOAD_TIMEOUT_MS);
            await options.storage.put(storageKey, body, {
              contentType: "application/octet-stream",
              metadata: tag !== undefined ? { "artifact-tag": tag } : undefined,
            });
            recordEvent(accountId, "turbo", "put", hash, body.length, startedAt);
            sendJson(res, 200, { urls: [url.pathname] });
            return;
          }

          fail("method_not_allowed", `Unsupported Turborepo artifact method ${method}`, 405);
        }

        fail("not_found", "Unknown Turborepo cache endpoint", 404);
      }

      fail("not_found", "Unknown cache endpoint", 404);
    } catch (failure) {
      const externalStatus = Number((failure as { status?: unknown })?.status);
      const status =
        failure instanceof RemoteCacheError
          ? failure.status
          : externalStatus === 401 || externalStatus === 403
            ? externalStatus
            : 503;

      if (currentTool && accountId && !skipStats && status !== 404) {
        recordEvent(accountId, currentTool, "error", currentKey, 0, startedAt);
      }

      if (!res.headersSent && !res.destroyed) {
        res.setHeader("Connection", "close");
        const code =
          failure instanceof RemoteCacheError
            ? failure.code
            : status === 401 || status === 403
              ? "unauthorized"
              : "cache_unavailable";
        const message =
          failure instanceof RemoteCacheError
            ? failure.message
            : status === 401 || status === 403
              ? "Unauthorized"
              : "Remote build cache is unavailable";
        sendJson(res, status, { error: { code, message } });
      }
      if (res.headersSent && !res.writableFinished) {
        res.destroy();
      }
      req.resume();
    } finally {
      if (admitted && accountId) {
        inflightTotal--;
        const remaining = (inflightAccounts.get(accountId) ?? 1) - 1;
        if (remaining > 0) inflightAccounts.set(accountId, remaining);
        else inflightAccounts.delete(accountId);
      }
    }
  };

  function statsForAccount(accountId: string): RemoteCacheAccountStats {
    const state = accountStats.get(accountId);
    if (!state) {
      return {
        enabled: true,
        bucket,
        sccache: emptyToolStats(),
        turbo: emptyToolStats(),
        buck2: emptyToolStats(),
        recent: [],
      };
    }
    return {
      enabled: true,
      bucket,
      sccache: cloneToolStats(state.sccache),
      turbo: cloneToolStats(state.turbo),
      buck2: cloneToolStats(state.buck2),
      recent: state.recent.map((event) => ({ ...event })),
    };
  }

  function stats() {
    return {
      enabled: true,
      bucket,
      accounts: accountStats.size,
      sccache: cloneToolStats(globalTools.sccache),
      turbo: cloneToolStats(globalTools.turbo),
      buck2: cloneToolStats(globalTools.buck2),
      recent: globalRecent.map((event) => ({ ...event })),
    };
  }

  return Object.assign(handler, { statsForAccount, stats });
}
