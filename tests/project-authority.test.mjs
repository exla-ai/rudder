import test from "node:test";
import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { createAuthority, AuthorityError, MonotonicLeaseFence } from "../cloud/dist/project-authority/index.js";
import { canonical, counter, next, MAX_COUNTER } from "../cloud/dist/project-authority/types.js";

const identity = { authorityId: randomUUID(), accountId: "account", projectId: randomUUID(), engineInstanceId: randomUUID(), machineBootId: "boot", ownerEpoch: "2", leaseId: randomUUID() };
const ctx = { accountId: "account", principalId: "user", credentialId: "credential", credentialSha256: "a".repeat(64) };
const config = () => ({ mode: "postgres", authorityId: randomUUID(), connection: { host: "db.internal", port: 5432, database: "authority", user: "runtime", password: "sensitive-do-not-log" } });
const safety = { marginMs: 100, engineRateError: 0, databaseRateError: 0, maxPositiveClockStepMs: 50 };
const grant = (requestId, extra = {}) => ({ ...identity, formatVersion: 1, requestId, renewalSerial: "0", issuedAt: "untrusted-wall-clock", leaseUntil: "untrusted-wall-clock", durationMs: 1000, ...extra });

test("authority defaults inactive without acquiring a connection", async () => {
  const api = createAuthority();
  assert.equal(api.pool, undefined, "raw transaction pool is a JavaScript private field");
  assert.deepEqual(api.stats(), { active: 0, queued: 0 });
  await assert.rejects(api.getReceipt(ctx, randomUUID(), "known"), { code: "inactive" });
  await api.close();
});
test("configuration rejects DSN/SSL overrides and public errors contain no private values", () => {
  const invalid = [ { ...config(), connectionString: "postgres://sensitive-do-not-log@host/db?sslmode=disable" },
    { ...config(), connection: { ...config().connection, ssl: false } },
    { ...config(), connection: { ...config().connection, password: "" } },
    { ...config(), connection: { ...config().connection, host: "/tmp/socket" } },
    { ...config(), maxQueued: 100000 }, { ...config(), maxConnections: 1000 } ];
  for (const value of invalid) assert.throws(() => createAuthority(value), error => {
    assert(error instanceof AuthorityError); assert.equal(error.code, "invalid_request");
    assert(!JSON.stringify(error).includes("sensitive-do-not-log")); assert(!String(error).includes("postgres://")); return true;
  });
});
test("signed counters use canonical decimal strings and fail closed on overflow", () => {
  assert.equal(counter(String(MAX_COUNTER)), String(MAX_COUNTER));
  for (const value of [1, "01", "-1", "1.0", "1e3", String(MAX_COUNTER + 1n), undefined]) assert.throws(() => counter(value), { code: "invalid_request" });
  assert.throws(() => next(String(MAX_COUNTER)), { code: "counter_exhausted" });
  assert.equal(next("9007199254740992"), "9007199254740993");
});
test("canonical operation bytes preserve ordering, reject unsafe/cyclic values and cap size", () => {
  assert.equal(canonical({ b: ["2"], a: "1" }).toString(), '{"a":"1","b":["2"]}');
  const cycle = {}; cycle.next = cycle;
  for (const value of [NaN, Infinity, 1.1, 9007199254740992, undefined, 1n, cycle, new Date()]) assert.throws(() => canonical(value), { code: "invalid_request" });
  assert.throws(() => canonical("x".repeat(100), 10), { code: "invalid_request" });
  const sparse = []; sparse.length = 3;
  const accessor = []; Object.defineProperty(accessor, "0", { get() { throw new Error("must not execute accessor"); }, enumerable: true });
  for (const value of [sparse,accessor,new Array(20000).fill(0),"x".repeat(1024*1024),{ huge: "x".repeat(1024*1024) }]) assert.throws(()=>canonical(value),{code:"invalid_request"});
  assert.throws(()=>canonical({ huge: "x".repeat(100000), later: undefined },100),{code:"invalid_request"});
});
test("lease deadline begins at FIRST send and never trusts response arrival or wall clocks", () => {
  const fence = new MonotonicLeaseFence(identity, safety);
  assert.equal(fence.current(10), false);
  fence.begin("claim", 100); fence.begin("claim", 300);
  assert.equal(fence.observe(grant("claim"), 500), true);
  assert.equal(fence.current(949), true);
  assert.equal(fence.current(950), false);
  assert.throws(() => fence.begin("new", 951), { code: "stale_fence" });
});
test("late renewal cannot revive an expired attempt even if the deadline was not polled", () => {
  const fence = new MonotonicLeaseFence(identity, safety);
  fence.begin("initial", 100); assert(fence.observe(grant("initial"), 101));
  fence.begin("renew", 800);
  assert.equal(fence.observe(grant("renew", { renewalSerial: "1" }), 951), false);
  assert.equal(fence.current(952), false);
});
test("old boot/process/epoch/serial responses never change a live fence", () => {
  for (const mismatch of [{ machineBootId: "old" }, { engineInstanceId: randomUUID() }, { ownerEpoch: "1" }, { leaseId: randomUUID() }]) {
    const fence = new MonotonicLeaseFence(identity, safety); fence.begin("request", 0);
    assert.equal(fence.observe(grant("request", mismatch), 1), false);
  }
  const fence = new MonotonicLeaseFence(identity, safety);
  fence.begin("a", 0); assert(fence.observe(grant("a", { renewalSerial: "1" }), 1));
  fence.begin("b", 10); assert.equal(fence.observe(grant("b", { renewalSerial: "0" }), 11), false);
  assert(fence.current(12)); fence.fence(); assert.equal(fence.current(13), false);
});
test("all observed monotonic regressions and nonfinite/reset clocks permanently fence", () => {
  for (const method of ["begin","observe","current"]) for (const now of [-1,50,NaN,Infinity,-Infinity]) {
    const fence=new MonotonicLeaseFence(identity,safety);fence.begin("initial",100);assert(fence.observe(grant("initial"),101));
    if(method==="begin") assert.throws(()=>fence.begin("renew",now),{code:"stale_fence"});
    else if(method==="observe") assert.equal(fence.observe(grant("unknown"),now),false);
    else assert.equal(fence.current(now),false);
    assert.equal(fence.current(102),false,"a later valid clock cannot repair the fence");
    assert.throws(()=>fence.begin("fresh",103),{code:"stale_fence"});
  }
  const fence=new MonotonicLeaseFence(identity,safety);fence.begin("initial",100);assert(fence.observe(grant("initial"),101));
  assert(fence.current(500));assert.throws(()=>fence.begin("new-request",400),{code:"stale_fence"});
});
test("missing or nonfinite safety inputs cannot create a NaN permanent lease",()=>{
  for(const value of [{},{marginMs:100},{...safety,databaseRateError:NaN},{...safety,marginMs:Infinity}]) assert.throws(()=>new MonotonicLeaseFence(identity,value),{code:"invalid_request"});
});
