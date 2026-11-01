import assert from "node:assert/strict";
import { inspect } from "node:util";
import { createHash } from "node:crypto";
import test from "node:test";
import { createFlyMachineLeaseSession, flyMachineConfigDigest, FlyCustodyError } from "../cloud/src/fly-custody.ts";

const NONCE = "private-fake-provider-nonce-72ab";
const CONFIG = { image: "registry.example/fixture@sha256:" + "1".repeat(64), guest: { cpus: 1, memory_mb: 256 }, services: [] };
const EXPECTED = { app: "fixture-workers", machineId: "abcd1234ef5678", instanceId: "01MACHINEGENERATION00000001",
  configSha256: flyMachineConfigDigest(CONFIG), challenge: "c".repeat(64) };
const copy = value => structuredClone(value);
const rejected = code => error => error instanceof FlyCustodyError && error.code === code;
function gate() {
  let entered, release;
  return { entered: new Promise(resolve => { entered = resolve; }), wait: new Promise(resolve => { release = resolve; }),
    enter: () => entered(), release: () => release() };
}
function fixture(overrides = {}) {
  let at = { wallMs: 1_700_000_000_000, boottimeMs: 10_000 };
  const calls = [], f = {
    expected: copy(EXPECTED), machine: { id: EXPECTED.machineId, version: EXPECTED.instanceId,
      instance_id: EXPECTED.instanceId, config: copy(CONFIG) }, lease: null, hook: null,
    advance(ms) { at = { wallMs: at.wallMs + ms, boottimeMs: at.boottimeMs + ms }; },
    wallStep(ms) { at = { ...at, wallMs: at.wallMs + ms }; },
    bootStep(ms) { at = { ...at, boottimeMs: at.boottimeMs + ms }; },
    now: () => ({ ...at }), calls,
  };
  const route = request => {
    const path = request.path, base = `/v1/apps/${f.expected.app}/machines/${f.expected.machineId}`;
    assert.ok(path.startsWith(base));
    const held = request.headers["fly-machine-lease-nonce"] === NONCE;
    if (path === `${base}/lease`) {
      if (request.method === "POST") {
        const body = JSON.parse(request.body);
        assert.equal([...body.description].length, 64, "actual provider description limit is64 runes");
        assert.equal(Buffer.byteLength(body.description), 64);
        assert.match(body.description, /^[0-9a-f]{64}$/);
        if (f.lease && !held) return { status: 409, body: { error: "leased" } };
        f.lease = { nonce: NONCE, expires_at: Math.ceil((at.wallMs + body.ttl * 1000) / 1000),
          description: body.description, owner: "fixture-owner", version: f.machine.version };
        return { status: 201, body: { status: "success", data: copy(f.lease) } };
      }
      if (request.method === "GET") return f.lease
        ? { status: 200, body: { status: "success", data: copy(f.lease) } }
        : { status: 404, body: { error: "not leased" } };
      if (request.method === "DELETE") {
        if (!held || !f.lease) return { status: 409, body: { error: "nonce mismatch" } };
        f.lease = null; return { status: 200, body: { status: "success", data: { ok: true } } };
      }
    }
    if (request.method === "GET" && path === base) return { status: 200, body: copy(f.machine) };
    assert.equal(held, true, "every actual mutation carries the directly acquired nonce");
    if (request.method === "POST" && path === base) {
      const body = JSON.parse(request.body);
      assert.equal(body.current_version, EXPECTED.instanceId);
      assert.equal(body.lease_ttl, undefined);
      f.machine.config = body.config;
      f.machine.version = f.machine.instance_id = "01MACHINEGENERATION00000002";
      return { status: 200, body: copy(f.machine) };
    }
    return { status: 200, body: { ok: true } };
  };
  const transport = { async request(request) {
    calls.push(request); const reply = route(request);
    return f.hook ? f.hook(request, reply) : reply;
  } };
  f.session = createFlyMachineLeaseSession({ expected: f.expected, transport, clock: { sample: f.now },
    ttlSeconds: 15, requestTimeoutMs: 1500, ...overrides });
  return f;
}

test("direct acquisition, exact identity reads, original ends and explicit release use official paths/header", async () => {
  const f = fixture(), observation = await f.session.acquire();
  assert.equal(observation.scope, "machine-lease-only"); assert.equal(observation.serial, 1);
  assert.equal(observation.originalEndWallMs, f.now().wallMs + 14_900);
  assert.equal(observation.originalEndBoottimeMs, f.now().boottimeMs + 14_900);
  assert.equal(Object.isFrozen(observation), true);
  assert.deepEqual(f.calls.map(call => call.method), ["POST", "GET", "GET", "GET"]);
  assert.equal(f.calls[0].headers["fly-machine-lease-nonce"], undefined);
  assert.equal(JSON.parse(f.calls[0].body).ttl, 15);
  assert.ok(f.calls.slice(1).every(call => call.headers["fly-machine-lease-nonce"] === NONCE));
  f.advance(200);
  const checked = await f.session.observe();
  assert.equal(checked.originalEndWallMs, observation.originalEndWallMs);
  assert.equal(checked.originalEndBoottimeMs, observation.originalEndBoottimeMs);
  await f.session.release(); assert.equal(f.session.status().state, "released");
  const last = f.calls.at(-1); assert.equal(last.method, "DELETE");
  assert.equal(last.path, `/v1/apps/${EXPECTED.app}/machines/${EXPECTED.machineId}/lease`);
  assert.equal(last.headers["fly-machine-lease-nonce"], NONCE);
  const count = f.calls.length; await f.session.release(); assert.equal(f.calls.length, count);
  await assert.rejects(f.session.observe(), rejected("not_current"));
});

test("acquisition and renewal descriptions preserve the whole challenge hash within64 provider runes", async () => {
  const challengeHash = value => createHash("sha256").update(value).digest("hex");
  const f = fixture(), first = await f.session.acquire(), expected = challengeHash(EXPECTED.challenge);
  assert.equal(first.challengeSha256, expected);
  assert.equal(f.session.status().challengeSha256, expected);
  f.advance(1000);
  const renewed = await f.session.renew(15);
  assert.equal(renewed.challengeSha256, expected);
  assert.equal(renewed.sessionEndWallMs, first.sessionEndWallMs);
  assert.equal(renewed.sessionEndBoottimeMs, first.sessionEndBoottimeMs);
  assert.equal(renewed.originalEndWallMs, first.originalEndWallMs);
  const posts = f.calls.filter(call => call.method === "POST");
  assert.equal(posts.length, 2);
  for (const request of posts) assert.deepEqual(JSON.parse(request.body), { description: expected, ttl: 15 });
  await f.session.release();
  // A last-hex-digit difference must change the full correlation; no shortened
  // prefix or reused wire description can bind two fresh challenges.
  const challenge = EXPECTED.challenge.slice(0, -1) + "d";
  const second = fixture({ expected: { ...EXPECTED, challenge } });
  const secondObservation = await second.session.acquire();
  assert.equal(JSON.parse(second.calls[0].body).description, challengeHash(challenge));
  assert.equal(secondObservation.challengeSha256, challengeHash(challenge));
  assert.notEqual(secondObservation.challengeSha256, expected);
  await second.session.release();
});

test("raw, legacy-prefixed, truncated or different hash echoes fence the original acquisition", async () => {
  for (const mismatch of ["raw", "legacy", "truncated", "last-digit"]) {
    const f = fixture();
    f.hook = (request, reply) => {
      if (request.method !== "POST" || !request.path.endsWith("/lease")) return reply;
      const wire = JSON.parse(request.body).description, result = copy(reply);
      result.body.data.description = mismatch === "raw" ? EXPECTED.challenge
        : mismatch === "legacy" ? `rudder-machine-custody-v1:${EXPECTED.challenge}`
        : mismatch === "truncated" ? wire.slice(0, -1)
        : wire.slice(0, -1) + (wire.at(-1) === "0" ? "1" : "0");
      return result;
    };
    await assert.rejects(f.session.acquire(), rejected("lease_changed"), mismatch);
    assert.equal(f.session.status().state, "uncertain", mismatch);
    await assert.rejects(f.session.observe(), rejected("not_current"));
    await assert.rejects(f.session.acquire(), rejected("not_current"));
    assert.equal(f.calls.filter(call => call.method === "POST").length, 1, "never replay a refused/uncertain acquire");
    f.hook = null; await f.session.release(); // only its directly observed private nonce, no restoration
    assert.equal(f.session.status().state, "uncertain");
  }
});

test("canonical exact config hash includes ingress fields and rejects unsupported/oversized inputs", () => {
  assert.equal(flyMachineConfigDigest({ b: [1, true], a: null }), flyMachineConfigDigest({ a: null, b: [1, true] }));
  assert.notEqual(flyMachineConfigDigest(CONFIG), flyMachineConfigDigest({ ...CONFIG, services: [{ internal_port: 8080 }] }));
  for (const input of [null, [], { x: undefined }, { x: Infinity }, { huge: "x".repeat(256 * 1024 + 1) }]) {
    assert.throws(() => flyMachineConfigDigest(input), rejected("invalid_input"));
  }
  const sparse = []; sparse.length = 1; sparse.other = 4;
  assert.throws(() => flyMachineConfigDigest({ sparse }), rejected("invalid_input"));
  const accessor = [1]; let accessed = false;
  Object.defineProperty(accessor, "0", { get() { accessed = true; return 1; }, enumerable: true });
  assert.throws(() => flyMachineConfigDigest({ accessor }), rejected("invalid_input"));
  assert.equal(accessed, false);
  const cyclic = {}; cyclic.cycle = cyclic;
  assert.throws(() => flyMachineConfigDigest(cyclic), rejected("invalid_input"));
});

test("lost acquisition ACK never retries or acquires a nonce from matching readback", async () => {
  const f = fixture(); let once = true;
  f.hook = (request, reply) => {
    if (once && request.method === "POST") { once = false; throw new Error(`lost ACK ${NONCE}`); }
    return reply;
  };
  await assert.rejects(f.session.acquire(), rejected("uncertain"));
  assert.equal(f.session.status().state, "uncertain");
  await assert.rejects(f.session.acquire(), rejected("not_current"));
  assert.deepEqual(await f.session.reconcile(), { kind: "fly-machine-lease-reconciliation-v1",
    observation: "matching_challenge_without_acquisition_ack", custodyRestored: false });
  await assert.rejects(f.session.release(), rejected("nonce_unavailable"));
  await assert.rejects(f.session.renew(15), rejected("not_current"));
  assert.equal(f.calls.filter(call => call.method === "POST").length, 1);
  assert.equal(f.calls.filter(call => call.method === "DELETE").length, 0);
  assert.equal(JSON.stringify(f.session).includes(NONCE), false);
});

test("late direct ACK retains cleanup nonce but cannot extend the request-start deadline", async () => {
  const f = fixture({ ttlSeconds: 1 }), blocked = gate();
  f.hook = async (request, reply) => {
    if (request.method === "POST") { blocked.enter(); await blocked.wait; }
    return reply;
  };
  const pending = f.session.acquire(); await blocked.entered;
  f.advance(950);
  assert.equal(f.session.status().state, "uncertain");
  assert.equal(f.session.status().inFlight, "acquire", "expiry does not settle actual IO");
  blocked.release(); await assert.rejects(pending, rejected("uncertain"));
  assert.equal(f.session.status().inFlight, null);
  f.hook = null; await f.session.release();
  assert.equal(f.session.status().state, "uncertain", "cleanup ACK never restores custody");
  assert.equal(f.session.status().releaseAcknowledged, true);
});

test("slow network consumes the one original operation budget across all reads", async () => {
  const f = fixture();
  f.hook = (request, reply) => { f.advance(600); return reply; };
  await assert.rejects(f.session.acquire(), rejected("expired"));
  assert.equal(f.calls.length, 3, "1500 ms budget is not restarted for each roundtrip");
  assert.equal(f.session.status().state, "uncertain");
});

test("wrong Machine, instance, version, config or echoed challenge fences permanently", async () => {
  for (const fault of ["machine", "instance", "version", "config", "lease-version", "challenge", "nonce"]) {
    const f = fixture();
    f.hook = (request, reply) => {
      const result = copy(reply);
      if (request.method === "GET" && !request.path.endsWith("/lease")) {
        if (fault === "machine") result.body.id = "another-machine";
        if (fault === "instance") result.body.instance_id = "other-instance";
        if (fault === "version") result.body.version = "other-version";
        if (fault === "config") result.body.config.services = [{ internal_port: 8080 }];
      }
      if (request.path.endsWith("/lease") && request.method === "GET") {
        if (fault === "lease-version") result.body.data.version = "other-version";
        if (fault === "challenge") result.body.data.description = "another-challenge";
        if (fault === "nonce") result.body.data.nonce = "someone-elses-nonce";
      }
      return result;
    };
    await assert.rejects(f.session.acquire(), error => error instanceof FlyCustodyError, fault);
    assert.equal(f.session.status().state, "uncertain", fault);
    f.hook = null; await assert.rejects(f.session.observe(), rejected("not_current"));
    await assert.rejects(f.session.acquire(), rejected("not_current"));
    assert.equal(f.calls.filter(call => call.method === "POST").length, 1);
  }
});

test("held lease readback changes and backward wall/BOOTTIME readings cannot unfence", async () => {
  for (const fault of ["lease-expiry", "lease-owner", "wall", "boot"]) {
    const f = fixture(); await f.session.acquire();
    if (fault === "lease-expiry") f.lease.expires_at += 1;
    if (fault === "lease-owner") f.lease.owner = "different-owner";
    if (fault === "wall") f.wallStep(-1);
    if (fault === "boot") f.bootStep(-1);
    await assert.rejects(f.session.observe(), error => error instanceof FlyCustodyError);
    assert.equal(f.session.status().state, "uncertain");
    f.advance(1); await assert.rejects(f.session.renew(60), rejected("not_current"));
  }
});

test("explicit renewal is serialized and cannot extend the original session cap or old observations", async () => {
  const f = fixture(), first = await f.session.acquire(); f.advance(1000);
  const next = await f.session.renew(60);
  assert.equal(next.serial, 2); assert.equal(first.serial, 1);
  assert.equal(next.originalEndWallMs, first.originalEndWallMs);
  assert.equal(next.originalEndBoottimeMs, first.originalEndBoottimeMs);
  assert.equal(JSON.parse(f.calls.filter(call => call.method === "POST").at(-1).body).ttl, 60);
  const count = f.calls.length; f.advance(13_900);
  await assert.rejects(f.session.renew(60), rejected("expired"));
  assert.equal(f.calls.length, count); assert.equal(f.session.status().state, "uncertain");
});

test("finite session lifetime supports timely renewals past initial provider TTL and immutable older observations", async () => {
  const f = fixture({ ttlSeconds: 10, sessionLifetimeMs: 180_000 });
  f.advance(5000); // The cap begins at acquire, not at factory construction.
  const origin = f.now(), first = await f.session.acquire(), original = JSON.stringify(first);
  assert.equal(first.originalEndWallMs, origin.wallMs + 9900);
  assert.equal(first.sessionEndWallMs, origin.wallMs + 179_900);
  assert.equal(first.sessionEndBoottimeMs, origin.boottimeMs + 179_900);
  f.advance(9000);
  const second = await f.session.renew(60);
  assert.equal(second.originalEndWallMs, origin.wallMs + 68_900);
  assert.equal(second.sessionEndWallMs, first.sessionEndWallMs);
  assert.equal(second.sessionEndBoottimeMs, first.sessionEndBoottimeMs);
  f.advance(2000); // Past the initial provider lease, under the explicitly renewed lease.
  assert.equal((await f.session.observe()).serial, 2);
  assert.ok(f.now().wallMs > first.originalEndWallMs);
  assert.equal(JSON.stringify(first), original, "a later renewal never changes the older observation");
  f.advance(57_000); await f.session.renew(60); // 68 s, before the second lease's 68.9 s end.
  f.advance(59_000); const final = await f.session.renew(60); // 127 s, before 127.9 s.
  assert.equal(final.originalEndWallMs, first.sessionEndWallMs);
  assert.equal(final.originalEndBoottimeMs, first.sessionEndBoottimeMs);
  assert.equal(final.sessionEndWallMs, first.sessionEndWallMs);
  assert.ok(f.calls.filter(call => call.method === "POST").every(call => JSON.parse(call.body).ttl <= 60));
  f.advance(52_900); // Original 179.9 s cap, even though Fly renewed for 60 s at 127 s.
  const count = f.calls.length;
  await assert.rejects(f.session.renew(60), rejected("expired"));
  assert.equal(f.calls.length, count, "no renewal is sent at or after the original session end");
  assert.equal(f.session.status().state, "uncertain");
  assert.equal(JSON.stringify(first), original);
});

test("session lifetime is a bounded duration, never a saved end or unlimited renewal setting", () => {
  for (const sessionLifetimeMs of [0, -1, 999, 1.5, Infinity, NaN, "180000", 86_400_001]) {
    assert.throws(() => fixture({ sessionLifetimeMs }), rejected("invalid_input"));
  }
});

test("expiry while renewal awaits ACK cannot adopt its later server expiry", async () => {
  for (const late of ["ack", "readback"]) {
    const f = fixture({ ttlSeconds: 1, sessionLifetimeMs: 180_000 });
    const first = await f.session.acquire(); let sent = false;
    assert.ok(first.sessionEndWallMs > first.originalEndWallMs);
    f.hook = (request, reply) => {
      if (request.method === "POST") { sent = true; if (late === "ack") f.advance(950); }
      else if (sent && late === "readback") f.advance(950);
      return reply;
    };
    await assert.rejects(f.session.renew(60), rejected("expired"));
    assert.ok(f.now().wallMs < first.sessionEndWallMs, "old lease expiry independently fences this multi-minute session");
    assert.equal(f.session.status().state, "uncertain");
    f.hook = null; await assert.rejects(f.session.observe(), rejected("not_current"));
    assert.equal(f.calls.filter(call => call.method === "POST").length, 2);
  }
});

test("mutation gateway injects nonce/current_version, retires before dispatch, and never adopts a new generation", async () => {
  const f = fixture(); await f.session.acquire();
  f.hook = (request, reply) => {
    if (request.method === "POST" && !request.path.endsWith("/lease")) assert.equal(f.session.status().state, "retired");
    return reply;
  };
  assert.deepEqual(await f.session.mutate({ action: "update", config: { ...CONFIG, guest: { cpus: 2, memory_mb: 512 } } }),
    { kind: "machine-mutation-http-ack", action: "update" });
  assert.equal(f.session.status().state, "retired");
  await assert.rejects(f.session.observe(), rejected("not_current"));
  await assert.rejects(f.session.renew(15), rejected("not_current"));
  assert.equal((await f.session.reconcile()).observation, "machine_mismatch");
  await f.session.release(); assert.equal(f.session.status().state, "released");
});

test("lost mutation ACK retains actual request charge and requires reconciliation, never retry", async () => {
  const f = fixture(); await f.session.acquire();
  f.hook = (request, reply) => { if (request.path.endsWith("/stop")) throw new Error(`network uncertainty ${NONCE}`); return reply; };
  await assert.rejects(f.session.mutate({ action: "stop" }), rejected("uncertain"));
  await assert.rejects(f.session.mutate({ action: "stop" }), rejected("not_current"));
  assert.equal(f.calls.filter(call => call.path.endsWith("/stop")).length, 1);
  assert.equal((await f.session.reconcile()).observation, "held_nonce");
  assert.equal(f.session.status().state, "uncertain");
});

test("release intent fences an in-flight mutation; renew/observe cannot compete or recover", async () => {
  const f = fixture(), blocked = gate(); await f.session.acquire();
  f.hook = async (request, reply) => { if (request.path.endsWith("/stop")) { blocked.enter(); await blocked.wait; } return reply; };
  const mutation = f.session.mutate({ action: "stop" }); await blocked.entered;
  await assert.rejects(f.session.renew(15), rejected("busy"));
  await assert.rejects(f.session.observe(), rejected("busy"));
  await assert.rejects(f.session.release(), rejected("busy"));
  assert.equal(f.session.status().state, "uncertain"); assert.equal(f.session.status().inFlight, "mutate");
  assert.equal(f.calls.filter(call => call.method === "DELETE").length, 0);
  blocked.release(); await assert.rejects(mutation, rejected("uncertain"));
  f.hook = null; await f.session.release(); assert.equal(f.session.status().releaseAcknowledged, true);
  assert.equal(f.session.status().state, "uncertain");
});

test("release intent also fences pending renewal without treating abort as settlement", async () => {
  const f = fixture(), blocked = gate(); await f.session.acquire();
  f.hook = async (request, reply) => { if (request.method === "POST") { blocked.enter(); await blocked.wait; } return reply; };
  const renewal = f.session.renew(15); await blocked.entered;
  await assert.rejects(f.session.mutate({ action: "stop" }), rejected("busy"));
  await assert.rejects(f.session.release(), rejected("busy"));
  assert.equal(f.calls.at(-1).signal.aborted, true);
  assert.equal(f.session.status().inFlight, "renew");
  blocked.release(); await assert.rejects(renewal, rejected("uncertain"));
  f.hook = null; await f.session.release(); assert.equal(f.session.status().state, "uncertain");
});

test("concurrent releases do not duplicate DELETE or abort the first explicit release", async () => {
  const f = fixture(), blocked = gate(); await f.session.acquire();
  f.hook = async (request, reply) => { if (request.method === "DELETE") { blocked.enter(); await blocked.wait; } return reply; };
  const releasing = f.session.release(); await blocked.entered;
  await assert.rejects(f.session.release(), rejected("busy"));
  await assert.rejects(f.session.renew(15), rejected("busy"));
  assert.equal(f.calls.at(-1).signal.aborted, false);
  blocked.release(); await releasing;
  assert.equal(f.session.status().state, "released");
  assert.equal(f.calls.filter(call => call.method === "DELETE").length, 1);
});

test("lost release ACK is absorbing and is reconciled only as readback data", async () => {
  const f = fixture(); await f.session.acquire();
  f.hook = (request, reply) => { if (request.method === "DELETE") throw new Error(`lost response ${NONCE}`); return reply; };
  await assert.rejects(f.session.release(), rejected("uncertain"));
  assert.equal((await f.session.reconcile()).observation, "lease_absent");
  assert.equal(f.session.status().releaseAcknowledged, false);
  await assert.rejects(f.session.release(), rejected("release_already_attempted"));
  await assert.rejects(f.session.observe(), rejected("not_current"));
  assert.equal(f.calls.filter(call => call.method === "DELETE").length, 1);
});

test("late direct release ACK records cleanup after expiry/fence without succeeding or dropping the actual pending slot", async () => {
  for (const cause of ["expiry-at-return", "observed-expiry", "fence"]) {
    const f = fixture(), blocked = gate(); await f.session.acquire();
    f.hook = async (request, reply) => {
      if (request.method === "DELETE") { blocked.enter(); await blocked.wait; }
      return reply;
    };
    let settled = false;
    const releasing = f.session.release().then(
      () => { settled = true; return { ok: true }; },
      error => { settled = true; return { ok: false, error }; });
    await blocked.entered;
    if (cause === "fence") f.session.fence();
    else f.advance(1500);
    if (cause === "observed-expiry") assert.equal(f.session.status().state, "uncertain");
    const diagnostic = JSON.parse(JSON.stringify(f.session));
    assert.equal(diagnostic.inFlight, "release"); assert.equal(diagnostic.releaseAcknowledged, false);
    assert.equal(settled, false, "deadline/fence cannot settle the held transport promise");
    await assert.rejects(f.session.release(), rejected("busy"));
    await assert.rejects(f.session.renew(15), rejected("busy"));
    assert.equal(f.calls.filter(call => call.method === "DELETE").length, 1);
    blocked.release(); const outcome = await releasing;
    assert.equal(outcome.ok, false, "cleanup knowledge must not repair the failed operation");
    assert.equal(outcome.error.code, cause === "expiry-at-return" ? "expired" : "uncertain");
    assert.equal(f.session.status().state, "uncertain");
    assert.equal(f.session.status().inFlight, null);
    assert.equal(f.session.status().releaseAcknowledged, true, "direct exact success remains a known cleanup fact");
    const count = f.calls.length;
    assert.deepEqual(await f.session.release(), { kind: "machine-lease-release-http-ack" });
    await assert.rejects(f.session.acquire(), rejected("not_current"));
    await assert.rejects(f.session.renew(15), rejected("not_current"));
    await assert.rejects(f.session.mutate({ action: "stop" }), rejected("not_current"));
    assert.equal(f.calls.length, count, "known ACK is local data; no DELETE or other mutation can repeat");
    f.hook = null;
    assert.equal((await f.session.reconcile()).observation, "lease_absent");
    assert.ok(f.calls.slice(count).every(call => call.method === "GET"
      && call.headers["fly-machine-lease-nonce"] === undefined), "known cleanup zeroizes/clears the private nonce");
    assert.equal(f.session.status().state, "uncertain");
  }
});

test("only exact successful lease DELETE ACK records cleanup, including after a fence", async () => {
  const invalid = [
    { status: 201, body: { status: "success", data: { ok: true } } },
    { status: 200, body: { status: "failure", data: { ok: true } } },
    { status: 200, body: { status: "success", data: { ok: "true" } } },
    { status: 200, body: { data: { ok: true } } },
  ];
  for (const reply of invalid) {
    const f = fixture(); await f.session.acquire();
    f.hook = (request, actual) => {
      if (request.method === "DELETE") { f.session.fence(); return reply; }
      return actual;
    };
    await assert.rejects(f.session.release(), rejected("uncertain"));
    assert.equal(f.session.status().releaseAcknowledged, false);
    await assert.rejects(f.session.release(), rejected("release_already_attempted"));
    assert.equal(f.calls.filter(call => call.method === "DELETE").length, 1);
  }
  const f = fixture(); await f.session.acquire();
  f.hook = (request, reply) => request.path.endsWith("/stop")
    ? { status: 200, body: { status: "success", data: { ok: true } } } : reply;
  await f.session.mutate({ action: "stop" });
  assert.equal(f.session.status().releaseAcknowledged, false, "a different operation cannot acknowledge lease cleanup");
  f.hook = null; await f.session.release(); assert.equal(f.session.status().releaseAcknowledged, true);
});

test("nonce, challenge and response secrets never appear in errors/session/request diagnostics", async () => {
  const f = fixture(); await f.session.acquire();
  for (const value of [f.session, ...f.calls, ...f.calls.map(call => call.headers)]) {
    for (const rendered of [JSON.stringify(value), inspect(value)]) {
      assert.equal(rendered.includes(NONCE), false); assert.equal(rendered.includes(EXPECTED.challenge), false);
    }
  }
  f.hook = () => { const error = new FlyCustodyError("rejected"); error.message = NONCE; throw error; };
  await assert.rejects(f.session.observe(), error => {
    assert.equal(error.code, "rejected"); assert.equal(String(error).includes(NONCE), false);
    assert.equal(inspect(error).includes(NONCE), false); return true;
  });
  assert.equal(f.session.status().state, "uncertain");
  const decoded = JSON.parse(JSON.stringify(f.session)); assert.equal(decoded.acquire, undefined);
  assert.throws(() => createFlyMachineLeaseSession({ ...decoded, expected: EXPECTED, ttlSeconds: 15 }), rejected("invalid_input"));
});

test("invalid routes, caller nonce fields and force defaults cannot bypass the gateway", async () => {
  const f = fixture(); await f.session.acquire(); const count = f.calls.length;
  await assert.rejects(f.session.mutate({ action: "../lease" }), rejected("invalid_input"));
  await assert.rejects(f.session.mutate({ action: "destroy" }), rejected("invalid_input"));
  assert.equal(f.calls.length, count);
  await f.session.mutate({ action: "destroy", force: false, headers: { "fly-machine-lease-nonce": "forged" } });
  assert.equal(f.calls.at(-1).headers["fly-machine-lease-nonce"], NONCE);
  assert.ok(f.calls.at(-1).path.endsWith("?force=false"));
  assert.equal(f.session.status().state, "retired");
});
