// Loopback remote-cache proxy started by home.mjs (modeled on burst-broker.mjs).
// Agents never see the home's workerToken; instead this proxy listens on
// 127.0.0.1 with a random per-boot bearer token (rdrc_...) and forwards
// sccache WebDAV and Turborepo remote cache requests to the control plane
// authenticated with workerToken and x-rudder-workspace-id.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";

const SCCACHE_METHODS = new Set(["OPTIONS", "GET", "HEAD", "PROPFIND", "MKCOL", "PUT", "DELETE"]);
const TURBO_METHODS = new Set(["GET", "HEAD", "PUT", "POST"]);
const BODY_METHODS = new Set(["PUT", "POST", "PROPFIND", "MKCOL", "DELETE"]);
const RESPONSE_HEADERS = [
  "content-type",
  "etag",
  "last-modified",
  "dav",
  "allow",
  "x-artifact-tag",
  "x-artifact-duration",
  "cache-control",
  "location",
];
const MAX_RECENT = 30;
const PERSIST_DEBOUNCE_MS = 2_000;

function emptyToolStats() {
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

function sanitizeToolStats(raw) {
  const base = emptyToolStats();
  if (!raw || typeof raw !== "object") return base;
  for (const key of ["hits", "misses", "writes", "errors", "bytesRead", "bytesWritten"]) {
    const val = Number(raw[key]);
    if (Number.isFinite(val) && val >= 0) base[key] = val;
  }
  const lastActivity = Number(raw.lastActivityAt);
  if (Number.isFinite(lastActivity) && lastActivity > 0) {
    base.lastActivityAt = lastActivity;
  }
  return base;
}

function sanitizeRecent(raw) {
  if (!Array.isArray(raw)) return [];
  const validOps = new Set(["hit", "miss", "put", "error"]);
  const validTools = new Set(["sccache", "turbo"]);
  const out = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    if (!validTools.has(entry.tool) || !validOps.has(entry.op)) continue;
    const at = Number(entry.at);
    out.push({
      at: Number.isFinite(at) && at > 0 ? at : Date.now(),
      tool: entry.tool,
      op: entry.op,
      key: typeof entry.key === "string" ? entry.key : "",
      bytes: Number.isFinite(Number(entry.bytes)) ? Math.max(0, Number(entry.bytes)) : 0,
      ms: Number.isFinite(Number(entry.ms)) ? Math.max(0, Number(entry.ms)) : 0,
    });
  }
  return out.slice(-MAX_RECENT);
}

function loadStats(statsFile, log) {
  if (!statsFile) {
    return { sccache: emptyToolStats(), turbo: emptyToolStats(), recent: [] };
  }
  try {
    const text = fs.readFileSync(statsFile, "utf8");
    const parsed = JSON.parse(text);
    return {
      sccache: sanitizeToolStats(parsed?.sccache),
      turbo: sanitizeToolStats(parsed?.turbo),
      recent: sanitizeRecent(parsed?.recent),
    };
  } catch (error) {
    if (error && error.code !== "ENOENT") {
      log(`cache-proxy: could not read stats from ${statsFile}: ${error.message}`);
    }
    return { sccache: emptyToolStats(), turbo: emptyToolStats(), recent: [] };
  }
}

function writeStatsAtomic(statsFile, stats, log) {
  if (!statsFile) return;
  try {
    const dir = path.dirname(statsFile);
    fs.mkdirSync(dir, { recursive: true });
    const tmp = `${statsFile}.${process.pid}.${Date.now()}.${crypto.randomBytes(4).toString("hex")}.tmp`;
    const payload = JSON.stringify({
      sccache: stats.sccache,
      turbo: stats.turbo,
      recent: stats.recent,
    }, null, 2);
    fs.writeFileSync(tmp, payload);
    fs.renameSync(tmp, statsFile);
  } catch (error) {
    log(`cache-proxy: could not persist stats to ${statsFile}: ${error.message}`);
  }
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) {
    chunks.push(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
  }
  return chunks.length ? Buffer.concat(chunks) : Buffer.alloc(0);
}

function isSccacheCheck(pathname) {
  return /(?:^|\/)\.sccache_check$/.test(pathname);
}

function isTurboProbe(pathname) {
  return /^\/turbo\/v\d+\/artifacts\/(?:status|events)$/.test(pathname);
}

export function startCacheProxy({
  cloudUrl,
  homeId,
  workerToken,
  statsFile,
  host = "127.0.0.1",
  port = 0,
  token,
  timeoutMs = 15_000,
  log = () => {},
  fetchImpl = fetch,
} = {}) {
  const cleanCloudUrl = typeof cloudUrl === "string" ? cloudUrl.trim().replace(/\/+$/, "") : "";
  const cleanHomeId = typeof homeId === "string" ? homeId.trim() : "";
  const cleanWorkerToken = typeof workerToken === "string" ? workerToken.trim() : "";
  if (!cleanCloudUrl) throw new Error("cache-proxy: cloudUrl is required");
  if (!cleanHomeId) throw new Error("cache-proxy: homeId is required");
  if (!cleanWorkerToken) throw new Error("cache-proxy: workerToken is required");

  let proxyToken;
  if (token === undefined) {
    proxyToken = `rdrc_${crypto.randomBytes(24).toString("hex")}`;
  } else {
    if (typeof token !== "string" || !token.trim()) {
      throw new Error("cache-proxy: token must be a non-empty string");
    }
    proxyToken = token.trim();
  }

  const expectedTokenHash = crypto.createHash("sha256").update(proxyToken).digest();
  const stats = loadStats(statsFile, log);
  let upstreamConnected = null;
  let proxyUrl = port > 0 ? `http://${host}:${port}` : null;
  let closed = false;
  let dirty = false;
  let persistTimer = null;

  function schedulePersist() {
    if (!statsFile) return;
    dirty = true;
    if (persistTimer) return;
    persistTimer = setTimeout(() => {
      persistTimer = null;
      if (!dirty) return;
      dirty = false;
      writeStatsAtomic(statsFile, stats, log);
    }, PERSIST_DEBOUNCE_MS);
    persistTimer.unref?.();
  }

  function flushStats() {
    if (persistTimer) {
      clearTimeout(persistTimer);
      persistTimer = null;
    }
    if (!statsFile || !dirty) return;
    dirty = false;
    writeStatsAtomic(statsFile, stats, log);
  }

  function authorized(req) {
    const header = req.headers.authorization;
    if (typeof header !== "string") return false;
    const match = /^Bearer\s+(\S+)$/i.exec(header.trim());
    if (!match) return false;
    const suppliedHash = crypto.createHash("sha256").update(match[1]).digest();
    return crypto.timingSafeEqual(suppliedHash, expectedTokenHash);
  }

  function recordEvent(tool, op, key, bytes, ms) {
    const at = Date.now();
    const bucket = stats[tool];
    if (op === "hit") {
      bucket.hits += 1;
      bucket.bytesRead += bytes;
    } else if (op === "miss") {
      bucket.misses += 1;
    } else if (op === "put") {
      bucket.writes += 1;
      bucket.bytesWritten += bytes;
    } else if (op === "error") {
      bucket.errors += 1;
    }
    bucket.lastActivityAt = at;
    stats.recent.push({ at, tool, op, key, bytes, ms });
    if (stats.recent.length > MAX_RECENT) {
      stats.recent.splice(0, stats.recent.length - MAX_RECENT);
    }
    schedulePersist();
  }

  function replyJson(res, status, body, extraHeaders = {}) {
    const text = JSON.stringify(body);
    res.writeHead(status, {
      "content-type": "application/json",
      "content-length": Buffer.byteLength(text),
      ...extraHeaders,
    });
    res.end(text);
  }

  const server = http.createServer(async (req, res) => {
    if (!authorized(req)) {
      replyJson(res, 401, { error: "unauthorized" });
      return;
    }

    const parsed = new URL(req.url || "/", "http://127.0.0.1");
    const { pathname, search } = parsed;
    const method = (req.method || "GET").toUpperCase();

    let tool = null;
    if (pathname === "/sccache" || pathname.startsWith("/sccache/")) {
      tool = "sccache";
      if (!SCCACHE_METHODS.has(method)) {
        replyJson(res, 405, { error: "method not allowed" }, { allow: [...SCCACHE_METHODS].join(", ") });
        return;
      }
    } else if (pathname.startsWith("/turbo/")) {
      tool = "turbo";
      if (!TURBO_METHODS.has(method)) {
        replyJson(res, 405, { error: "method not allowed" }, { allow: [...TURBO_METHODS].join(", ") });
        return;
      }
    } else {
      replyJson(res, 404, { error: "not found" });
      return;
    }

    const key = tool === "turbo"
      ? pathname.replace(/^\/turbo\/v\d+\/artifacts\//, "").replace(/^\/turbo\/?/, "")
      : pathname.replace(/^\/sccache\/?/, "");
    const skipCounts = tool === "sccache" ? isSccacheCheck(pathname) : isTurboProbe(pathname);
    const upstreamUrl = `${cleanCloudUrl}/api/rudder/cache${pathname}${search}`;
    const headers = {
      authorization: `Bearer ${cleanWorkerToken}`,
      "x-rudder-workspace-id": cleanHomeId,
    };
    if (req.headers["content-type"]) {
      headers["content-type"] = String(req.headers["content-type"]);
    }
    if (tool === "sccache" && req.headers.depth !== undefined) {
      headers.depth = String(req.headers.depth);
    }
    if (tool === "turbo") {
      if (req.headers["x-artifact-tag"] !== undefined) {
        headers["x-artifact-tag"] = String(req.headers["x-artifact-tag"]);
      }
      if (req.headers["x-artifact-duration"] !== undefined) {
        headers["x-artifact-duration"] = String(req.headers["x-artifact-duration"]);
      }
    }

    let bodyBuf;
    try {
      if (BODY_METHODS.has(method)) {
        bodyBuf = await readBody(req);
      }
    } catch (error) {
      log(`cache-proxy: request body read failed: ${error instanceof Error ? error.message : String(error)}`);
      replyJson(res, 400, { error: "failed to read request body" });
      return;
    }

    const startedAt = Date.now();
    try {
      const init = {
        method,
        headers,
        signal: AbortSignal.timeout(timeoutMs),
      };
      if (bodyBuf !== undefined && (method === "PUT" || method === "POST" || bodyBuf.length > 0)) {
        init.body = bodyBuf;
      }
      const upstream = await fetchImpl(upstreamUrl, init);
      const resBuf = method === "HEAD" ? Buffer.alloc(0) : Buffer.from(await upstream.arrayBuffer());
      const ms = Math.max(0, Date.now() - startedAt);
      const status = upstream.status;

      upstreamConnected = status < 500;

      if (status >= 500) {
        recordEvent(tool, "error", key, 0, ms);
      } else if (!skipCounts) {
        if (method === "GET") {
          if (status >= 200 && status < 300) {
            recordEvent(tool, "hit", key, resBuf.length, ms);
          } else if (status === 404) {
            recordEvent(tool, "miss", key, 0, ms);
          }
        } else if (method === "PUT" && status >= 200 && status < 300) {
          recordEvent(tool, "put", key, bodyBuf ? bodyBuf.length : 0, ms);
        }
      }

      const outHeaders = {};
      for (const name of RESPONSE_HEADERS) {
        const value = upstream.headers.get(name);
        if (value !== null) outHeaders[name] = value;
      }
      if (method === "HEAD") {
        const len = upstream.headers.get("content-length");
        if (len !== null) outHeaders["content-length"] = len;
      } else {
        outHeaders["content-length"] = String(resBuf.length);
      }
      res.writeHead(status, outHeaders);
      res.end(method === "HEAD" ? undefined : resBuf);
    } catch (error) {
      const ms = Math.max(0, Date.now() - startedAt);
      upstreamConnected = false;
      recordEvent(tool, "error", key, 0, ms);
      const message = error instanceof Error ? error.message : String(error);
      log(`cache-proxy: upstream ${method} ${pathname} failed: ${message}`);
      replyJson(res, 503, { error: `remote cache upstream unavailable: ${message}` });
    }
  });

  const ready = new Promise((resolve, reject) => {
    const onError = (error) => {
      log(`cache-proxy: listen failed: ${error.message}`);
      reject(error);
    };
    server.once("error", onError);
    server.listen(port, host, () => {
      server.off("error", onError);
      server.on("error", (error) => log(`cache-proxy: ${error.message}`));
      const addr = server.address();
      const boundPort = typeof addr === "object" && addr ? addr.port : port;
      proxyUrl = `http://${host}:${boundPort}`;
      resolve({ url: proxyUrl, token: proxyToken, port: boundPort });
    });
  });

  function snapshot() {
    return {
      proxyActive: !closed,
      proxyUrl,
      upstreamConnected,
      sccache: { ...stats.sccache },
      turbo: { ...stats.turbo },
      recent: stats.recent.map((entry) => ({ ...entry })),
    };
  }

  function close() {
    if (closed) return Promise.resolve();
    closed = true;
    flushStats();
    return new Promise((resolve) => {
      server.close(() => resolve());
      server.closeIdleConnections?.();
    });
  }

  return {
    server,
    ready,
    get url() {
      return proxyUrl;
    },
    token: proxyToken,
    snapshot,
    close,
  };
}
