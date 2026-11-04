import assert from "node:assert/strict";
import test from "node:test";
import "./board-source.mjs";

const { isLoopbackHost } = await import("../src/board/daemon.ts");
const { BoardAuthority } = await import("../src/board/authority.ts");

// The guards only read req.headers, so a plain object stands in for IncomingMessage.
const reqWith = (headers) => ({ headers });

test("isLoopbackHost accepts loopback hosts and rejects everything else (anti-DNS-rebinding)", () => {
  for (const host of ["127.0.0.1:4774", "localhost:4774", "127.0.0.1", "localhost", "[::1]:4774"]) {
    assert.equal(isLoopbackHost(reqWith({ host })), true, `loopback: ${host}`);
  }
  for (const host of ["evil.com", "evil.com:4774", "rudder.example.com", "192.168.1.5:4774", ""]) {
    assert.equal(isLoopbackHost(reqWith({ host })), false, `non-loopback: ${host}`);
  }
  assert.equal(isLoopbackHost(reqWith({})), false, "missing Host is rejected");
});

test("private tickets, bearers and streams obey instance, origin, expiry and admission bounds", () => {
  let now = 0;
  const authority = new BoardAuthority("project", "projector", () => now);
  authority.bindOrigin("http://127.0.0.1:4774");
  const req = reqWith({ host: "127.0.0.1:4774" });
  try {
    for (let i = 0; i < 64; i++) authority.bootstrapUrl();
    assert.throws(() => authority.bootstrapUrl(), /unavailable/);
    now = 30_001;
    const ticket = new URL(authority.bootstrapUrl()).hash.split("=")[1];
    assert.equal(authority.exchange(req, ticket, "wrong-instance"), null);
    const result = authority.exchange(req, ticket, authority.instanceId);
    assert.ok(result);
    assert.equal(authority.exchange(req, ticket, authority.instanceId), null, "tickets are one-use");
    const offered = reqWith({ host: "127.0.0.1:4774", authorization: `Bearer ${result.token}` });
    const session = authority.authenticate(offered);
    assert.ok(session);
    assert.equal(authority.authenticate(reqWith({ ...offered.headers, origin: "http://127.0.0.1:4775" })), null);
    assert.equal(authority.authenticate(reqWith({ ...offered.headers, authorization: `Bearer ${"0".repeat(64)}` })), null);
    let closed = 0;
    for (let i = 0; i < 4; i++) assert.ok(authority.registerStream(session, () => closed++));
    assert.equal(authority.registerStream(session, () => {}), null);
    now += 3_600_001; authority.reap();
    assert.equal(closed, 4);
    assert.equal(authority.authenticate(offered), null);
  } finally { authority.close(); }
});

test("session and global SSE capacity stay bounded until expiry, release or owner shutdown", () => {
  let now = 0, closed = 0;
  const authority = new BoardAuthority("owner", "scheduler", () => now);
  authority.bindOrigin("http://127.0.0.1:4774");
  const req = reqWith({ host: "127.0.0.1:4774" });
  try {
    const sessions = [];
    for (let i = 0; i < 64; i++) {
      const ticket = new URL(authority.bootstrapUrl()).hash.split("=")[1];
      const result = authority.exchange(req, ticket, authority.instanceId); assert.ok(result);
      sessions.push(authority.authenticate(reqWith({ ...req.headers, authorization: `Bearer ${result.token}` })));
    }
    const extra = new URL(authority.bootstrapUrl()).hash.split("=")[1];
    assert.equal(authority.exchange(req, extra, authority.instanceId), null);
    const release = [];
    for (let i = 0; i < 8; i++) for (let j = 0; j < 4; j++) release.push(authority.registerStream(sessions[i], () => closed++));
    assert.ok(release.every(Boolean)); assert.equal(authority.registerStream(sessions[8], () => {}), null);
    release[0](); assert.ok(authority.registerStream(sessions[8], () => closed++));
    now = 30_001; assert.equal(authority.exchange(req, extra, authority.instanceId), null, "expired admission ticket is never revived");
    now = 3_600_001; authority.reap(); assert.equal(closed, 32);
    const fresh = new URL(authority.bootstrapUrl()).hash.split("=")[1];
    assert.ok(authority.exchange(req, fresh, authority.instanceId));
    authority.close(); assert.equal(authority.matchesOrigin(req), false); assert.throws(() => authority.bootstrapUrl(), /unavailable/);
  } finally { authority.close(); }
});

test("canonical default-port origins keep exact Host and Origin checks", () => {
  const authority = new BoardAuthority("owner", "projector");
  try {
    authority.bindOrigin("http://127.0.0.1");
    assert.equal(authority.matchesOrigin(reqWith({ host: "127.0.0.1" })), true);
    assert.equal(authority.matchesOrigin(reqWith({ host: "127.0.0.1:80", origin: "http://127.0.0.1" })), true);
    for (const host of ["127.0.0.1:81", "localhost", "127.0.0.1.evil"]) assert.equal(authority.matchesOrigin(reqWith({ host })), false);
  } finally { authority.close(); }
});
