// The burst build pool (cloud/src/builders.ts, built to cloud/dist) against
// node:sqlite and a fake Fly: scale out, reuse, busy, expiry, sweep, teardown.
import { test as nodeTest } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import { DatabaseSync } from "node:sqlite";

const built = new URL("../cloud/dist/builders.js", import.meta.url);
const lib = fs.existsSync(built) ? await import(built.href) : null;
const test = lib ? nodeTest : (name, fn) => nodeTest(name, { skip: "cloud not built" }, fn);

function fakeFly({ failStart = false } = {}) {
  const fly = {
    appName: "rudder-workers-test",
    image: "registry.fly.io/rudder-workers-test:4.0.0-alpha.34",
    machines: new Map(),
    volumes: new Map(),
    calls: [],
    async createVolume({ name, region, sizeGb }) {
      const id = `vol_${fly.volumes.size + 1}`;
      fly.volumes.set(id, { name, region, sizeGb });
      fly.calls.push(`createVolume ${name}`);
      return { id };
    },
    async createMachine({ name, region, config }) {
      const id = `m${fly.machines.size + 1}${Math.random().toString(16).slice(2, 6)}`;
      fly.machines.set(id, { id, name, region, config, state: "created", private_ip: `fdaa::${fly.machines.size + 1}` });
      fly.calls.push(`createMachine ${config.mounts[0].volume}`);
      return { id, state: "created", private_ip: fly.machines.get(id).private_ip };
    },
    async getMachine(id) {
      const machine = fly.machines.get(id);
      if (!machine) return null;
      // A created or starting machine is started on the next look.
      if (machine.state === "created" || machine.state === "starting") machine.state = failStart ? "failed" : "started";
      return { id, state: machine.state, private_ip: machine.private_ip };
    },
    async startMachine(id) {
      fly.calls.push(`start ${id}`);
      fly.machines.get(id).state = "starting";
    },
    async stopMachine(id) {
      fly.calls.push(`stop ${id}`);
      fly.machines.get(id).state = "stopped";
    },
    async destroyMachine(id) {
      fly.calls.push(`destroyMachine ${id}`);
      fly.machines.delete(id);
    },
    async destroyVolume(id) {
      fly.calls.push(`destroyVolume ${id}`);
      fly.volumes.delete(id);
    },
  };
  return fly;
}

function setup({ env = {}, fly = fakeFly(), key = "builder-key-0123456789", homes = new Set(["home-a"]) } = {}) {
  const db = new DatabaseSync(":memory:");
  let clock = Date.parse("2026-11-03T12:00:00Z");
  const policy = lib.builderPolicy({ RUDDER_BUILDERS: "1", ...env });
  const make = (k = key) => lib.createBuilderPool({
    db,
    fly,
    policy,
    key: k,
    homeExists: (id) => homes.has(id),
    now: () => clock,
    sleep: async () => {},
  });
  return { db, fly, policy, pool: make(), make, homes, advance: (ms) => { clock += ms; }, now: () => clock };
}

const home = { id: "home-a", accountId: "acct-1", region: "iad" };

test("policy defaults to a 2 x 16 CPU pool and is off unless asked for", () => {
  const off = lib.builderPolicy({});
  assert.equal(off.enabled, false);
  const on = lib.builderPolicy({ RUDDER_BUILDERS: "1", RUDDER_BUILDER_CPUS: "8", RUDDER_BUILDERS_MAX_PER_HOME: "nonsense" });
  assert.equal(on.enabled, true);
  assert.equal(on.cpus, 8);
  assert.equal(on.maxPerHome, 2, "an unparseable value falls back to the default");
  assert.equal(on.memoryMb, 32768);
});

test("a disabled pool refuses leases with 404", async () => {
  const { db, fly } = setup();
  const pool = lib.createBuilderPool({ db, fly, policy: lib.builderPolicy({}), key: "k" });
  await assert.rejects(pool.lease(home), (error) => error.status === 404);
});

test("the first lease creates a volume and machine; the token is never stored", async () => {
  const { pool, fly, db, policy } = setup();
  const grant = await pool.lease(home, { holder: "rudder: cargo build", seconds: 600 });
  assert.equal(grant.kind, "granted");
  assert.equal(grant.warm, false);
  assert.match(grant.token, /^rdrb_/);
  assert.match(grant.url, /^http:\/\/\[fdaa::1\]:7420$/);
  assert.deepEqual(fly.calls.slice(0, 2), ["createVolume rdb_home_a_0", "createMachine vol_1"]);
  const machine = [...fly.machines.values()][0];
  assert.equal(machine.config.env.RUDDER_BUILDER, "1");
  assert.equal(machine.config.env.RUDDER_BUILDER_TOKEN_SHA256, lib.sha256Hex(grant.token));
  assert.ok(lib.builderTokenMatches(grant.token, machine.config.env.RUDDER_BUILDER_TOKEN_SHA256));
  assert.ok(!lib.builderTokenMatches(`${grant.token}x`, machine.config.env.RUDDER_BUILDER_TOKEN_SHA256));
  assert.deepEqual(machine.config.guest, { cpu_kind: "performance", cpus: policy.cpus, memory_mb: policy.memoryMb });
  assert.deepEqual(machine.config.restart, { policy: "no" });
  assert.equal(machine.config.services, undefined, "builders are private-network only");
  const dump = JSON.stringify(db.prepare("select * from rudder_builders").all());
  assert.ok(!dump.includes(grant.token), "the database never holds a builder token");
  assert.ok(!JSON.stringify(machine.config).includes(grant.token), "neither does the Fly config");
  const [view] = pool.list(home.id);
  assert.equal(view.leased, true);
  assert.equal(view.leaseHolder, "rudder: cargo build");
  assert.equal(view.token, undefined);
});

test("scales out while busy, answers busy at the maximum, then reuses warm builders", async () => {
  const { pool, fly, advance } = setup();
  const first = await pool.lease(home);
  const second = await pool.lease(home);
  assert.notEqual(first.builderId, second.builderId, "a busy builder is never shared");
  assert.equal(fly.volumes.size, 2);
  const busy = await pool.lease(home);
  assert.equal(busy.kind, "busy");
  assert.equal(busy.builders, 2);
  assert.ok(busy.retryAfterSeconds >= 5 && busy.retryAfterSeconds <= 300);
  assert.deepEqual(pool.snapshot(), { leased: 2, idle: 0, creating: 0, failed: 0, destroying: 0 });

  assert.equal(pool.release(home.id, first.leaseId), true);
  assert.equal(pool.release(home.id, first.leaseId), false, "a lease releases once");
  advance(1000);
  const again = await pool.lease(home);
  assert.equal(again.builderId, first.builderId);
  assert.equal(again.warm, true, "the machine was still running");
  assert.equal(fly.volumes.size, 2, "no new builder for a free one");
  assert.equal(again.token, first.token, "same builder, same token");
});

test("a stopped builder is started again on its old volume", async () => {
  const { pool, fly } = setup();
  const grant = await pool.lease(home);
  pool.release(home.id, grant.leaseId);
  const machine = [...fly.machines.values()][0];
  machine.state = "stopped";
  const next = await pool.lease(home);
  assert.equal(next.builderId, grant.builderId);
  assert.equal(next.warm, false);
  assert.ok(fly.calls.includes(`start ${machine.id}`));
});

test("renew extends a live lease; an expired lease frees the builder", async () => {
  const { pool, advance, now } = setup();
  const grant = await pool.lease(home, { seconds: 120 });
  advance(100_000);
  const renewed = pool.renew(home.id, grant.leaseId, 120);
  assert.equal(renewed.expiresAt, new Date(now() + 120_000).toISOString());
  assert.equal(pool.renew(home.id, "not-a-lease"), null);
  advance(121_000);
  assert.equal(pool.renew(home.id, grant.leaseId), null, "expired");
  assert.equal(pool.list(home.id)[0].leased, false);
});

test("a builder that never finished creating is torn down by the sweep", async () => {
  const fly = fakeFly({ failStart: true });
  const { pool, advance } = setup({ fly });
  await assert.rejects(pool.lease(home), (error) => error.status === 503);
  assert.equal(pool.list(home.id)[0].status, "ready", "creation finished; the start failed, so the volume (cache) is kept");
  assert.equal(pool.list(home.id)[0].leased, false);

  // Creation itself failing leaves a failed row the sweep removes.
  const broken = fakeFly();
  broken.createMachine = async () => { throw new Error("capacity"); };
  const second = setup({ fly: broken });
  await assert.rejects(second.pool.lease(home), /capacity/);
  assert.equal(second.pool.list(home.id)[0].status, "failed");
  second.advance(61_000);
  await second.pool.sweep();
  assert.equal(second.pool.list(home.id).length, 0);
  assert.ok(broken.calls.includes("destroyVolume vol_1"));
  advance(0);
});

test("the sweep stops a wedged idle builder, then destroys unused and orphaned ones", async () => {
  const { pool, fly, advance, homes } = setup();
  const grant = await pool.lease(home);
  pool.release(home.id, grant.leaseId);
  const machine = [...fly.machines.values()][0];
  advance(5 * 60_000);
  await pool.sweep();
  assert.equal(machine.state, "started", "not yet: the builder stops itself first");
  advance(11 * 60_000);
  await pool.sweep();
  assert.equal(machine.state, "stopped");
  assert.ok(fly.calls.includes(`stop ${machine.id}`));

  advance(8 * 86_400_000);
  await pool.sweep();
  assert.equal(pool.list(home.id).length, 0, "unused for the retention period");
  assert.equal(fly.volumes.size, 0);

  const other = { id: "home-b", accountId: "acct-2", region: "iad" };
  homes.add("home-b");
  const leased = await pool.lease(other);
  homes.delete("home-b");
  await pool.sweep();
  assert.equal(pool.list("home-b").length, 1, "a leased builder is never torn down");
  pool.release("home-b", leased.leaseId);
  await pool.sweep();
  assert.equal(pool.list("home-b").length, 0, "its home is gone");
});

test("destroyForHome tears down every builder of a deleted home", async () => {
  const { pool, fly } = setup();
  const a = await pool.lease(home);
  const b = await pool.lease(home);
  pool.release(home.id, a.leaseId);
  pool.release(home.id, b.leaseId);
  await pool.destroyForHome(home.id);
  assert.equal(pool.list(home.id).length, 0);
  assert.equal(fly.machines.size, 0);
  assert.equal(fly.volumes.size, 0);
});

test("a rotated token key recreates the machine on the same volume", async () => {
  const { pool, fly, make } = setup();
  const grant = await pool.lease(home);
  pool.release(home.id, grant.leaseId);
  const rotated = make("a-different-key-9876543210");
  const next = await rotated.lease(home);
  assert.notEqual(next.token, grant.token);
  assert.equal(fly.volumes.size, 1, "the cache volume is kept");
  const [machine] = [...fly.machines.values()];
  assert.equal(machine.config.mounts[0].volume, "vol_1");
  assert.equal(machine.config.env.RUDDER_BUILDER_TOKEN_SHA256, lib.sha256Hex(next.token));
});

test("volume names fit Fly's 30-character limit", () => {
  assert.equal(lib.builderVolumeName("home-53c2", 1), "rdb_home_53c2_1");
  const long = lib.builderVolumeName("home-with-a-very-long-identifier-indeed", 7);
  assert.ok(long.length <= 30);
  assert.ok(long.endsWith("_7"));
  assert.match(long, /^[a-zA-Z0-9_]+$/);
});
