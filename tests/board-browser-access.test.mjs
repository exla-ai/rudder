import "./board-source.mjs";
import assert from "node:assert/strict";
import test from "node:test";
import * as esbuild from "esbuild";
const { BrowserBoardAccess, BoardAccessExpired } = await import("../src/board/ui/access.ts");
const { readBoardEvents, streamBoardEvents } = await import("../src/board/ui/events.ts");

const origin = "http://127.0.0.1:4774";
const instance = "11111111-1111-4111-8111-111111111111";
const ticket = "a".repeat(64), token = "b".repeat(64);
const descriptor = { format_version: 1, origin, instance_id: instance };
const session = (expires = 3_600_000) => ({ ...descriptor, expires_at_ms: expires, owner_slug: "owner", control_mode: "projector",
  scope: { registered_projects: true, usage: true, owner_project_control: true } });
const json = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "content-type": "application/json" } });
function storage() {
  const values = new Map();
  return { get length() { return values.size; }, key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null, setItem: (key, value) => values.set(key, value), removeItem: (key) => values.delete(key) };
}
function environment(overrides = {}) {
  return { origin, href: `${origin}/rudder/owner#rudder-board-ticket=${ticket}`, now: () => 0, replaceUrl: () => {}, storage: storage(),
    fetch: async (url) => url.endsWith("/descriptor") ? json(descriptor) : json({ token, session: session() }), ...overrides };
}

test("browser sessions renew once for concurrent requests and expire without sending stale authority", async (t) => {
  let now = 0, renewals = 0, calls = 0, lost = 0;
  const saved = storage();
  const access = new BrowserBoardAccess(environment({ now: () => now, storage: saved, fetch: async (url, init) => {
    calls++;
    if (url.endsWith("/descriptor")) return json(descriptor);
    if (url.endsWith("/session")) return json({ token, session: session() });
    assert.equal(new Headers(init.headers).get("Authorization"), `Bearer ${token}`);
    if (url.endsWith("/renew")) { renewals++; await Promise.resolve(); return json(session(now + 3_600_000)); }
    return json([]);
  } }));
  t.after(() => access.close()); access.onLost(() => lost++);
  await access.initialize(); now = 3_550_000;
  await Promise.all([access.fetch("/api/projects"), access.fetch("/api/usage/machine")]);
  assert.equal(renewals, 1);
  const before = calls; now += 3_600_001;
  await assert.rejects(access.fetch("/api/projects"), BoardAccessExpired);
  assert.equal(calls, before); assert.equal(saved.length, 0); assert.equal(lost, 1);
  assert.deepEqual(Object.keys(access), [], "tokens, bootstrap fragments and environment are private class state");
});

test("an expired/replayed fragment and unauthorized saved session clear local authority without manual token handling", async (t) => {
  for (const freshTicket of [true, false]) {
    const saved = storage(); let erased = !freshTicket;
    saved.setItem(`rudder.board.session.v1:${origin}:${instance}`, JSON.stringify({ token, session: session() }));
    const access = new BrowserBoardAccess(environment({ storage: saved, href: origin + (freshTicket ? `/#rudder-board-ticket=${ticket}` : "/usage"),
      replaceUrl: (value) => { erased = true; assert.ok(!value.includes(ticket)); },
      fetch: async (url) => { assert.ok(erased); return url.endsWith("/descriptor") ? json(descriptor) : json({ error: "expired" }, 401); } }));
    t.after(() => access.close()); await assert.rejects(access.initialize(), BoardAccessExpired);
    assert.equal(saved.length, 0); await assert.rejects(access.fetch("/api/projects"), BoardAccessExpired);
  }
});

test("late bootstrap responses cannot resurrect a closed session and storage failure permits memory-only use", async (t) => {
  let finish;
  const access = new BrowserBoardAccess(environment({ fetch: () => new Promise((resolve) => { finish = resolve; }) }));
  const opening = access.initialize(); access.close(); finish(json(descriptor));
  await assert.rejects(opening, BoardAccessExpired); await assert.rejects(access.fetch("/api/projects"), BoardAccessExpired);
  const deniedStorage = { get length() { throw new Error("denied"); }, getItem() { throw new Error("denied"); },
    setItem() { throw new Error("denied"); }, removeItem() { throw new Error("denied"); }, key() { throw new Error("denied"); } };
  const memory = new BrowserBoardAccess(environment({ storage: deniedStorage })); t.after(() => memory.close());
  await memory.initialize(); assert.equal((await memory.fetch("/api/projects")).status, 200);
});

test("SSE handles split UTF-8/CRLF, rejects excess work and releases a cancelled reader", async () => {
  const frames = [];
  const bytes = new TextEncoder().encode(": ping\r\n\r\nevent: snapshot\r\ndata: {\"text\":\"雪\"}\r\ndata: second\r\n\r\ndata: final\n\n");
  const response = new Response(new ReadableStream({ start(controller) { for (const byte of bytes) controller.enqueue(Uint8Array.of(byte)); controller.close(); } }));
  await readBoardEvents(response, (frame) => frames.push(frame), new AbortController().signal);
  assert.deepEqual(frames, [{ event: "snapshot", data: '{"text":"雪"}\nsecond' }, { event: "message", data: "final" }]);
  let cancelled = 0;
  const oversized = new Response(new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("data: " + "x".repeat(4 * 1024 * 1024))); }, cancel() { cancelled++; } }));
  await assert.rejects(readBoardEvents(oversized, () => assert.fail("must not emit an oversized frame"), new AbortController().signal), /frame limit/);
  assert.equal(cancelled, 1);
  const controller = new AbortController();
  const waiting = new Response(new ReadableStream({ cancel() { cancelled++; } }));
  const reading = readBoardEvents(waiting, () => {}, controller.signal); controller.abort(); await reading;
  assert.equal(cancelled, 2);
});

test("SSE reconnects sequentially with bearer headers, no query secrets, and stops on authentication loss", { timeout: 5000 }, async (t) => {
  let connections = 0, reconnects = 0, open = 0;
  const events = [], controller = new AbortController();
  const access = new BrowserBoardAccess(environment({ fetch: async (url, init) => {
    if (url.endsWith("/descriptor")) return json(descriptor);
    if (url.endsWith("/session")) return json({ token, session: session() });
    assert.equal(url, `${origin}/api/projects/owner/events`);
    assert.equal(new Headers(init.headers).get("Authorization"), `Bearer ${token}`);
    assert.equal(new Headers(init.headers).get("Accept"), "text/event-stream");
    assert.equal(init.credentials, "omit"); assert.ok(init.signal);
    connections++;
    return connections === 1 ? new Response("event: snapshot\ndata: {}\n\n", { headers: { "content-type": "text/event-stream" } }) : json({}, 401);
  } }));
  t.after(() => access.close()); await access.initialize();
  await streamBoardEvents("/api/projects/owner/events", { signal: controller.signal, open: () => open++,
    consume: (event) => events.push(event), reconnecting: () => reconnects++ }, access.fetch.bind(access));
  assert.equal(connections, 2); assert.equal(open, 1); assert.equal(reconnects, 1); assert.equal(events.length, 1);
  await assert.rejects(access.fetch("/api/projects"), BoardAccessExpired);
});

test("the real browser entry bundles without inline bootstrap authority", async () => {
  const built = await esbuild.build({ entryPoints: [new URL("../src/board/ui/index.tsx", import.meta.url).pathname], bundle: true,
    format: "esm", platform: "browser", jsx: "automatic", jsxImportSource: "preact", write: false, logLevel: "silent" });
  const source = built.outputFiles[0].text;
  assert.doesNotMatch(source, /__RUDDER_TOKEN__|x-rudder-token|new EventSource\(/);
  assert.match(source, /Authorization/);
});
