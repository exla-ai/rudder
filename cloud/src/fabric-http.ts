import type { IncomingMessage, ServerResponse } from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import {
  FabricCas, FabricCasError, validateFabricObjectRef,
  type FabricObjectStorage, type FabricVerificationCache,
} from "./fabric-cas.js";

const PREFIX = "/api/rudder/fabric";
const JSON_LIMIT = 128 * 1024;

function fail(code: string, message: string, status = 400): never {
  throw new FabricCasError(code, message, status);
}

function send(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
  res.end(JSON.stringify(body));
}

async function readBody(req: IncomingMessage, maxBytes: number, deadlineMs: number): Promise<Buffer> {
  const length = req.headers["content-length"];
  if (length !== undefined && (!/^\d+$/.test(length) || Number(length) > maxBytes)) fail("too_large", "Fabric request exceeds byte limit", 413);
  return new Promise<Buffer>((resolve, reject) => {
    const chunks: Buffer[] = [];
    let total = 0;
    const cleanup = () => {
      clearTimeout(timer);
      req.off("data", data);
      req.off("end", end);
      req.off("error", error);
      req.off("aborted", aborted);
    };
    const failed = (failure: Error) => { cleanup(); req.pause(); reject(failure); };
    const data = (chunk: Buffer) => {
      total += chunk.length;
      if (total > maxBytes) failed(new FabricCasError("too_large", "Fabric request exceeds byte limit", 413));
      else chunks.push(chunk);
    };
    const end = () => { cleanup(); resolve(Buffer.concat(chunks, total)); };
    const error = () => failed(new FabricCasError("invalid_body", "Fabric request body was interrupted"));
    const aborted = () => failed(new FabricCasError("invalid_body", "Fabric request body was interrupted"));
    const timer = setTimeout(() => failed(new FabricCasError("body_timeout", "Fabric body exceeded its deadline", 408)), deadlineMs);
    req.on("data", data);
    req.once("end", end);
    req.once("error", error);
    req.once("aborted", aborted);
  });
}

async function jsonBody(req: IncomingMessage, keys: string[]): Promise<Record<string, unknown>> {
  if (req.headers["content-type"]?.split(";")[0].trim().toLowerCase() !== "application/json") fail("invalid_body", "Fabric JSON requires application/json", 415);
  const bytes = await readBody(req, JSON_LIMIT, 5_000);
  let value: unknown;
  try { value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)); }
  catch { fail("invalid_body", "Fabric request requires valid UTF-8 JSON"); }
  if (!value || typeof value !== "object" || Array.isArray(value)) fail("invalid_body", "Expected a Fabric request object");
  const data = value as Record<string, unknown>;
  if (data.format_version !== 1 || Object.keys(data).length !== keys.length + 1 || keys.some((key) => !Object.hasOwn(data, key))) fail("invalid_body", "Unsupported or missing Fabric request field");
  return data;
}

/** Authentication supplies the namespace. A URL/body can never select it.
 * Keep large object operations bounded across every account on this process.
 * These limits are admission control, not fleet coordination or scheduling.
 */
export function createFabricHttpHandler(options: {
  storage: FabricObjectStorage;
  /** Enable only for an audited immutable prefix with no GC/external expiry. */
  verificationCache?: FabricVerificationCache;
  authenticate: (req: IncomingMessage) => string | Promise<string>;
}) {
  let heavy = 0;
  let total = 0;
  const accounts = new Map<string, number>();
  return async (req: IncomingMessage, res: ServerResponse, url: URL): Promise<void> => {
    let account: string | undefined;
    let admitted = false;
    let large = false;
    try {
      account = await options.authenticate(req);
      const route = url.pathname.slice(PREFIX.length);
      large = route === "/missing" || route.startsWith("/objects/") || (route.startsWith("/roots/") && req.method === "POST");
      if (total >= 12 || (accounts.get(account) ?? 0) >= 8 || (large && heavy >= 2)) fail("busy", "Fabric transfer capacity is busy", 429);
      total++;
      if (large) heavy++;
      accounts.set(account, (accounts.get(account) ?? 0) + 1);
      admitted = true;
      // Large blobs stream with backpressure; metadata reads remain bounded.
      // Two large operations cap private upload spool use at 512 MiB.
      const cas = new FabricCas({ storage: options.storage, accountId: account, verificationCache: options.verificationCache, limits: { maxConcurrency: 1 } });
      if (req.method === "POST" && route === "/staging") {
        const body = await jsonBody(req, ["ttl_ms"]);
        send(res, 201, await cas.beginStaging(body.ttl_ms as number));
        return;
      }
      const renewal = /^\/staging\/([a-f0-9-]+)\/renew$/.exec(route);
      if (req.method === "POST" && renewal) {
        const body = await jsonBody(req, ["ttl_ms"]);
        send(res, 200, await cas.renewStaging(renewal[1]!, body.ttl_ms as number));
        return;
      }
      const staging = /^\/staging\/([a-f0-9-]+)$/.exec(route);
      if (req.method === "DELETE" && staging) {
        await jsonBody(req, []);
        await cas.abandonStaging(staging[1]!);
        send(res, 200, { format_version: 1 });
        return;
      }
      if (req.method === "POST" && route === "/missing") {
        const body = await jsonBody(req, ["objects"]);
        if (!Array.isArray(body.objects)) fail("invalid_body", "objects must be an array");
        send(res, 200, { format_version: 1, missing: await cas.missing(body.objects) });
        return;
      }
      const objectPath = /^\/objects\/([a-z_]+)\/([a-f0-9]{64})\/(0|[1-9][0-9]{0,10})$/.exec(route);
      if (objectPath && ["GET", "PUT"].includes(req.method ?? "")) {
        const object = validateFabricObjectRef({ kind: objectPath[1], algorithm: "sha256", digest: objectPath[2], size_bytes: Number(objectPath[3]) });
        if (object.size_bytes > cas.limits.maxObjectBytes || (object.kind !== "blob" && object.size_bytes > cas.limits.maxMetadataBytes)) fail("too_large", "Fabric object exceeds byte limit", 413);
        if (req.method === "GET") {
          if (object.kind === "blob") {
            await cas.withBlob(object, async (body) => {
              res.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": object.size_bytes, "Cache-Control": "private, max-age=31536000, immutable", "ETag": `"sha256-${object.digest}"` });
              await pipeline(Readable.from(body, { objectMode: false }), res);
            });
          } else {
            const bytes = await cas.getObject(object);
            res.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": bytes.length, "Cache-Control": "private, max-age=31536000, immutable", "ETag": `"sha256-${object.digest}"` });
            res.end(bytes);
          }
        } else {
          const lease = req.headers["x-rudder-staging-lease"];
          if (typeof lease !== "string") fail("invalid_lease", "A staging lease is required");
          if (object.kind === "blob") {
            const length = req.headers["content-length"];
            if (length !== undefined && (!/^\d+$/.test(length) || Number(length) !== object.size_bytes)) fail("invalid_body", "Blob Content-Length must match the typed reference");
            send(res, 200, { format_version: 1, ...await cas.putBlobStream(lease, object, req) });
          } else {
            const bytes = await readBody(req, object.size_bytes, 30_000);
            send(res, 200, { format_version: 1, ...await cas.putObject(lease, object, bytes) });
          }
        }
        return;
      }
      const rootPath = /^\/roots\/([^/]+)$/.exec(route);
      if (rootPath && ["GET", "POST"].includes(req.method ?? "")) {
        let id: string;
        try { id = decodeURIComponent(rootPath[1]!); } catch { fail("invalid_root_id", "Invalid publication identifier"); }
        if (req.method === "GET") {
          const root = await cas.getRoot(id);
          if (!root) fail("not_found", "Fabric root is not published", 404);
          send(res, 200, root);
        } else {
          const body = await jsonBody(req, ["lease_id", "root"]);
          if (typeof body.lease_id !== "string") fail("invalid_lease", "A staging lease is required");
          send(res, 200, await cas.publishRoot(id, body.lease_id, body.root));
        }
        return;
      }
      fail("not_found", "Unknown Fabric endpoint", 404);
    } catch (failure) {
      const externalStatus = Number((failure as { status?: unknown })?.status);
      const status = failure instanceof FabricCasError ? failure.status : externalStatus === 401 || externalStatus === 403 ? externalStatus : 503;
      if (!res.headersSent && !res.destroyed) {
        res.setHeader("Connection", "close");
        send(res, status, { format_version: 1, error: { code: failure instanceof FabricCasError ? failure.code : status === 401 || status === 403 ? "unauthorized" : "storage_unavailable", message: failure instanceof FabricCasError ? failure.message : status === 401 || status === 403 ? "Unauthorized" : "Fabric content store is unavailable" } });
      }
      if (res.headersSent && !res.writableFinished) res.destroy();
      // Drain without accumulating a rejected request body. Connection:close
      // bounds slow/malformed upload lifetimes without discarding the response.
      req.resume();
    } finally {
      if (admitted && account) {
        if (large && !res.writableFinished && !res.destroyed && res.headersSent) {
          // Keep the slot while a slow receiver still owns a buffered blob.
          await new Promise<void>((resolve) => {
            const done = () => { clearTimeout(timer); res.off("finish", done); res.off("close", done); resolve(); };
            const timer = setTimeout(() => { res.destroy(); done(); }, 30_000);
            res.once("finish", done);
            res.once("close", done);
          });
        }
        total--;
        if (large) heavy--;
        const remaining = (accounts.get(account) ?? 1) - 1;
        if (remaining) accounts.set(account, remaining); else accounts.delete(account);
      }
    }
  };
}
