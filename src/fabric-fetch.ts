/** Private persistent read-only fabric transport. Auth comes only from the
 * trusted engine's initial stdio frame. No login/profile/environment fallback,
 * uploads, local root publication, endpoint selection or credential rotation.
 */
import { AsyncLocalStorage } from "node:async_hooks";
import { lookup } from "node:dns/promises";
import { Agent as HttpAgent, request as httpRequest } from "node:http";
import { Agent as HttpsAgent, request as httpsRequest } from "node:https";
import { isIP, type LookupFunction } from "node:net";
import { Readable, type Writable } from "node:stream";
import { FabricRemoteClient, FabricRemoteError, validateFabricReference, type FabricObjectRef } from "./fabric-remote.js";

export const FABRIC_FETCH_FRAME_BYTES = 64 * 1024;
export const FabricFetchFrame = Object.freeze({ init: 1, object: 2, root: 3, cancel: 4, shutdown: 5, hello: 10, data: 11, objectDone: 12, rootDone: 13, error: 14, fatal: 15, shutdownAck: 16 });
type Frame = { type: number; id: bigint; body: Buffer };
type Scope = { provider_id: string; tenant_id: string };
type Init = { format_version: 1; nonce: string; scope: Scope; base_url: string; auth_headers: Record<string, string>; transport: "https" | "loopback_http_test"; max_jobs: number; max_inflight_bytes: number; http_timeout_ms: number };
class ProtocolError extends Error { constructor() { super("Invalid private fabric transport"); } }
const exact = (value: unknown, names: string[]): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).length !== names.length || names.some((name) => !Object.hasOwn(value, name))) throw new ProtocolError();
  return value as Record<string, unknown>;
};
const integer = (value: unknown, min: number, max: number): number => { if (!Number.isSafeInteger(value) || Number(value) < min || Number(value) > max) throw new ProtocolError(); return Number(value); };
const scope = (value: unknown): Scope => {
  const item = exact(value, ["provider_id", "tenant_id"]);
  if (![item.provider_id, item.tenant_id].every((part) => typeof part === "string" && /^[A-Za-z0-9._:-]{1,128}$/.test(part))) throw new ProtocolError();
  return { provider_id: item.provider_id as string, tenant_id: item.tenant_id as string };
};
function initialization(value: unknown): Init {
  const item = exact(value, ["format_version", "nonce", "scope", "base_url", "auth_headers", "transport", "max_jobs", "max_inflight_bytes", "http_timeout_ms"]);
  if (item.format_version !== 1 || typeof item.nonce !== "string" || !/^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(item.nonce) || typeof item.base_url !== "string" || item.base_url.length > 2048 || item.base_url.trim() !== item.base_url || !/^https?:\/\//.test(item.base_url) || /[?#\\\x00-\x20\x7f]/.test(item.base_url)) throw new ProtocolError();
  const authority = /^https?:\/\/([^/\\]*)/.exec(item.base_url)?.[1];
  const url = new URL(item.base_url);
  if (!authority || authority.includes("@") || url.username || url.password || url.search || url.hash) throw new ProtocolError();
  if (item.transport === "https") { if (url.protocol !== "https:") throw new ProtocolError(); }
  else if (item.transport === "loopback_http_test") { if (url.protocol !== "http:" || !["127.0.0.1", "[::1]"].includes(url.hostname)) throw new ProtocolError(); }
  else throw new ProtocolError();
  if (!item.auth_headers || typeof item.auth_headers !== "object" || Array.isArray(item.auth_headers)) throw new ProtocolError();
  const headers: Record<string, string> = {};
  for (const [key, value] of Object.entries(item.auth_headers)) {
    const lower = key.toLowerCase();
    if (!["authorization", "x-rudder-workspace-id"].includes(lower) || Object.hasOwn(headers, lower) || typeof value !== "string" || !value || value.length > 8192 || /[\r\n\0]/.test(value)) throw new ProtocolError();
    headers[lower] = value;
  }
  if (!headers.authorization) throw new ProtocolError();
  return { format_version: 1, nonce: item.nonce, scope: scope(item.scope), base_url: url.toString(), auth_headers: Object.freeze(headers), transport: item.transport, max_jobs: integer(item.max_jobs, 1, 8), max_inflight_bytes: integer(item.max_inflight_bytes, 1, 64 * 1024 * 1024), http_timeout_ms: integer(item.http_timeout_ms, 1, 120_000) };
}
export function encodeFabricFetchFrame(type: number, id: bigint, body: Uint8Array = Buffer.alloc(0)): Buffer {
  if (!Number.isInteger(type) || type < 1 || type > 255 || id < 0n || id > 0xffffffffffffffffn || body.byteLength > FABRIC_FETCH_FRAME_BYTES) throw new ProtocolError();
  const bytes = Buffer.allocUnsafe(16 + body.byteLength); bytes.fill(0, 0, 16); bytes[0] = type; bytes.writeBigUInt64BE(id, 4); bytes.writeUInt32BE(body.byteLength, 12); bytes.set(body, 16); return bytes;
}
async function* frames(input: Readable): AsyncGenerator<Frame> {
  let header = Buffer.alloc(16), headerAt = 0, body = Buffer.alloc(0), bodyAt = 0, type = 0, id = 0n;
  for await (const value of input) {
    const chunk = Buffer.isBuffer(value) ? value : Buffer.from(value as Uint8Array); let at = 0;
    while (at < chunk.length) {
      if (headerAt < 16) {
        const count = Math.min(16 - headerAt, chunk.length - at); chunk.copy(header, headerAt, at, at + count); at += count; headerAt += count; if (headerAt < 16) continue;
        if (header[1] || header[2] || header[3]) throw new ProtocolError(); type = header[0]; id = header.readBigUInt64BE(4); const length = header.readUInt32BE(12);
        if (length > FABRIC_FETCH_FRAME_BYTES) throw new ProtocolError(); body = Buffer.allocUnsafe(length); bodyAt = 0;
        if (length === 0) { yield { type, id, body }; headerAt = 0; continue; }
      }
      const count = Math.min(body.length - bodyAt, chunk.length - at); chunk.copy(body, bodyAt, at, at + count); at += count; bodyAt += count;
      if (bodyAt === body.length) { yield { type, id, body }; headerAt = 0; }
    }
  }
  if (headerAt !== 0) throw new ProtocolError();
}
type Operation = { controller: AbortController; cap: number; io: Set<Promise<void>> };
type Job = { id: bigint; controller: AbortController; task: Promise<void>; cost: number; cancelled: boolean };

/** Fixed-origin pool, with DNS resolved once before Hello. Request.close is
 * tracked separately from fetch/body settlement: aborting a Promise alone does
 * not prove that its underlying connection has released an admission slot.
 * TLS still uses the original hostname/SNI/certificate checks. No redirects.
 */
async function transport(init: Init, storage: AsyncLocalStorage<Operation>): Promise<{ fetch: typeof globalThis.fetch; close: () => void }> {
  const base = new URL(init.base_url), host = base.hostname.replace(/^\[|\]$/g, "");
  const addresses = isIP(host) ? [{ address: host, family: isIP(host) }] : await lookup(host, { all: true });
  if (!addresses.length || addresses.length > 64) throw new ProtocolError();
  const pinnedLookup: LookupFunction = (_hostname, options, callback) => {
    if (options.all) callback(null, addresses);
    else callback(null, addresses[0].address, addresses[0].family);
  };
  const settings = { keepAlive: true, maxSockets: init.max_jobs, maxTotalSockets: init.max_jobs, maxFreeSockets: init.max_jobs, lookup: pinnedLookup };
  const agent = base.protocol === "https:" ? new HttpsAgent(settings) : new HttpAgent(settings);
  const fetch: typeof globalThis.fetch = async (target, options) => {
    const operation = storage.getStore(), url = new URL(String(target));
    if (!operation || url.origin !== base.origin || options?.method !== "GET" || options.body !== undefined || url.username || url.password || url.hash) throw new ProtocolError();
    let completed!: () => void;
    operation.io.add(new Promise<void>((resolve) => { completed = resolve; }));
    return new Promise<Response>((resolve, reject) => {
      let request: ReturnType<typeof httpRequest>;
      try {
        const headers = Object.fromEntries(new Headers(options.headers).entries());
        headers["accept-encoding"] = "identity";
        request = (base.protocol === "https:" ? httpsRequest : httpRequest)(url, { method: "GET", headers, agent, maxHeaderSize: 16 * 1024 }, (incoming) => {
          try {
            const responseHeaders = new Headers();
            for (let at = 0; at < incoming.rawHeaders.length; at += 2) responseHeaders.append(incoming.rawHeaders[at], incoming.rawHeaders[at + 1]);
            const status = incoming.statusCode ?? 500;
            const body = [204, 205, 304].includes(status) ? null : Readable.toWeb(incoming) as ReadableStream<Uint8Array>;
            if (!body) incoming.resume();
            resolve(new Response(body, { status, headers: responseHeaders }));
          } catch { incoming.destroy(); request.destroy(); reject(new ProtocolError()); }
        });
      } catch { completed(); reject(new ProtocolError()); return; }
      const abort = () => request.destroy(new ProtocolError());
      request.once("error", () => reject(new ProtocolError()));
      request.once("upgrade", (_response, socket) => { socket.destroy(); request.destroy(); reject(new ProtocolError()); });
      request.once("close", () => { options.signal?.removeEventListener("abort", abort); completed(); });
      options.signal?.addEventListener("abort", abort, { once: true });
      if (options.signal?.aborted) abort();
      request.end();
    });
  };
  return { fetch, close: () => agent.destroy() };
}

/** Exported for stream/HTTP fixtures. Production calls this through the private
 * dispatcher. Error frames contain fixed codes, never config or provider text.
 */
export async function runFabricFetchHelper(options: { input?: Readable; output?: Writable; fetch?: typeof globalThis.fetch } = {}): Promise<void> {
  const input = options.input ?? process.stdin, output = options.output ?? process.stdout;
  const storage = new AsyncLocalStorage<Operation>();
  const jobs = new Map<bigint, Job>(); let admittedBytes = 0, lastId = 0n, init: Init | undefined, remote: FabricRemoteClient | undefined, closing: Promise<void> | undefined, connection: Awaited<ReturnType<typeof transport>> | undefined;
  let writeTail: Promise<void> = Promise.resolve();
  const write = (type: number, id: bigint, body: unknown = undefined, binary?: Uint8Array): Promise<void> => {
    const bytes = encodeFabricFetchFrame(type, id, binary ?? (body === undefined ? Buffer.alloc(0) : Buffer.from(JSON.stringify(body))));
    const work = writeTail.then(() => new Promise<void>((resolve, reject) => { output.write(bytes, (failure) => failure ? reject(new ProtocolError()) : resolve()); }));
    writeTail = work.catch(() => { }); return work;
  };
  const close = (reason: "protocol" | "auth_failed" | "closed" | "shutdown"): Promise<void> => {
    if (closing) return closing;
    // Set closing synchronously before destroying stdin or scheduling callbacks.
    let complete!: () => void; closing = new Promise<void>((resolve) => { complete = resolve; });
    for (const job of jobs.values()) job.controller.abort(); input.destroy();
    void (async () => {
      await Promise.allSettled([...jobs.values()].map((job) => job.task));
      connection?.close();
      try { await write(reason === "shutdown" ? FabricFetchFrame.shutdownAck : FabricFetchFrame.fatal, 0n, reason === "shutdown" ? undefined : { code: reason }); } catch { /* private output already closed */ }
      output.end(); complete();
    })();
    return closing;
  };
  output.on("error", () => { void close("closed"); });
  const trackedFetch: typeof globalThis.fetch = async (url, request) => {
    const operation = storage.getStore(); if (!operation) throw new ProtocolError();
    let finish!: () => void; const settled = new Promise<void>((resolve) => { finish = resolve; }); operation.io.add(settled);
    const signal = AbortSignal.any([operation.controller.signal, ...(request?.signal ? [request.signal] : [])]);
    try {
      const response = await (options.fetch ?? connection!.fetch)(url, { ...request, signal });
      const length = response.headers.get("content-length");
      if (response.ok && length !== null && (!/^\d+$/.test(length) || Number(length) > operation.cap)) { await response.body?.cancel().catch(() => { }); finish(); throw new ProtocolError(); }
      if (!response.body) { finish(); return response; }
      const reader = response.body.getReader(); let consumed = 0, ended = false;
      const end = async (reason?: unknown) => { if (!ended) { ended = true; try { await reader.cancel(reason); } catch { /* cancellation still settled */ } finally { finish(); } } };
      const stream = new ReadableStream<Uint8Array>({
        async pull(controller) {
          try { const part = await reader.read(); if (part.done) { await end(); controller.close(); return; } consumed += part.value.byteLength; if (consumed > operation.cap) throw new ProtocolError(); controller.enqueue(part.value); }
          catch (failure) { await end(failure); controller.error(new ProtocolError()); }
        },
        async cancel(reason) { await end(reason); },
      }, { highWaterMark: 0 });
      return new Response(stream, { status: response.status, statusText: response.statusText, headers: response.headers });
    } catch (failure) { finish(); throw failure; }
  };
  const settled = async (operation: Operation) => { await Promise.allSettled([...operation.io]); };
  try {
    for await (const frame of frames(input)) {
      if (closing) break;
      if (!init) {
        if (frame.type !== FabricFetchFrame.init || frame.id !== 0n || !frame.body.length) throw new ProtocolError();
        init = initialization(JSON.parse(frame.body.toString("utf8")));
        if (!options.fetch) connection = await transport(init, storage);
        remote = new FabricRemoteClient({ baseUrl: init.base_url, authHeaders: () => init!.auth_headers, fetch: trackedFetch, concurrency: init.max_jobs, requestTimeoutMs: init.http_timeout_ms });
        await write(FabricFetchFrame.hello, 0n, { format_version: 1, nonce: init.nonce, scope: init.scope, max_jobs: init.max_jobs, max_inflight_bytes: init.max_inflight_bytes }); continue;
      }
      if (frame.type === FabricFetchFrame.shutdown) { if (frame.id || frame.body.length) throw new ProtocolError(); await close("shutdown"); break; }
      if (frame.type === FabricFetchFrame.cancel) {
        if (!frame.id || frame.id > lastId || frame.body.length) throw new ProtocolError(); const job = jobs.get(frame.id); if (job) { job.cancelled = true; job.controller.abort(); } continue;
      }
      if ((frame.type !== FabricFetchFrame.object && frame.type !== FabricFetchFrame.root) || frame.id <= lastId || !frame.id || frame.body.length > 4096) throw new ProtocolError();
      lastId = frame.id;
      const request = JSON.parse(frame.body.toString("utf8")); let reference: FabricObjectRef | undefined, rootId: string | undefined, timeout: number, cost: number;
      if (frame.type === FabricFetchFrame.object) {
        const body = exact(request, ["reference", "timeout_ms"]); reference = validateFabricReference(body.reference);
        if (reference.size_bytes > (reference.kind === "blob" ? 4 * 1024 * 1024 : 64 * 1024)) throw new ProtocolError(); timeout = integer(body.timeout_ms, 1, init.http_timeout_ms); cost = Math.max(1, reference.size_bytes);
      } else {
        const body = exact(request, ["root_id", "timeout_ms"]); if (typeof body.root_id !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(body.root_id)) throw new ProtocolError(); rootId = body.root_id; timeout = integer(body.timeout_ms, 1, init.http_timeout_ms); cost = 64 * 1024;
      }
      if (jobs.size >= init.max_jobs || admittedBytes + cost > init.max_inflight_bytes) { await write(FabricFetchFrame.error, frame.id, { code: "busy" }); continue; }
      const controller = new AbortController(), operation: Operation = { controller, cap: reference?.size_bytes ?? 64 * 1024, io: new Set() };
      admittedBytes += cost;
      const job: Job = { id: frame.id, controller, cost, task: Promise.resolve(), cancelled: false };
      jobs.set(frame.id, job);
      job.task = (async () => {
        let timedOut = false; const timer = setTimeout(() => { timedOut = true; controller.abort(); }, timeout);
        let data: Buffer | undefined, receipt: unknown, failure: unknown;
        try { await storage.run(operation, async () => { if (reference) data = await remote!.getObject(reference); else receipt = await remote!.getRoot(rootId!); }); }
        catch (error) { failure = error; }
        finally { controller.abort(); await settled(operation); clearTimeout(timer); }
        try {
          if (failure instanceof FabricRemoteError && [401, 403].includes(failure.status ?? 0)) { void close("auth_failed"); return; }
          if (closing) return;
          if (timedOut) { await write(FabricFetchFrame.error, frame.id, { code: "timeout" }); return; }
          if (job.cancelled) { await write(FabricFetchFrame.error, frame.id, { code: "cancelled" }); return; }
          if (failure) { await write(FabricFetchFrame.error, frame.id, { code: "failed" }); return; }
          if (reference && data) {
            for (let at = 0; at < data.length; at += FABRIC_FETCH_FRAME_BYTES) { if (closing) return; if (job.cancelled) { await write(FabricFetchFrame.error, frame.id, { code: "cancelled" }); return; } await write(FabricFetchFrame.data, frame.id, undefined, data.subarray(at, at + FABRIC_FETCH_FRAME_BYTES)); }
            await write(FabricFetchFrame.objectDone, frame.id);
          } else await write(FabricFetchFrame.rootDone, frame.id, receipt);
        } finally { if (jobs.delete(frame.id)) admittedBytes -= cost; }
      })().catch(() => { if (jobs.delete(frame.id)) admittedBytes -= cost; void close("closed"); });
    }
    if (!closing) await close("closed");
  } catch { await close("protocol"); }
  if (closing) await closing;
}
