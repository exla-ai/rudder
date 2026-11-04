import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { createHash, randomUUID } from "node:crypto";
import { FabricCas, MemoryFabricObjectStorage, canonicalFabricMetadata } from "../cloud/dist/fabric-cas.js";
import { FabricRemoteClient, FabricRemoteError } from "../dist/fabric-remote.js";

const ref = (kind, bytes) => ({ kind, algorithm: "sha256", digest: createHash("sha256").update(bytes).digest("hex"), size_bytes: bytes.length });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function fixtureObjects() {
  const first = Buffer.from("first source"); const second = Buffer.from("second source");
  const blobs = [first, second].map((bytes) => ({ object: ref("blob", bytes), bytes }));
  const bytes = canonicalFabricMetadata({ format_version: 1, mode: 0o755, entries: blobs.map(({ object }, index) => ({ name_hex: Buffer.from(`file-${index}`).toString("hex"), node: { type: "file", object, mode: 0o644 } })) });
  const root = { object: ref("directory", bytes), bytes };
  return { root, all: [...blobs, root] };
}
function sourceAdapter(all) {
  const events = []; const owned = new Set(); const userPin = "user-existing-pin"; const read = new Map(all.map(({ object, bytes }) => [object.digest, bytes]));
  return {
    events, owned, userPin,
    async pin() { const pin = `transfer-${randomUUID()}`; events.push("pin"); owned.add(pin); return pin; },
    async unpin(id) { assert.notEqual(id, userPin); assert.ok(owned.delete(id)); events.push("unpin"); },
    async closure() { assert.equal(owned.size, 1); events.push("closure"); return all.map(({ object }) => object); },
    async read(object) { assert.equal(owned.size, 1); events.push("read"); return Buffer.from(read.get(object.digest)); },
  };
}
async function serverFixture(t, options = {}) {
  const cas = options.cas || new FabricCas({ storage: new MemoryFabricObjectStorage(), accountId: "account-a" });
  const requests = []; let active = 0, maxActive = 0;
  const server = http.createServer(async (request, response) => {
    active++; maxActive = Math.max(maxActive, active);
    const send = (status, body) => { response.writeHead(status, { "Content-Type": "application/json" }); response.end(JSON.stringify(body)); };
    try {
      assert.equal(request.headers.authorization, "Bearer fixture-only");
      const parts = new URL(request.url, "http://127.0.0.1").pathname.replace(/^\/api\/rudder\/fabric\/?/, "").split("/");
      requests.push({ method: request.method, path: parts.join("/") });
      const chunks = []; for await (const chunk of request) chunks.push(chunk); const bytes = Buffer.concat(chunks);
      const body = bytes.length && request.headers["content-type"] === "application/json" ? JSON.parse(bytes.toString()) : null;
      if (parts[0] === "staging" && parts.length === 1) return send(200, await cas.beginStaging(body.ttl_ms));
      if (parts[0] === "staging" && parts[2] === "renew") return send(200, await cas.renewStaging(parts[1], body.ttl_ms));
      if (parts[0] === "missing") return send(200, { format_version: 1, missing: await cas.missing(body.objects) });
      if (parts[0] === "objects") {
        const object = { kind: parts[1], algorithm: "sha256", digest: parts[2], size_bytes: Number(parts[3]) };
        if (request.method === "PUT") { if (options.putDelay) await sleep(options.putDelay); return send(200, await cas.putObject(request.headers["x-rudder-staging-lease"], object, bytes)); }
        response.end(await cas.getObject(object)); return;
      }
      if (parts[0] === "roots") return send(200, request.method === "POST" ? await cas.publishRoot(parts[1], body.lease_id, body.root) : await cas.getRoot(parts[1]));
      send(404, {});
    } catch (error) { send(error.status || 500, { error: { code: error.code || "failed", message: "safe fixture error" } }); }
    finally { active--; }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => { server.closeAllConnections(); await new Promise((resolve) => server.close(resolve)); });
  const client = new FabricRemoteClient({ baseUrl: `http://127.0.0.1:${server.address().port}/api/rudder/fabric`, authHeaders: () => ({ Authorization: "Bearer fixture-only" }), concurrency: 2 });
  return { cas, client, requests, maxActive: () => maxActive };
}

test("real HTTP transfer pins source, persists lease first, uploads missing objects and verifies publication", async (t) => {
  const { root, all } = fixtureObjects(); const source = sourceAdapter(all); const { cas, client, requests, maxActive } = await serverFixture(t, { putDelay: 10 });
  const result = await client.uploadRoot("snapshot", root.object, source);
  assert.equal(result.receipt.durability, "replicated"); assert.equal(result.uploaded_objects, 3); assert.equal(source.owned.size, 0);
  assert.equal(source.events[0], "pin"); assert.equal(source.events.at(-1), "unpin");
  assert.equal(requests[0].path, "staging"); assert.ok(requests.findIndex((item) => item.path === "staging") < requests.findIndex((item) => item.method === "PUT"));
  assert.ok(requests.some((item) => /^staging\/.+\/renew$/.test(item.path)), "lease is renewed before publication");
  assert.ok(maxActive() <= 2, "object uploads respect configured concurrency");
  assert.deepEqual(await client.getRoot("snapshot"), result.receipt);
  for (const { object, bytes } of all) assert.deepEqual(await client.getObject(object), bytes);
  const repeated = await client.uploadRoot("snapshot", root.object, source);
  assert.equal(repeated.uploaded_objects, 0); assert.deepEqual(repeated.receipt, result.receipt);
  assert.equal((await cas.closure(root.object)).objects.length, 3);
});

test("client validates bytes before upload and always releases only its own temporary source pin", async (t) => {
  const { root, all } = fixtureObjects(); const source = sourceAdapter(all); const original = source.read;
  source.read = async (object) => object.kind === "blob" ? Buffer.from("corrupt source") : original(object);
  const { client, cas } = await serverFixture(t);
  await assert.rejects(client.uploadRoot("must-not-publish", root.object, source), { code: "checksum_mismatch" });
  assert.equal(source.owned.size, 0); assert.equal(await cas.getRoot("must-not-publish"), null);
});

test("a failed concurrent transfer waits for the other source read before releasing retention", async (t) => {
  const { root, all } = fixtureObjects(); const source = sourceAdapter(all); let release; let started = 0;
  const original = source.read;
  source.read = async (object) => {
    started++;
    if (started === 1) { await sleep(10); throw new Error("private source failure"); }
    await new Promise((resolve) => { release = resolve; });
    return original(object);
  };
  const { client, cas } = await serverFixture(t);
  const result = client.uploadRoot("incomplete", root.object, source).then(() => assert.fail("transfer must fail"), (error) => error);
  for (let count = 0; count < 100 && !release; count++) await sleep(2);
  assert.ok(release); await sleep(15); assert.equal(source.owned.size, 1);
  release(); const error = await result;
  assert.ok(error instanceof FabricRemoteError); assert.ok(!error.message.includes("private source failure"));
  assert.equal(source.owned.size, 0); assert.equal(await cas.getRoot("incomplete"), null);
});

test("publication remains successful when its owned local pin needs cleanup", async (t) => {
  const { root, all } = fixtureObjects(); const source = sourceAdapter(all);
  source.unpin = async () => { throw new Error("local disk unavailable"); };
  const { client, cas } = await serverFixture(t);
  const result = await client.uploadRoot("committed", root.object, source);
  assert.match(result.cleanup_warning, /committed/); assert.deepEqual(await cas.getRoot("committed"), result.receipt);
});

test("download rejects checksum corruption and oversized or malformed responses", async () => {
  const bytes = Buffer.from("abc"); const object = ref("blob", bytes);
  for (const response of [() => new Response("abd"), () => new Response("abcd"), () => new Response("abc", { headers: { "Content-Length": "999999999" } })]) {
    const client = new FabricRemoteClient({ baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => ({}), fetch: async () => response() });
    await assert.rejects(client.getObject(object), (error) => error instanceof FabricRemoteError);
  }
});

test("malicious missing-object responses cannot make source read undeclared objects", async () => {
  const bytes = Buffer.from("abc"); const object = ref("blob", bytes); const other = ref("blob", Buffer.from("other"));
  for (const missing of [[other], [object, object]]) {
    const client = new FabricRemoteClient({ baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => ({}), fetch: async () => Response.json({ format_version: 1, missing }) });
    await assert.rejects(client.missing([object]), { code: "invalid_response" });
  }
});

test("HTTP deadlines include an uncooperative transport and response body decoding", async () => {
  const options = { baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => ({}), requestTimeoutMs: 10 };
  const hangs = new FabricRemoteClient({ ...options, fetch: () => new Promise(() => { }) });
  await assert.rejects(hangs.beginStaging(), { code: "request_timeout" });
  const body = new FabricRemoteClient({ ...options, fetch: async () => new Response(new ReadableStream({ start() { } })) });
  await assert.rejects(body.beginStaging(), { code: "request_timeout" });
});

test("timed-out PUTs hold request admission through retries until the transport actually settles", async (t) => {
  const bytes = Buffer.from("retained upload"); const object = ref("blob", bytes);
  const source = sourceAdapter([{ object, bytes }]);
  let releasePut, stalled = true, activePuts = 0, putCalls = 0, authCalls = 0;
  const client = new FabricRemoteClient({
    baseUrl: "https://fixture.invalid/api/rudder/fabric", concurrency: 1, requestTimeoutMs: 20,
    authHeaders: () => { authCalls++; return {}; },
    fetch: async (url, init) => {
      if (init.method === "PUT") {
        putCalls++; activePuts++;
        try { if (stalled) await new Promise((resolve) => { releasePut = resolve; }); }
        finally { activePuts--; }
        return Response.json({ format_version: 1 });
      }
      if (url.pathname.endsWith("/missing")) return Response.json({ format_version: 1, missing: [object] });
      if (init.method === "DELETE") return Response.json({ format_version: 1 });
      if (url.pathname.includes("/roots/")) return Response.json({ format_version: 1, root_id: "retry", root: object, durability: "replicated", published_at_ms: Date.now(), object_count: 1, total_bytes: bytes.length });
      return Response.json({ format_version: 1, lease_id: /\/staging\/([^/]+)\/renew$/.exec(url.pathname)?.[1] ?? randomUUID(), expires_at_ms: Date.now() + 300_000 });
    },
  });
  t.after(() => releasePut?.());
  await assert.rejects(client.uploadRoot("retry", object, source), { code: "request_timeout" });
  assert.equal(activePuts, 1); assert.equal(source.owned.size, 0);
  const admittedAuthCalls = authCalls;
  for (let count = 0; count < 4; count++) {
    await assert.rejects(client.uploadRoot("retry", object, source), { code: "request_busy" });
    await assert.rejects(client.missing([object]), { code: "request_busy" });
    await assert.rejects(client.getObject(object), { code: "request_busy" });
  }
  assert.equal(authCalls, admittedAuthCalls, "rejected admission must not start more authentication work");
  assert.equal(putCalls, 1); assert.equal(activePuts, 1); assert.equal(source.owned.size, 0);
  stalled = false; releasePut();
  for (let count = 0; count < 100 && client.inFlight; count++) await sleep(1);
  const result = await client.uploadRoot("retry", object, source);
  assert.equal(result.uploaded_objects, 1); assert.equal(putCalls, 2); assert.equal(activePuts, 0);
});

test("request admission recovers after preflight rejection and synchronous transport failure", async () => {
  const bytes = Buffer.from("abc"); const object = ref("blob", bytes); let calls = 0;
  const client = new FabricRemoteClient({ baseUrl: "https://fixture.invalid/api/rudder/fabric", concurrency: 1, authHeaders: () => ({}), fetch: () => {
    if (++calls === 1) throw new Error("private transport failure");
    return Promise.resolve(Response.json({ format_version: 1, lease_id: randomUUID(), expires_at_ms: Date.now() + 300_000 }));
  } });
  await assert.rejects(client.putObject(randomUUID(), object, Buffer.from("bad")), { code: "checksum_mismatch" });
  assert.equal(calls, 0);
  await assert.rejects(client.beginStaging(), { code: "request_failed" });
  assert.equal((await client.beginStaging()).format_version, 1);
});

test("server errors and auth-provider failures never echo private data", async () => {
  const options = { baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => ({ Authorization: "Bearer private-test-only" }) };
  const rejected = new FabricRemoteClient({ ...options, fetch: async () => new Response("secret server details", { status: 500 }) });
  await assert.rejects(rejected.beginStaging(), (error) => !error.message.includes("secret") && !error.message.includes("private-test-only"));
  const badAuth = new FabricRemoteClient({ ...options, authHeaders: () => { throw new Error("secret token detail"); } });
  await assert.rejects(badAuth.beginStaging(), (error) => !error.message.includes("secret"));
});

test("late authentication cannot start a POST after the request deadline", async () => {
  let release; let calls = 0; let stalled = true;
  const client = new FabricRemoteClient({ baseUrl: "https://fixture.invalid/api/rudder/fabric", authHeaders: () => stalled ? new Promise((resolve) => { release = resolve; }) : {}, fetch: async () => { calls++; return Response.json({ format_version: 1, lease_id: randomUUID(), expires_at_ms: Date.now() + 300_000 }); }, requestTimeoutMs: 10 });
  await assert.rejects(client.beginStaging(), { code: "request_timeout" });
  await assert.rejects(client.beginStaging(), { code: "request_busy" });
  stalled = false;
  release({ Authorization: "Bearer fixture-only" }); await sleep(5);
  assert.equal(calls, 0);
  assert.equal((await client.beginStaging()).format_version, 1); assert.equal(calls, 1);
});
