import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { PassThrough } from "node:stream";
import test from "node:test";

const { runFabricFetchHelper, encodeFabricFetchFrame: frame, FabricFetchFrame: F } = await import(process.env.RUDDER_FABRIC_FETCH_MODULE ?? new URL("../dist/fabric-fetch.js", import.meta.url).href);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const bounded = async (task, ms, message) => { let timer; try { return await Promise.race([task, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), ms); })]); } finally { clearTimeout(timer); } };
const reference = (bytes, kind = "blob") => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
const init = (base = "http://127.0.0.1:1/api/rudder/fabric") => ({ format_version: 1, nonce: randomUUID(), scope: { provider_id: "fixture-provider", tenant_id: "fixture-account" }, base_url: base, auth_headers: { Authorization: "Bearer private-fixture-token", "X-Rudder-Workspace-Id": "own-workspace" }, transport: "loopback_http_test", max_jobs: 2, max_inflight_bytes: 8 * 1024 * 1024, http_timeout_ms: 2000 });

function harness(config, fetch) {
  const input = new PassThrough(), output = new PassThrough(), received = [], waiters = new Set();
  let buffer = Buffer.alloc(0);
  output.on("data", (part) => {
    buffer = Buffer.concat([buffer, part]);
    while (buffer.length >= 16 && buffer.length >= 16 + buffer.readUInt32BE(12)) {
      assert.equal(buffer.subarray(1, 4).toString("hex"), "000000");
      const length = buffer.readUInt32BE(12); assert.ok(length <= 65536);
      received.push({ type: buffer[0], id: buffer.readBigUInt64BE(4), body: Buffer.from(buffer.subarray(16, 16 + length)) });
      buffer = buffer.subarray(16 + length);
    }
    for (const wake of waiters) wake();
  });
  const task = runFabricFetchHelper({ input, output, fetch });
  const send = (type, id = 0n, body) => input.write(frame(type, id, body === undefined ? undefined : Buffer.from(JSON.stringify(body))));
  const next = async (predicate, ms = 5000) => {
    const end = Date.now() + ms;
    while (true) {
      const at = received.findIndex(predicate);
      if (at >= 0) return received.splice(at, 1)[0];
      assert.ok(Date.now() < end, "private helper response deadline");
      await new Promise((resolve) => { const wake = () => { clearTimeout(timer); waiters.delete(wake); resolve(); }; const timer = setTimeout(wake, Math.min(25, end - Date.now())); waiters.add(wake); });
    }
  };
  const close = async () => { if (!input.destroyed) send(F.shutdown); await bounded(task, 5000, "helper shutdown deadline"); };
  if (config) send(F.init, 0n, config);
  return { input, output, send, next, close, task, received };
}
async function ready(config = init(), fetch = undefined) {
  const h = harness(config, fetch); const hello = JSON.parse((await h.next((f) => f.type === F.hello)).body);
  assert.deepEqual(hello, { format_version: 1, nonce: config.nonce, scope: config.scope, max_jobs: config.max_jobs, max_inflight_bytes: config.max_inflight_bytes });
  return h;
}
async function server(handler) {
  const instance = createServer(handler); await new Promise((resolve) => instance.listen(0, "127.0.0.1", resolve));
  return { base: `http://127.0.0.1:${instance.address().port}/api/rudder/fabric`, close: async () => { instance.closeAllConnections(); await new Promise((resolve) => instance.close(resolve)); } };
}

test("persistent helper multiplexes real authenticated HTTP and frames verified 4MiB bytes", async () => {
  const bytes = Buffer.alloc(4 * 1024 * 1024, 0xa7), small = Buffer.from("second concurrent request 🦀"), refs = [reference(bytes), reference(small)];
  const requests = [], pending = [];
  const http = await server((req, res) => {
    requests.push({ path: req.url, auth: req.headers.authorization, workspace: req.headers["x-rudder-workspace-id"], method: req.method });
    pending.push(() => { const body = req.url.includes(refs[0].digest) ? bytes : small; res.writeHead(200, { "Content-Length": body.length }); res.end(body); });
    if (pending.length === 2) pending.splice(0).forEach((respond) => respond());
  });
  const h = await ready(init(http.base));
  try {
    refs.forEach((ref, index) => h.send(F.object, BigInt(index + 1), { reference: ref, timeout_ms: 2000 }));
    await h.next((f) => f.type === F.objectDone && f.id === 1n); await h.next((f) => f.type === F.objectDone && f.id === 2n);
    for (let i = 0; i < refs.length; i++) {
      const chunks = h.received.filter((f) => f.type === F.data && f.id === BigInt(i + 1)); assert.ok(chunks.every((chunk) => chunk.body.length <= 65536));
      assert.deepEqual(Buffer.concat(chunks.map((chunk) => chunk.body)), i ? small : bytes);
    }
    assert.equal(requests.length, 2); assert.ok(requests.every((r) => r.auth === "Bearer private-fixture-token" && r.workspace === "own-workspace" && r.method === "GET"));
  } finally { await h.close(); await http.close(); }
});

test("root lookup returns the existing validated replicated receipt without uploads", async () => {
  const root = reference(Buffer.from("opaque root"), "source_root");
  const receipt = { format_version: 1, root_id: "prepared-root", root, durability: "replicated", published_at_ms: 1, object_count: 1, total_bytes: root.size_bytes };
  const paths = []; const http = await server((req, res) => { paths.push([req.method, req.url]); res.end(JSON.stringify(receipt)); });
  const h = await ready(init(http.base));
  try { h.send(F.root, 1n, { root_id: receipt.root_id, timeout_ms: 1000 }); assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.rootDone)).body), receipt); assert.deepEqual(paths, [["GET", "/api/rudder/fabric/roots/prepared-root"]]); }
  finally { await h.close(); await http.close(); }
});

test("cancel reaches real HTTP socket and has a terminal acknowledgement", async () => {
  let started; const requestStarted = new Promise((resolve) => { started = resolve; }); let closed; const requestClosed = new Promise((resolve) => { closed = resolve; });
  const http = await server((req) => { req.socket.once("close", closed); started(); }); const h = await ready(init(http.base));
  try {
    h.send(F.object, 1n, { reference: reference(Buffer.from("not sent")), timeout_ms: 1500 }); await requestStarted; h.send(F.cancel, 1n);
    assert.equal(JSON.parse((await h.next((f) => f.type === F.error)).body).code, "cancelled");
    await bounded(requestClosed, 1000, "HTTP socket survived cancellation");
  } finally { await h.close(); await http.close(); }
});

test("uncooperative fetch retains admission until it actually settles", async () => {
  let release, observedSignal; const blocked = new Promise((resolve) => { release = resolve; });
  const h = await ready({ ...init(), max_jobs: 1 }, async (_url, request) => { observedSignal = request.signal; await blocked; throw new Error("private provider error"); });
  try {
    h.send(F.object, 1n, { reference: reference(Buffer.from("blocked")), timeout_ms: 500 });
    for (let i = 0; !observedSignal && i < 50; i++) await sleep(2);
    h.send(F.cancel, 1n); await sleep(25); assert.equal(observedSignal.aborted, true); assert.equal(h.received.some((f) => f.id === 1n), false);
    h.send(F.object, 2n, { reference: reference(Buffer.from("other")), timeout_ms: 500 });
    assert.equal(JSON.parse((await h.next((f) => f.id === 2n)).body).code, "busy");
    release(); assert.equal(JSON.parse((await h.next((f) => f.id === 1n)).body).code, "cancelled");
  } finally { release(); await h.close(); }
});

test("absolute deadline aborts a stalled real HTTP response", async () => {
  const http = await server((_req, res) => { res.writeHead(200); res.write("a"); }); const h = await ready(init(http.base));
  try { h.send(F.object, 1n, { reference: reference(Buffer.alloc(20)), timeout_ms: 40 }); assert.equal(JSON.parse((await h.next((f) => f.type === F.error)).body).code, "timeout"); }
  finally { await h.close(); await http.close(); }
});

test("hash, size, root receipt and provider text failures expose no unverified bytes", async () => {
  let mode = "hash";
  const bytes = Buffer.from("right");
  const http = await server((_req, res) => { if (mode === "hash") res.end("wrong"); else if (mode === "size") { res.writeHead(200, { "Content-Length": 10000 }); res.end("secret-provider-error"); } else res.end(JSON.stringify({ root_id: "foreign", token: "secret-provider-error" })); });
  const h = await ready(init(http.base));
  try {
    for (const [index, value] of ["hash", "size", "root"].entries()) {
      mode = value; h.send(value === "root" ? F.root : F.object, BigInt(index + 1), value === "root" ? { root_id: "own", timeout_ms: 1000 } : { reference: reference(bytes), timeout_ms: 1000 });
      assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.error)).body), { code: "failed" });
    }
    assert.equal(h.received.some((f) => f.type === F.data), false); assert.equal(JSON.stringify(h.received).includes("secret-provider-error"), false);
  } finally { await h.close(); await http.close(); }
});

test("auth rejection closes the scoped helper and never emits provider credentials", async () => {
  const http = await server((_req, res) => { res.writeHead(401); res.end("private-fixture-token private server diagnostics"); }); const h = await ready(init(http.base));
  try { h.send(F.object, 1n, { reference: reference(Buffer.alloc(0)), timeout_ms: 1000 }); assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.fatal)).body), { code: "auth_failed" }); await h.task; assert.equal(h.received.some((f) => f.type === F.data), false); }
  finally { await h.close(); await http.close(); }
});

test("redirects never forward private auth to a second origin", async () => {
  let foreignCalls = 0; const foreign = await server((_req, res) => { foreignCalls++; res.end("wrong"); });
  const original = await server((_req, res) => { res.writeHead(302, { Location: foreign.base }); res.end(); });
  const h = await ready(init(original.base));
  try { h.send(F.object, 1n, { reference: reference(Buffer.from("right")), timeout_ms: 1000 }); assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.error)).body), { code: "failed" }); assert.equal(foreignCalls, 0); }
  finally { await h.close(); await original.close(); await foreign.close(); }
});

test("initialization rejects public HTTP, userinfo, URL suffixes, backslashes and extra auth", async () => {
  const cases = [
    { base_url: "http://example.com/api" }, { base_url: "http://localhost/api" }, { base_url: "http://user:pass@127.0.0.1/api" },
    { base_url: "http://127.0.0.1/api?q=private" }, { base_url: "http://127.0.0.1/api#private" }, { base_url: "http://127.0.0.1/api\\evil" },
    { auth_headers: { Authorization: "private", Cookie: "private" } }, { auth_headers: { Authorization: "private", authorization: "different" } }, { extra: "field" },
  ];
  for (const change of cases) { const h = harness({ ...init(), ...change }); assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.fatal)).body), { code: "protocol" }); await h.task; }
});

test("second initialization cannot rotate auth or endpoint", async () => {
  const h = await ready(init(), async () => new Response("unused"));
  h.send(F.init, 0n, { ...init(), auth_headers: { Authorization: "new secret" } });
  assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.fatal)).body), { code: "protocol" }); await h.task;
});

test("oversized, partial and reserved-bit frames close without growing an unbounded parser", async () => {
  for (const type of ["large", "partial", "reserved", "foreign-cancel"]) {
    const h = await ready(init(), async () => new Response("unused"));
    if (type === "foreign-cancel") h.send(F.cancel, 999n);
    else { const bad = Buffer.alloc(16); bad[0] = F.object; if (type === "large") bad.writeUInt32BE(65537, 12); if (type === "reserved") bad[2] = 1; h.input.end(type === "partial" ? bad.subarray(0, 9) : bad); }
    assert.deepEqual(JSON.parse((await h.next((f) => f.type === F.fatal)).body), { code: "protocol" }); await h.task;
  }
});
