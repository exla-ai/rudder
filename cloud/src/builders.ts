import { createHash, createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";

// The burst build pool: per-home builder machines that scale from zero.
//
// A home leases a builder for one build session. The pool reuses a free
// builder (warm caches on its volume), creates one when every builder is busy
// and the home is under its maximum, and otherwise answers busy. A builder
// stops itself after it has been idle for a while, so a pool costs only its
// volumes when nobody is building; volumes unused for the retention period are
// destroyed too. Each builder serves one lease at a time: it is a serialized
// build service, never a writable target shared between concurrent agents.
//
// The control plane holds the Fly token. A home authenticates with its worker
// token and receives a per-builder token; the builder machine only knows that
// token's SHA-256, so neither the Fly config nor this database holds it.

export type BuilderPolicy = {
  enabled: boolean;
  maxPerHome: number;
  cpuKind: string;
  cpus: number;
  memoryMb: number;
  volumeGb: number;
  idleSeconds: number;
  leaseSeconds: number;
  maxLeaseSeconds: number;
  retainDays: number;
  port: number;
};

function intEnv(env: NodeJS.ProcessEnv, name: string, fallback: number, min: number, max: number): number {
  const raw = env[name];
  if (raw === undefined || raw.trim() === "") return fallback;
  const value = Number(raw);
  return Number.isInteger(value) && value >= min && value <= max ? value : fallback;
}

export function builderPolicy(env: NodeJS.ProcessEnv = process.env): BuilderPolicy {
  const cpus = intEnv(env, "RUDDER_BUILDER_CPUS", 16, 1, 16);
  return {
    enabled: env.RUDDER_BUILDERS === "1",
    maxPerHome: intEnv(env, "RUDDER_BUILDERS_MAX_PER_HOME", 2, 1, 8),
    cpuKind: (env.RUDDER_BUILDER_CPU_KIND || "performance").trim(),
    cpus,
    memoryMb: intEnv(env, "RUDDER_BUILDER_MEMORY_MB", 32768, 2048, 131072),
    volumeGb: intEnv(env, "RUDDER_BUILDER_VOLUME_GB", 100, 10, 500),
    idleSeconds: intEnv(env, "RUDDER_BUILDER_IDLE_SECONDS", 300, 60, 3600),
    leaseSeconds: intEnv(env, "RUDDER_BUILDER_LEASE_SECONDS", 1800, 60, 4 * 3600),
    maxLeaseSeconds: intEnv(env, "RUDDER_BUILDER_MAX_LEASE_SECONDS", 4 * 3600, 60, 24 * 3600),
    retainDays: intEnv(env, "RUDDER_BUILDER_RETAIN_DAYS", 7, 1, 90),
    port: intEnv(env, "RUDDER_BUILDER_PORT", 7420, 1024, 65535),
  };
}

export type BuilderMachine = { id: string; state?: string; private_ip?: string };

/** The few Fly Machines calls the pool needs; server.ts implements them. */
export interface BuilderFly {
  appName: string;
  image: string;
  createVolume(input: { name: string; region: string; sizeGb: number }): Promise<{ id: string }>;
  createMachine(input: { name: string; region: string; config: Record<string, unknown> }): Promise<BuilderMachine>;
  /** null when the machine no longer exists. */
  getMachine(id: string): Promise<BuilderMachine | null>;
  startMachine(id: string): Promise<void>;
  stopMachine(id: string): Promise<void>;
  /** Already-gone counts as done. */
  destroyMachine(id: string): Promise<void>;
  /** Already-gone counts as done. */
  destroyVolume(id: string): Promise<void>;
}

/** better-sqlite3 and node:sqlite both fit. */
export interface BuilderDatabase {
  exec(sql: string): unknown;
  prepare(sql: string): {
    run(...params: unknown[]): unknown;
    get(...params: unknown[]): unknown;
    all(...params: unknown[]): unknown[];
  };
}

export type BuilderHome = { id: string; accountId: string; region: string };

export type LeaseGrant = {
  kind: "granted";
  leaseId: string;
  builderId: string;
  url: string;
  token: string;
  expiresAt: string;
  warm: boolean;
  cpus: number;
  memoryMb: number;
};

export type LeaseBusy = { kind: "busy"; retryAfterSeconds: number; builders: number };

export type BuilderView = {
  id: string;
  slot: number;
  status: string;
  machineState: string | null;
  leased: boolean;
  leaseHolder: string | null;
  leaseExpiresAt: string | null;
  lastUsedAt: string;
  error: string | null;
};

export type BuilderSnapshot = { leased: number; idle: number; creating: number; failed: number; destroying: number };

type Row = {
  id: string;
  home_id: string;
  account_id: string;
  slot: number;
  region: string;
  machine_id: string | null;
  volume_id: string | null;
  private_ip: string | null;
  token_salt: string;
  token_sha256: string;
  status: "creating" | "ready" | "failed" | "destroying";
  machine_state: string | null;
  lease_id: string | null;
  lease_holder: string | null;
  lease_expires_at: number | null;
  last_used_at: number;
  error: string | null;
  created_at: number;
  updated_at: number;
};

export class BuilderPoolError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

const START_TIMEOUT_MS = 120_000;
const FAILED_CLEANUP_MS = 60_000;
const STUCK_CREATING_MS = 10 * 60_000;

export function ensureBuilderSchema(db: BuilderDatabase): void {
  db.exec(`
    create table if not exists rudder_builders (
      id text primary key,
      home_id text not null,
      account_id text not null,
      slot integer not null,
      region text not null,
      machine_id text,
      volume_id text,
      private_ip text,
      token_salt text not null,
      token_sha256 text not null,
      status text not null,
      machine_state text,
      lease_id text,
      lease_holder text,
      lease_expires_at integer,
      last_used_at integer not null,
      error text,
      created_at integer not null,
      updated_at integer not null,
      unique (home_id, slot)
    );
    create index if not exists rudder_builders_home on rudder_builders (home_id);
  `);
}

export function builderToken(key: string, builderId: string, salt: string): string {
  return `rdrb_${createHmac("sha256", key).update(`rudder-builder-v1\0${builderId}\0${salt}`).digest("base64url")}`;
}

export function sha256Hex(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

/** Constant-time check of a presented builder token against its hash. */
export function builderTokenMatches(presented: string, expectedSha256: string): boolean {
  const actual = Buffer.from(sha256Hex(presented), "hex");
  const expected = Buffer.from(expectedSha256, "hex");
  return actual.length === expected.length && expected.length === 32 && timingSafeEqual(actual, expected);
}

export function builderVolumeName(homeId: string, slot: number): string {
  const tail = `_${slot}`;
  return `rdb_${homeId.replace(/[^a-zA-Z0-9]/g, "_")}`.slice(0, 30 - tail.length) + tail;
}

export function builderMachineConfig(policy: BuilderPolicy, image: string, row: { id: string; home_id: string; volume_id: string; token_sha256: string }): Record<string, unknown> {
  return {
    image,
    env: {
      RUDDER_BUILDER: "1",
      RUDDER_BUILDER_ID: row.id,
      RUDDER_BUILDER_HOME_ID: row.home_id,
      RUDDER_BUILDER_TOKEN_SHA256: row.token_sha256,
      RUDDER_BUILDER_PORT: String(policy.port),
      RUDDER_BUILDER_IDLE_SECONDS: String(policy.idleSeconds),
      RUDDER_CACHE_DIR: "/workspace/cache/build",
      RUDDER_WORKER_SKIP_AGENT_UPDATE: "1",
    },
    guest: { cpu_kind: policy.cpuKind, cpus: policy.cpus, memory_mb: policy.memoryMb },
    // The builder exits when idle; "no" lets that stop the machine.
    restart: { policy: "no" },
    auto_destroy: false,
    mounts: [{ volume: row.volume_id, path: "/workspace" }],
    metadata: { "rudder-builder": row.id, "rudder-home": row.home_id },
  };
}

function message(error: unknown): string {
  return (error instanceof Error ? error.message : String(error)).slice(0, 500);
}

export function createBuilderPool({
  db,
  fly,
  policy,
  key,
  homeExists = () => true,
  persist = () => {},
  now = () => Date.now(),
  sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)),
  log = () => {},
}: {
  db: BuilderDatabase;
  fly: BuilderFly;
  policy: BuilderPolicy;
  /** HMAC key for builder tokens. */
  key: string;
  homeExists?: (homeId: string) => boolean;
  persist?: () => void;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
  log?: (line: string) => void;
}) {
  if (!key) throw new Error("a builder token key is required");
  ensureBuilderSchema(db);
  const q = {
    forHome: db.prepare("select * from rudder_builders where home_id = ? order by slot"),
    all: db.prepare("select * from rudder_builders order by home_id, slot"),
    byId: db.prepare("select * from rudder_builders where id = ?"),
    byLease: db.prepare("select * from rudder_builders where home_id = ? and lease_id = ?"),
    insert: db.prepare(`insert into rudder_builders (
      id, home_id, account_id, slot, region, token_salt, token_sha256, status,
      lease_id, lease_holder, lease_expires_at, last_used_at, created_at, updated_at
    ) values (
      @id, @home_id, @account_id, @slot, @region, @token_salt, @token_sha256, 'creating',
      @lease_id, @lease_holder, @lease_expires_at, @now, @now, @now
    )`),
    claim: db.prepare(`update rudder_builders
      set lease_id = @lease_id, lease_holder = @lease_holder, lease_expires_at = @lease_expires_at, error = null, updated_at = @now
      where id = @id and lease_id is null and status = 'ready'`),
    renew: db.prepare("update rudder_builders set lease_expires_at = @lease_expires_at, updated_at = @now where home_id = @home_id and lease_id = @lease_id"),
    release: db.prepare(`update rudder_builders
      set lease_id = null, lease_holder = null, lease_expires_at = null, last_used_at = @now, updated_at = @now
      where home_id = @home_id and lease_id = @lease_id`),
    expire: db.prepare(`update rudder_builders
      set lease_id = null, lease_holder = null, lease_expires_at = null, last_used_at = @now, updated_at = @now
      where lease_id is not null and lease_expires_at < @now`),
    machine: db.prepare(`update rudder_builders
      set machine_id = @machine_id, private_ip = @private_ip, machine_state = @machine_state, updated_at = @now
      where id = @id`),
    volume: db.prepare("update rudder_builders set volume_id = @volume_id, updated_at = @now where id = @id"),
    status: db.prepare("update rudder_builders set status = @status, error = @error, updated_at = @now where id = @id"),
    token: db.prepare("update rudder_builders set token_sha256 = @token_sha256, updated_at = @now where id = @id"),
    clearLease: db.prepare(`update rudder_builders
      set lease_id = null, lease_holder = null, lease_expires_at = null, error = @error, updated_at = @now
      where id = @id`),
    remove: db.prepare("delete from rudder_builders where id = ?"),
  };
  const get = (id: string) => q.byId.get(id) as Row | undefined;
  const iso = (ms: number) => new Date(ms).toISOString();
  const urlFor = (row: Row) => {
    const host = row.private_ip ? `[${row.private_ip}]` : `${row.machine_id}.vm.${fly.appName}.internal`;
    return `http://${host}:${policy.port}`;
  };

  function expireLeases(): void {
    q.expire.run({ now: now() });
  }

  function remember(row: Row, machine: BuilderMachine): void {
    row.machine_id = machine.id;
    row.private_ip = machine.private_ip || row.private_ip;
    row.machine_state = machine.state || null;
    q.machine.run({ id: row.id, machine_id: row.machine_id, private_ip: row.private_ip, machine_state: row.machine_state, now: now() });
  }

  async function createMachine(row: Row): Promise<void> {
    const config = builderMachineConfig(policy, fly.image, { id: row.id, home_id: row.home_id, volume_id: row.volume_id!, token_sha256: row.token_sha256 });
    const suffix = randomBytes(3).toString("hex");
    const name = `rudder-build-${row.home_id}-${row.slot}`.toLowerCase().replace(/[^a-z0-9-]+/g, "-").slice(0, 55) + `-${suffix}`;
    remember(row, await fly.createMachine({ name, region: row.region, config }));
  }

  /** Bring a claimed builder to "started". Returns whether it was already running. */
  async function ensureRunning(row: Row): Promise<boolean> {
    const token = builderToken(key, row.id, row.token_salt);
    // A machine made by this call is a cold start whatever state Fly reports.
    let wasRunning = true;
    if (row.status === "creating") {
      if (!row.volume_id) {
        const volume = await fly.createVolume({ name: builderVolumeName(row.home_id, row.slot), region: row.region, sizeGb: policy.volumeGb });
        row.volume_id = volume.id;
        q.volume.run({ id: row.id, volume_id: volume.id, now: now() });
      }
      await createMachine(row);
      wasRunning = false;
      row.status = "ready";
      q.status.run({ id: row.id, status: "ready", error: null, now: now() });
      persist();
    } else if (sha256Hex(token) !== row.token_sha256 || !row.machine_id || !(await fly.getMachine(row.machine_id))) {
      // The token key changed or the machine is gone: a fresh machine on the
      // same volume keeps the caches and picks up the current token hash.
      if (row.machine_id) await fly.destroyMachine(row.machine_id);
      row.token_sha256 = sha256Hex(token);
      q.token.run({ id: row.id, token_sha256: row.token_sha256, now: now() });
      await createMachine(row);
      wasRunning = false;
      persist();
    }
    const deadline = now() + START_TIMEOUT_MS;
    for (;;) {
      const machine = await fly.getMachine(row.machine_id!);
      if (!machine) throw new BuilderPoolError("builder machine disappeared while starting", 503);
      remember(row, machine);
      if (machine.state === "started") return wasRunning;
      wasRunning = false;
      if (machine.state === "stopped" || machine.state === "created" || machine.state === "suspended") {
        await fly.startMachine(machine.id);
      } else if (machine.state === "failed" || machine.state === "destroyed" || machine.state === "destroying") {
        throw new BuilderPoolError(`builder machine is ${machine.state}`, 503);
      }
      if (now() > deadline) throw new BuilderPoolError("builder did not start in time", 503);
      await sleep(1000);
    }
  }

  async function lease(home: BuilderHome, options: { holder?: string; seconds?: number } = {}): Promise<LeaseGrant | LeaseBusy> {
    if (!policy.enabled) throw new BuilderPoolError("the build pool is not enabled", 404);
    expireLeases();
    const seconds = Math.min(policy.maxLeaseSeconds, Math.max(60, Math.round(options.seconds ?? policy.leaseSeconds)));
    const holder = (options.holder || "").replace(/[^\x20-\x7e]/g, "").slice(0, 120) || null;
    const leaseId = randomUUID();
    const at = now();
    const leaseFields = { lease_id: leaseId, lease_holder: holder, lease_expires_at: at + seconds * 1000, now: at };
    const rows = q.forHome.all(home.id) as Row[];
    // Warm first (already started), then the most recently used caches.
    const free = rows
      .filter((row) => row.status === "ready" && !row.lease_id)
      .sort((a, b) => Number(b.machine_state === "started") - Number(a.machine_state === "started") || b.last_used_at - a.last_used_at);
    let row: Row | undefined;
    for (const candidate of free) {
      if ((q.claim.run({ id: candidate.id, ...leaseFields }) as { changes: number }).changes === 1) {
        row = get(candidate.id);
        break;
      }
    }
    if (!row) {
      if (rows.length >= policy.maxPerHome) {
        const expiries = rows.map((r) => r.lease_expires_at).filter((t): t is number => typeof t === "number");
        const soonest = expiries.length ? Math.ceil((Math.min(...expiries) - at) / 1000) : 30;
        return { kind: "busy", retryAfterSeconds: Math.min(300, Math.max(5, soonest)), builders: rows.length };
      }
      const used = new Set(rows.map((r) => r.slot));
      let slot = 0;
      while (used.has(slot)) slot += 1;
      const id = `bld-${randomBytes(6).toString("hex")}`;
      const salt = randomBytes(16).toString("base64url");
      q.insert.run({
        id,
        home_id: home.id,
        account_id: home.accountId,
        slot,
        region: home.region,
        token_salt: salt,
        token_sha256: sha256Hex(builderToken(key, id, salt)),
        ...leaseFields,
      });
      row = get(id)!;
      log(`[builders] ${home.id} scaling out to ${rows.length + 1} (slot ${slot})`);
    }
    persist();
    try {
      const wasRunning = await ensureRunning(row);
      const fresh = get(row.id)!;
      if (fresh.lease_id !== leaseId) throw new BuilderPoolError("the lease expired while the builder started", 409);
      return {
        kind: "granted",
        leaseId,
        builderId: row.id,
        url: urlFor(fresh),
        token: builderToken(key, row.id, row.token_salt),
        expiresAt: iso(fresh.lease_expires_at!),
        warm: wasRunning,
        cpus: policy.cpus,
        memoryMb: policy.memoryMb,
      };
    } catch (error) {
      const reason = message(error);
      log(`[builders] ${home.id} ${row.id} failed to start: ${reason}`);
      // A builder that never finished creating is torn down by the sweep; a
      // ready one keeps its volume (the cache) and is retried next lease.
      if (get(row.id)?.status === "creating") q.status.run({ id: row.id, status: "failed", error: reason, now: now() });
      q.clearLease.run({ id: row.id, error: reason, now: now() });
      persist();
      throw error instanceof BuilderPoolError ? error : new BuilderPoolError(`could not start a builder: ${reason}`, 503);
    }
  }

  function renew(homeId: string, leaseId: string, seconds?: number): { expiresAt: string } | null {
    expireLeases();
    const extend = Math.min(policy.maxLeaseSeconds, Math.max(60, Math.round(seconds ?? policy.leaseSeconds)));
    const at = now();
    const result = q.renew.run({ home_id: homeId, lease_id: leaseId, lease_expires_at: at + extend * 1000, now: at }) as { changes: number };
    return result.changes === 1 ? { expiresAt: iso(at + extend * 1000) } : null;
  }

  function release(homeId: string, leaseId: string): boolean {
    const released = (q.release.run({ home_id: homeId, lease_id: leaseId, now: now() }) as { changes: number }).changes === 1;
    if (released) persist();
    return released;
  }

  function list(homeId: string): BuilderView[] {
    expireLeases();
    return (q.forHome.all(homeId) as Row[]).map((row) => ({
      id: row.id,
      slot: row.slot,
      status: row.status,
      machineState: row.machine_state,
      leased: row.lease_id !== null,
      leaseHolder: row.lease_holder,
      leaseExpiresAt: row.lease_expires_at === null ? null : iso(row.lease_expires_at),
      lastUsedAt: iso(row.last_used_at),
      error: row.error,
    }));
  }

  function snapshot(): BuilderSnapshot {
    const counts: BuilderSnapshot = { leased: 0, idle: 0, creating: 0, failed: 0, destroying: 0 };
    for (const row of q.all.all() as Row[]) {
      if (row.status === "ready") counts[row.lease_id ? "leased" : "idle"] += 1;
      else if (row.status === "creating") counts.creating += 1;
      else counts[row.status] += 1;
    }
    return counts;
  }

  function markDestroying(row: Row, reason: string): void {
    q.status.run({ id: row.id, status: "destroying", error: reason, now: now() });
    log(`[builders] ${row.home_id} ${row.id} destroying: ${reason}`);
  }

  async function finishDestroy(row: Row): Promise<void> {
    if (row.machine_id) await fly.destroyMachine(row.machine_id);
    if (row.volume_id) await fly.destroyVolume(row.volume_id);
    q.remove.run(row.id);
  }

  /** Mark every builder of a deleted home for teardown, then tear down. */
  async function destroyForHome(homeId: string): Promise<void> {
    for (const row of q.forHome.all(homeId) as Row[]) {
      if (row.status !== "destroying") markDestroying(row, "home deleted");
    }
    persist();
    await sweep();
  }

  let sweeping: Promise<void> | null = null;
  /** Expire leases, stop wedged idle builders and destroy what is no longer needed. */
  function sweep(): Promise<void> {
    // .finally runs as a microtask, after the assignment; a try/finally inside
    // the async body would clear `sweeping` before it is set when nothing awaits.
    sweeping ??= runSweep().finally(() => {
      sweeping = null;
    });
    return sweeping;
  }

  async function runSweep(): Promise<void> {
    expireLeases();
    const at = now();
    for (const listed of q.all.all() as Row[]) {
      const row = get(listed.id);
      if (!row || row.lease_id) continue;
      try {
        if (row.status !== "destroying") {
          if (!homeExists(row.home_id)) markDestroying(row, "home is gone");
          else if (row.status === "failed" && at - row.updated_at > FAILED_CLEANUP_MS) markDestroying(row, row.error || "creation failed");
          else if (row.status === "creating" && at - row.updated_at > STUCK_CREATING_MS) markDestroying(row, "creation never finished");
          else if (row.status === "ready" && at - row.last_used_at > policy.retainDays * 86_400_000) markDestroying(row, `unused for ${policy.retainDays} days`);
        }
        const current = get(row.id);
        if (!current || current.lease_id) continue;
        if (current.status === "destroying") {
          await finishDestroy(current);
          persist();
          continue;
        }
        // Builders stop themselves when idle; this is the backstop for one
        // whose process wedged. Recheck the lease after every await.
        if (current.status === "ready" && current.machine_id && current.machine_state !== "stopped"
          && at - current.last_used_at > (policy.idleSeconds + 600) * 1000) {
          const machine = await fly.getMachine(current.machine_id);
          if (!machine) {
            q.machine.run({ id: current.id, machine_id: null, private_ip: null, machine_state: null, now: now() });
          } else if (machine.state === "started" && !get(current.id)?.lease_id) {
            log(`[builders] ${current.home_id} ${current.id} still running while idle; stopping it`);
            await fly.stopMachine(machine.id);
            q.machine.run({ id: current.id, machine_id: machine.id, private_ip: machine.private_ip || current.private_ip, machine_state: "stopping", now: now() });
          } else {
            remember(current, machine);
          }
        }
      } catch (error) {
        log(`[builders] sweep ${row.id}: ${message(error)}`);
      }
    }
  }

  return { policy, lease, renew, release, list, snapshot, sweep, destroyForHome };
}

export type BuilderPool = ReturnType<typeof createBuilderPool>;
