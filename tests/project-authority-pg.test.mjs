import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { createRequire } from "node:module";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { qualificationPool } from "../cloud/dist/project-authority/qualification.js";
import { AuthorityError } from "../cloud/dist/project-authority/index.js";

const require = createRequire(new URL("../cloud/package.json", import.meta.url));
const pg = require("pg");
const exec = promisify(execFile);
const hash = value => createHash("sha256").update(value).digest("hex");
const stateFile = process.env.RUDDER_AUTHORITY_PG_TEST_STATE;

test("real Node authority transactions, auth/fence/CAS races, crash/reply loss and bounded settlement", { skip: !stateFile, timeout: 150000 }, async t => {
  const state = JSON.parse(await fs.readFile(stateFile, "utf8"));
  assert.match(state.nonce, /^[0-9a-f-]{36}$/);
  assert.equal(path.dirname(state.socket), `/var/tmp/rudder-authority-node-${state.nonce}`);
  assert.equal(state.data, path.dirname(state.socket) + "/data");
  assert.equal((await fs.readFile(path.dirname(state.socket) + "/qualification-nonce", "utf8")).trim(), state.nonce);
  const authorityId = randomUUID();
  const S = "rudder_project_authority";
  const base = { nonce: state.nonce, socket: state.socket, database: "postgres", authorityId };
  const settings = { host: state.socket, port: 55432, database: "postgres", user: "postgres", password: "unused", application_name: "qualification-administrator" };
  const connect = async () => { const c = new pg.Client(settings); c.on("error", () => {}); await c.connect(); return c; };
  let admin = await connect();
  const migration = await qualificationPool({ ...base, user: "postgres", role: "migration", requestMs: 10000, statementMs: 9000, lockMs: 1000 });
  const resources = [migration];
  t.after(async () => { for (const resource of resources) await resource.pool.close(); await admin.end().catch(() => {}); });
  await admin.query("CREATE ROLE rudder_authority_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS");
  const committed = outcome => { assert.equal(outcome.status, "committed"); return outcome.value; };
  await assert.rejects(migration.migrate("postgres"),{code:"role_rejected"});
  assert.equal((await admin.query("SELECT to_regnamespace($1)::text AS schema",[S])).rows[0].schema,null,"rejected migration rolls back every schema object");
  await admin.query("CREATE ROLE unsafe_authority_login LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS; GRANT CREATE ON SCHEMA public TO unsafe_authority_login");
  await assert.rejects(migration.migrate("unsafe_authority_login"),{code:"role_rejected"});
  assert.equal((await admin.query("SELECT to_regnamespace($1)::text AS schema",[S])).rows[0].schema,null,"DDL-capable runtime login is rejected atomically");
  assert.equal(committed(await migration.migrate("rudder_authority_runtime")).state, "frozen");
  assert.equal(committed(await migration.migrate("rudder_authority_runtime")).state, "existing");
  assert.equal((await admin.query(`SELECT state FROM ${S}.authorities`)).rows[0].state, "frozen");
  const runtime = await qualificationPool({ ...base, user: "rudder_authority_runtime", role: "runtime" }); resources.push(runtime);
  const api = runtime.authority;
  const accountId = "tenant-a";
  const ctx = { accountId, principalId: "owner", credentialId: "user-key", credentialSha256: hash("synthetic-user") };
  const operator = { accountId, principalId: "operator", credentialId: "operator-key", credentialSha256: hash("synthetic-operator") };
  const engines = { a: randomUUID(), b: randomUUID(), newer: randomUUID() };
  const engineContext = (label = "a", boot = label === "b" ? "boot-b" : "boot-a") => ({ ...ctx,
    engine: { instanceId: engines[label], bootId: boot, credentialId: "engine-" + label, credentialSha256: hash("synthetic-engine-" + label) } });
  for (const [id, principal, kind, token] of [["user-key", "owner", "user", "synthetic-user"], ["operator-key", "operator", "service", "synthetic-operator"],
    ...Object.keys(engines).map(label => ["engine-" + label, "engine-" + label, "engine", "synthetic-engine-" + label])]) {
    await admin.query(`INSERT INTO ${S}.authority_credentials(account_id,credential_id,principal_id,kind,token_sha256,expires_at) VALUES($1,$2,$3,$4,$5,clock_timestamp()+interval '1 day')`, [accountId, id, principal, kind, hash(token)]);
  }
  for (const [label, instance] of Object.entries(engines)) await admin.query(`INSERT INTO ${S}.engine_instances(account_id,engine_instance_id,machine_id,machine_installation_id,machine_boot_id,protocol_version,authenticated_key_id)
    VALUES($1,$2,'isolated-disposable',$3,$4,1,$5)`, [accountId, instance, randomUUID(), label === "b" ? "boot-b" : "boot-a", "engine-" + label]);
  await admin.query(`INSERT INTO ${S}.account_principals VALUES($1,$2,'owner','["project:bootstrap"]',NULL,0),($1,$2,'operator','["retention:admit","handoff:admit","verification:admit"]',NULL,0)`, [authorityId, accountId]);
  await t.test("frozen incarnation and privileged runtime role are refused", async () => {
    await assert.rejects(api.admitRetainedRoot(operator, { rootId: "blocked", kind: "source", digest: "a".repeat(64), sizeBytes: "1", retentionReceipt: {} }), { code: "inactive" });
    const unsafe = await qualificationPool({ ...base, user: "postgres", role: "runtime" }); resources.push(unsafe);
    await assert.rejects(unsafe.authority.getReceipt(ctx, randomUUID(), "key"), { code: "role_rejected" });
    await admin.query(`UPDATE ${S}.authorities SET state='ready',protocol_version=2`);
    await assert.rejects(api.admitRetainedRoot(operator, {rootId:"incompatible",kind:"source",digest:"a".repeat(64),sizeBytes:"1",retentionReceipt:{}}),{code:"inactive"});
    await admin.query(`UPDATE ${S}.authorities SET state='frozen',protocol_version=1`);
  });
  // Only this isolated administrator activates a test incarnation. The library has no activation API.
  await admin.query(`UPDATE ${S}.authorities SET state='ready' WHERE authority_id=$1`, [authorityId]);
  for (const [rootId, kind] of [["source-a", "source"], ["source-b", "source"], ["env", "environment"], ["handoff", "handoff"], ["gate", "gate-evidence"], ["contract", "build-contract"]]) {
    committed(await api.admitRetainedRoot(operator, { rootId, kind, digest: hash(rootId), sizeBytes: "18446744073709551615", retentionReceipt: { syntheticVerifiedPin: rootId } }));
  }
  async function witness(projectId, epoch = "0", instance = null) {
    const witnessId = randomUUID();
    committed(await api.admitHandoff(operator, { projectId, witnessId, rootId: "handoff", previousOwnerEpoch: epoch,
      previousEngineInstanceId: instance, legacyOrigin: "synthetic-stopped-owner", processesQuiesced: true, effectsReconciled: true, launchersDisabled: true, credentialsFenced: true }));
    return witnessId;
  }
  async function prepareProject(label) {
    const projectId = randomUUID(); const witnessId = await witness(projectId);
    const boot = { projectId, witnessId, idempotencyKey: "bootstrap", projectKey: label, head: "base", sourceRoot: "source-a", environmentRoot: "env", topics: ["policy/verification", "interface"] };
    committed(await api.bootstrapProject(ctx, boot));
    return { projectId, witnessId, boot };
  }
  async function own(project) {
    return committed(await api.acquireOwner(engineContext(), { projectId: project.projectId, witnessId: project.witnessId, idempotencyKey: "acquire", expectedOwnerEpoch: "1" })).data;
  }
  const row = async projectId => (await admin.query(`SELECT *,owner_epoch::text,event_seq::text,integration_version::text,renew_serial::text FROM ${S}.projects WHERE project_id=$1`, [projectId])).rows[0];
  const proof = [];
  async function raceUnderLock(firstOperation, secondOperation, blocker, projectId) {
    await blocker.query("BEGIN");
    await blocker.query(`SELECT 1 FROM ${S}.projects WHERE project_id=$1 FOR UPDATE`, [projectId]);
    const first = firstOperation();
    // Register rejection handlers immediately, while both requests are in flight.
    const firstSettled = Promise.allSettled([first]);
    let observed;
    for (let i = 0; i < 50; i++) {
      const waits = await admin.query("SELECT pid,wait_event_type,wait_event,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE application_name='rudder-authority-node-qualification' AND wait_event_type='Lock'");
      if (waits.rows.length) { observed = waits.rows; break; }
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert(observed, "real adapter session must wait on row lock"); proof.push({ kind: "row-lock", projectId, observed });
    const second = secondOperation(); const both = Promise.allSettled([first, second]);
    let overlap;
    for (let i=0;i<50;i++) {
      const waits = await admin.query("SELECT pid,wait_event_type,wait_event,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE application_name='rudder-authority-node-qualification' AND wait_event_type='Lock'");
      if(waits.rows.length>=2) {overlap=waits.rows;break;}
      await new Promise(resolve=>setTimeout(resolve,5));
    }
    assert(overlap,"both actual Node adapter requests must overlap under the row lock");
    proof.push({kind:"concurrent-node-adapter-requests",projectId,observed:overlap});
    await blocker.query("COMMIT"); await firstSettled; return both;
  }
  await t.test("tenant, credential, request-author and admission boundaries", async () => {
    const p = await prepareProject("auth");
    await assert.rejects(api.getReceipt({ ...ctx, accountId: "tenant-b" }, p.projectId, "bootstrap"), { code: "not_found" });
    await assert.rejects(api.getReceipt({ ...ctx, principalId: "other-user" }, p.projectId, "bootstrap"), { code: "forbidden" });
    await assert.rejects(api.getReceipt({ ...ctx, credentialSha256: "0".repeat(64) }, p.projectId, "bootstrap"), { code: "forbidden" });
    await assert.rejects(api.acquireOwner(engineContext(), { projectId: p.projectId, witnessId: p.witnessId, expectedOwnerEpoch: "1", idempotencyKey: "fake-author", author: "user" }), { code: "invalid_request" });
    await assert.rejects(api.admitHandoff(ctx, { projectId: p.projectId, witnessId: randomUUID(), rootId: "handoff", previousOwnerEpoch: "0", previousEngineInstanceId: null, legacyOrigin: "x", processesQuiesced: true, effectsReconciled: true, launchersDisabled: true, credentialsFenced: true }), { code: "forbidden" });
    const raw = new pg.Client({ ...settings, user: "rudder_authority_runtime" }); raw.on("error", () => {}); await raw.connect();
    assert.equal((await raw.query(`SELECT count(*)::text AS n FROM ${S}.projects`)).rows[0].n, "0", "RLS hides rows without trusted tenant setting");
    await assert.rejects(raw.query(`UPDATE ${S}.authorities SET state='ready'`), { code: "42501" });
    await assert.rejects(raw.query(`UPDATE ${S}.idempotency SET operation_kind='changed'`), { code: "42501" });
    await raw.end();
  });
  await t.test("concurrent acquire, renewal dedup, old boot/process, physical release and expiry", async () => {
    const p = await prepareProject("ownership"); const blocker = await connect();
    const results = await raceUnderLock(
      () => api.acquireOwner(engineContext(), { projectId: p.projectId, witnessId: p.witnessId, idempotencyKey: "claim-a", expectedOwnerEpoch: "1" }),
      () => api.acquireOwner(engineContext("b"), { projectId: p.projectId, witnessId: p.witnessId, idempotencyKey: "claim-b", expectedOwnerEpoch: "1" }), blocker,p.projectId);
    assert.equal(results.filter(r => r.status === "fulfilled").length, 1);
    const winner = results.find(r => r.status === "fulfilled"); const grant = committed(winner.value).data;
    const winning = grant.engineInstanceId === engines.a ? "a" : "b"; const owner = engineContext(winning);
    const renewal = { projectId: p.projectId, requestId: randomUUID(), ownerEpoch: grant.ownerEpoch, leaseId: grant.leaseId, expectedSerial: "0" };
    const a = committed(await api.renewOwner(owner, renewal)); const b = committed(await api.renewOwner(owner, renewal)); assert.deepEqual(a, b);
    await assert.rejects(api.renewOwner(engineContext(winning, "wrong-boot"), { ...renewal, requestId: randomUUID(), expectedSerial: "1" }), { code: "stale_fence" });
    await assert.rejects(api.renewOwner(engineContext("newer"), { ...renewal, requestId: randomUUID(), expectedSerial: "1" }), { code: "stale_fence" });
    await assert.rejects(api.releaseOwner(owner, { projectId: p.projectId, ownerEpoch: grant.ownerEpoch, leaseId: grant.leaseId, witnessId: p.witnessId, idempotencyKey: "release-no-proof" }), { code: "handoff_required" });
    await admin.query(`UPDATE ${S}.projects SET lease_until=clock_timestamp()-interval '1 second' WHERE project_id=$1`, [p.projectId]);
    await assert.rejects(api.renewOwner(owner, { ...renewal, requestId: randomUUID(), expectedSerial: "1" }), { code: "stale_fence" });
    await assert.rejects(api.acquireOwner(engineContext(winning === "a" ? "b" : "a"), { projectId: p.projectId, expectedOwnerEpoch: "2", witnessId: p.witnessId, idempotencyKey: "expiry-alone" }), { code: "handoff_required" });
    const sealed = await witness(p.projectId, "2", grant.engineInstanceId);
    const release = { projectId: p.projectId, ownerEpoch: grant.ownerEpoch, leaseId: grant.leaseId, witnessId: sealed, idempotencyKey: "release" };
    assert.deepEqual(committed(await api.releaseOwner(owner, release)), committed(await api.releaseOwner(owner, release)));
    assert.equal((await row(p.projectId)).owner_epoch, "3");
    const moved = committed(await api.acquireOwner(engineContext("newer"), { projectId: p.projectId, expectedOwnerEpoch: "3", witnessId: sealed, idempotencyKey: "new-owner" }));
    assert.equal(moved.data.ownerEpoch, "4"); await blocker.end();
  });
  await t.test("post-lock clock rejects an expired renewal and revocation races serialize before admission", async () => {
    const p=await prepareProject("clock-and-revocation"); const grant=await own(p); const blocker=await connect();
    const renewal={projectId:p.projectId,ownerEpoch:grant.ownerEpoch,leaseId:grant.leaseId,expectedSerial:"0",requestId:randomUUID()};
    const expiry=(await admin.query(`UPDATE ${S}.projects SET lease_until=clock_timestamp()+interval '150 milliseconds' WHERE project_id=$1 RETURNING lease_until::text`,[p.projectId])).rows[0].lease_until;
    await blocker.query("BEGIN"); await blocker.query(`SELECT 1 FROM ${S}.projects WHERE project_id=$1 FOR UPDATE`,[p.projectId]);
    const pending=api.renewOwner(engineContext(),renewal).catch(error=>error);
    let observed;
    for(let i=0;i<30;i++) {
      const rows=await admin.query("SELECT pid,xact_start::text,wait_event_type,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE application_name='rudder-authority-node-qualification' AND wait_event_type='Lock'");
      if(rows.rows.length) {observed=rows.rows[0];break;}
      await new Promise(resolve=>setTimeout(resolve,3));
    }
    assert(observed); await new Promise(resolve=>setTimeout(resolve,170)); await blocker.query("COMMIT");
    assert.equal((await pending).code,"stale_fence");
    assert.equal((await row(p.projectId)).renew_serial,"0");
    assert(Date.parse(observed.xact_start)<Date.parse(expiry));
    proof.push({kind:"node-renewal-clock-after-row-lock",expiry,observed});
    await admin.query(`UPDATE ${S}.projects SET lease_until=clock_timestamp()+interval '15 seconds' WHERE project_id=$1`,[p.projectId]);
    await blocker.query("BEGIN");await blocker.query(`SELECT 1 FROM ${S}.projects WHERE project_id=$1 FOR UPDATE`,[p.projectId]);
    const revoked=api.renewOwner(engineContext(),{...renewal,requestId:randomUUID()}).catch(error=>error);
    await new Promise(resolve=>setTimeout(resolve,20));
    await admin.query(`UPDATE ${S}.authority_credentials SET revoked_at=clock_timestamp() WHERE credential_id='user-key'`);
    await blocker.query("COMMIT"); assert.equal((await revoked).code,"forbidden");
    await admin.query(`UPDATE ${S}.authority_credentials SET revoked_at=NULL WHERE credential_id='user-key'`); await blocker.end();
  });
  async function verifiedProject(label) {
    const p = await prepareProject(label); const grant = await own(p);
    const owner = { ownerEpoch: grant.ownerEpoch, leaseId: grant.leaseId };
    committed(await api.acceptDecision(engineContext(), { projectId: p.projectId, ...owner, topic: "policy/verification", expectedVersion: "0", statement: { policy: "fixture" }, idempotencyKey: "policy" }));
    const candidates = {};
    for (const candidate of ["a", "b"]) {
      const verificationId = randomUUID();
      const admission = { projectId: p.projectId, ...owner, verificationId, baseVersion: "0", baseHead: "base", testedHead: "candidate-" + candidate,
        sourceRoot: "source-" + candidate, environmentRoot: "env", gateReceiptRoot: "gate", buildContractRoot: "contract", topics: { "policy/verification": "1", interface: "0" } };
      committed(await api.admitVerification({ ...operator, engine: engineContext().engine }, admission));
      candidates[candidate] = { projectId: p.projectId, ...owner, verificationId, expectedVersion: "0", expectedHead: "base", newHead: admission.testedHead, sourceRoot: admission.sourceRoot, environmentRoot: "env", idempotencyKey: "land-" + candidate };
    }
    return { p, owner, candidates };
  }
  await t.test("exact head/topic CAS and verification identity, durable event/receipt/outbox", async () => {
    const { p, owner, candidates } = await verifiedProject("integration");
    await assert.rejects(api.integrate(engineContext(), { ...candidates.a, verificationId: candidates.b.verificationId, idempotencyKey: "wrong-candidate" }), { code: "head_conflict" });
    const blocker = await connect();
    const outcomes = await raceUnderLock(() => api.integrate(engineContext(), candidates.a), () => api.integrate(engineContext(), candidates.b), blocker, p.projectId);
    assert.equal(outcomes.filter(r => r.status === "fulfilled").length, 1);
    assert.equal((await row(p.projectId)).integration_version, "1");
    const landed = outcomes.find(r => r.status === "fulfilled").value;
    const winning = landed.value.data.head.endsWith("a") ? candidates.a : candidates.b;
    assert.deepEqual(await api.integrate(engineContext(), winning), landed);
    const counts = await admin.query(`SELECT (SELECT count(*) FROM ${S}.events WHERE project_id=$1)::text AS events,
      (SELECT count(*) FROM ${S}.idempotency WHERE project_id=$1)::text AS receipts,(SELECT count(*) FROM ${S}.outbox WHERE project_id=$1)::text AS outbox`, [p.projectId]);
    assert.deepEqual(counts.rows[0], { events: "4", receipts: "4", outbox: "4" }); await blocker.end();
    const changed = await verifiedProject("changed-topic");
    committed(await api.acceptDecision(engineContext(), { projectId: changed.p.projectId, ...changed.owner, topic: "interface", expectedVersion: "0", statement: "new", idempotencyKey: "new-interface" }));
    await assert.rejects(api.integrate(engineContext(), changed.candidates.a), { code: "head_conflict" });
    assert.equal((await row(changed.p.projectId)).head_vcs_commit, "base");
  });
  let durable;
  await t.test("lost COMMIT acknowledgment is uncertain and reconciles by original key", async () => {
    const p = await prepareProject("lost-reply"); const req = { projectId: p.projectId, witnessId: p.witnessId, idempotencyKey: "lost-commit", expectedOwnerEpoch: "1" };
    const original = pg.Client.prototype.query; let injected = false;
    pg.Client.prototype.query = function (query, ...args) {
      const result = original.call(this, query, ...args);
      if (query === "COMMIT" && !injected && this.connectionParameters.user === "rudder_authority_runtime") {
        injected = true; return result.then(() => { throw Object.assign(new Error("private SQL and credentials must not escape"), { code: "ECONNRESET" }); });
      }
      return result;
    };
    let outcome; try { outcome = await api.acquireOwner(engineContext(), req); } finally { pg.Client.prototype.query = original; }
    assert.deepEqual(outcome, { status: "uncertain", reconcile: "retry_same_operation" }); assert(injected);
    const retry = await api.acquireOwner(engineContext(), req); assert.equal(committed(retry).data.ownerEpoch, "2");
    assert.equal((await row(p.projectId)).event_seq, "2");
    durable = { p, req, retry };
  });
  await t.test("client budget does not free admission while a driver operation is unsettled", async () => {
    const limited = await qualificationPool({ ...base, user: "rudder_authority_runtime", role: "runtime", maxConnections: 1, maxQueued: 1, requestMs: 400, statementMs: 100, lockMs: 50 }); resources.push(limited);
    const one = limited.pool.run(accountId, tx => tx.query("SELECT pg_sleep(2)")).catch(error => error);
    await new Promise(resolve => setTimeout(resolve, 25));
    const two = limited.pool.run(accountId, tx => tx.query("SELECT 1 AS ok"));
    await assert.rejects(limited.pool.run(accountId, tx => tx.query("SELECT 2")), { code: "busy" });
    assert.deepEqual(limited.pool.stats(), { active: 1, queued: 1 });
    assert.equal((await one).code, "deadline_exceeded"); committed(await two);
    assert.deepEqual(limited.pool.stats(), { active: 0, queued: 0 });
    const short = await qualificationPool({ ...base,user:"rudder_authority_runtime",role:"runtime",maxConnections:1,maxQueued:0,requestMs:120,statementMs:1000,lockMs:50 }); resources.push(short);
    const original=pg.Client.prototype.query;
    pg.Client.prototype.query=function(query,...args) {
      const result=original.call(this,query,...args);
      if(query==="SELECT pg_sleep(2)") return result.then(async value=>{await new Promise(resolve=>setTimeout(resolve,150));return value;},async error=>{await new Promise(resolve=>setTimeout(resolve,150));throw error;});
      return result;
    };
    try {
      const held=short.pool.run(accountId,tx=>tx.query("SELECT pg_sleep(2)")).catch(error=>error);
      await new Promise(resolve=>setTimeout(resolve,160));
      assert.deepEqual(short.pool.stats(),{active:1,queued:0},"destroyed client still owns admission until query promise settles");
      await assert.rejects(short.pool.run(accountId,tx=>tx.query("SELECT 1")),{code:"busy"});
      assert.equal((await held).code,"deadline_exceeded");assert.deepEqual(short.pool.stats(),{active:0,queued:0});
    } finally {pg.Client.prototype.query=original;}
  });
  await t.test("server crash rolls back full acceptance and durable retry survives restart", async () => {
    const projectId = randomUUID(); const witnessId = await witness(projectId);
    const req = { projectId, witnessId, idempotencyKey: "crash-bootstrap", projectKey: "crash-bootstrap", head: "base", sourceRoot: "source-a", environmentRoot: "env", topics: ["policy/verification", "interface"] };
    const original = pg.Client.prototype.query; let announce; const reached = new Promise(resolve => { announce = resolve; });
    pg.Client.prototype.query = function (query, ...args) {
      const result = original.call(this, query, ...args);
      if (typeof query === "string" && query.includes(`INSERT INTO ${S}.outbox`) && args[0]?.[2] === projectId) {
        return result.then(() => { announce(); return original.call(this, "SELECT pg_sleep(20)"); });
      }
      return result;
    };
    const pending = api.bootstrapProject(ctx, req).catch(error => error);
    await reached;
    try { await exec(path.join(state.pgbin, "pg_ctl"), ["-D", state.data, "-m", "immediate", "-w", "stop"]); }
    finally { pg.Client.prototype.query = original; }
    const error = await pending; assert(error instanceof AuthorityError); assert.equal(error.code, "unavailable");
    await admin.end().catch(() => {});
    await exec(path.join(state.pgbin, "pg_ctl"), ["-D", state.data, "-l", state.log, "-w", "start"]);
    admin = await connect();
    assert.equal((await admin.query(`SELECT count(*)::text AS n FROM ${S}.projects WHERE project_id=$1`, [projectId])).rows[0].n, "0");
    assert.equal((await admin.query(`SELECT count(*)::text AS n FROM ${S}.idempotency WHERE project_id=$1`, [projectId])).rows[0].n, "0");
    assert.deepEqual(await api.acquireOwner(engineContext(), durable.req), durable.retry);
    committed(await api.bootstrapProject(ctx, req));
  });
  await t.test("overflow rolls back and sanitized failures expose neither SQL nor parameters", async () => {
    const p = await prepareProject("overflow"); await own(p);
    await admin.query(`UPDATE ${S}.projects SET event_seq=9223372036854775807 WHERE project_id=$1`, [p.projectId]);
    await assert.rejects(api.acceptDecision(engineContext(), { projectId: p.projectId, ownerEpoch: "2", leaseId: (await row(p.projectId)).lease_id,
      topic: "interface", expectedVersion: "0", statement: "sensitive-param-sentinel", idempotencyKey: "overflow" }), error => {
      assert.equal(error.code, "counter_exhausted"); assert(!String(error).includes("sensitive-param-sentinel")); assert(!JSON.stringify(error).includes("SELECT")); return true;
    });
  });
  await fs.writeFile(path.join(path.dirname(state.socket), "node-proof.json"), JSON.stringify({ authorityId, realSessionBlockers: proof, scope: "Node adapter on one isolated PostgreSQL server; no HA/RPO/application-ingress qualification" }, null, 2));
});
