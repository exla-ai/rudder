import { createHash, randomUUID } from "node:crypto";
import { AuthorityPool, SCHEMA as S, type Transaction } from "./pool.js";
import { canonical, counter, exact, fail, hash, next, text, uuid, type Accepted, type AcquireRequest,
  type BootstrapRequest, type DecisionRequest, type HandoffAdmission, type IntegrationRequest,
  type Json, type Outcome, type OwnerFence, type OwnerGrant, type ReleaseRequest, type RenewRequest,
  type RootAdmission, type TrustedContext, type VerificationAdmission } from "./types.js";

type Project = { authority_id: string; account_id: string; project_id: string; mode: string;
  owner_epoch: string; owner_engine_instance: string | null; lease_id: string | null; lease_until: string | null;
  renew_serial: string; event_seq: string; integration_version: string; head_vcs_commit: string };
const projectColumns = "authority_id,account_id,project_id,mode,owner_epoch::text,owner_engine_instance,lease_id,lease_until,renew_serial::text,event_seq::text,integration_version::text,head_vcs_commit";
const creatorOps = ["receipt:read", "owner:acquire", "owner:renew", "owner:release", "decision:accept", "integration:accept"];
const sha = (value: Buffer) => createHash("sha256").update(value).digest("hex");
const snapshot = <T>(value: T): T => JSON.parse(canonical(value).toString()) as T;

function context(input: TrustedContext): TrustedContext {
  exact(input, ["accountId", "principalId", "credentialId", "credentialSha256", "engine"]);
  const result: TrustedContext = { accountId: text(input.accountId), principalId: text(input.principalId),
    credentialId: text(input.credentialId), credentialSha256: hash(input.credentialSha256) };
  if (input.engine) {
    exact(input.engine, ["instanceId", "bootId", "credentialId", "credentialSha256"]);
    result.engine = { instanceId: uuid(input.engine.instanceId), bootId: text(input.engine.bootId),
      credentialId: text(input.engine.credentialId), credentialSha256: hash(input.engine.credentialSha256) };
  }
  return result;
}
function fence(input: OwnerFence): void { counter(input.ownerEpoch, true); uuid(input.leaseId); }
function operationBytes(ctx: TrustedContext, kind: string, input: unknown): Buffer {
  return canonical({ formatVersion: 1, kind, request: input,
    engine: ctx.engine ? { instanceId: ctx.engine.instanceId, bootId: ctx.engine.bootId } : null });
}

/** Dormant library. No server route, scheduler, environmental fallback or automatic activation. */
export class ProjectAuthority {
  #pool?: AuthorityPool;
  constructor(readonly authorityId: string, pool?: AuthorityPool) { uuid(authorityId); this.#pool = pool; }
  stats(): { active: number; queued: number } { return this.#pool?.stats() ?? { active: 0, queued: 0 }; }
  async close(): Promise<void> { await this.#pool?.close(); }
  #run<T>(ctx: TrustedContext, body: (tx: Transaction) => Promise<T>): Promise<Outcome<T>> {
    if (!this.#pool) fail("inactive");
    return this.#pool.run(ctx.accountId, async tx => {
      const result = await body(tx);
      // Expiry is time-based, so recheck after all potentially waiting locks/writes.
      await this.#credential(tx, ctx);
      if (ctx.engine) await this.#credential(tx, ctx, true);
      return result;
    });
  }
  async #authority(tx: Transaction, requireReady = true): Promise<void> {
    const row = await tx.query(`SELECT state,protocol_version FROM ${S}.authorities WHERE authority_id=$1 FOR SHARE`, [this.authorityId]);
    if (row.rows.length !== 1 || row.rows[0].protocol_version !== 1 || row.rows[0].state === "retired" || (requireReady && row.rows[0].state !== "ready")) fail("inactive");
  }
  async #credential(tx: Transaction, ctx: TrustedContext, engine = false): Promise<string> {
    const source = engine ? ctx.engine : ctx;
    if (!source) fail("forbidden");
    const row = await tx.query(`SELECT principal_id,kind,expires_at FROM ${S}.authority_credentials
      WHERE account_id=$1 AND credential_id=$2 AND token_sha256=$3 AND revoked_at IS NULL FOR SHARE`,
    [ctx.accountId, source.credentialId, source.credentialSha256]);
    if (row.rows.length !== 1 || (!engine && row.rows[0].principal_id !== ctx.principalId)
      || (engine && row.rows[0].kind !== "engine")) fail("forbidden");
    const live = await tx.query("SELECT $1::timestamptz>clock_timestamp() AS live", [row.rows[0].expires_at]);
    if (!live.rows[0].live) fail("forbidden");
    return row.rows[0].kind;
  }
  async #accountGrant(tx: Transaction, ctx: TrustedContext, operation: string): Promise<string> {
    const kind = await this.#credential(tx, ctx);
    if (kind === "engine" || (operation !== "project:bootstrap" && kind !== "service")) fail("forbidden");
    const row = await tx.query(`SELECT operations FROM ${S}.account_principals WHERE authority_id=$1 AND account_id=$2
      AND principal_id=$3 AND revoked_at IS NULL FOR SHARE`, [this.authorityId, ctx.accountId, ctx.principalId]);
    if (row.rows.length !== 1 || !row.rows[0].operations.includes(operation)) fail("forbidden");
    return kind;
  }
  async #project(tx: Transaction, ctx: TrustedContext, projectId: string): Promise<Project> {
    await this.#authority(tx, false);
    const row = await tx.query<Project>(`SELECT ${projectColumns} FROM ${S}.projects
      WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 FOR UPDATE`, [this.authorityId, ctx.accountId, projectId]);
    if (row.rows.length !== 1) fail("not_found");
    return row.rows[0];
  }
  async #member(tx: Transaction, ctx: TrustedContext, projectId: string, operation: string, topic?: string): Promise<void> {
    await this.#credential(tx, ctx);
    const row = await tx.query(`SELECT operations,scopes FROM ${S}.project_principals WHERE authority_id=$1 AND account_id=$2
      AND project_id=$3 AND principal_id=$4 AND revoked_at IS NULL FOR SHARE`, [this.authorityId, ctx.accountId, projectId, ctx.principalId]);
    if (row.rows.length !== 1 || !row.rows[0].operations.includes(operation) || !row.rows[0].scopes.includes("project")
      || (topic !== undefined && !row.rows[0].scopes.includes("topic:" + topic))) fail("forbidden");
  }
  async #engine(tx: Transaction, ctx: TrustedContext): Promise<void> {
    await this.#credential(tx, ctx, true);
    const e = ctx.engine!;
    const row = await tx.query(`SELECT 1 FROM ${S}.engine_instances WHERE account_id=$1 AND engine_instance_id=$2
      AND machine_boot_id=$3 AND authenticated_key_id=$4 AND protocol_version=1 AND revoked_at IS NULL FOR SHARE`,
    [ctx.accountId, e.instanceId, e.bootId, e.credentialId]);
    if (row.rows.length !== 1) fail("stale_fence");
  }
  async #owner(tx: Transaction, ctx: TrustedContext, p: Project, expected: OwnerFence, live = true): Promise<string> {
    await this.#authority(tx);
    await this.#engine(tx, ctx);
    if (p.mode !== "ready" || p.owner_epoch !== expected.ownerEpoch || p.lease_id !== expected.leaseId
      || p.owner_engine_instance !== ctx.engine!.instanceId) fail("stale_fence");
    const row = await tx.query("SELECT clock_timestamp() AS now, $1::timestamptz>clock_timestamp() AS live", [p.lease_until]);
    if (live && !row.rows[0].live) fail("stale_fence");
    return row.rows[0].now;
  }
  async #prior(tx: Transaction, ctx: TrustedContext, projectId: string, key: string, kind: string, bytes: Buffer): Promise<Accepted | null> {
    const row = await tx.query(`SELECT request_canonical,operation_kind,accepted_receipt FROM ${S}.idempotency
      WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND principal_id=$4 AND idempotency_key=$5`,
    [this.authorityId, ctx.accountId, projectId, ctx.principalId, key]);
    if (!row.rows.length) return null;
    if (row.rows[0].operation_kind !== kind || !row.rows[0].request_canonical.equals(bytes)) fail("conflict");
    return row.rows[0].accepted_receipt as Accepted;
  }
  async #record(tx: Transaction, ctx: TrustedContext, projectId: string, key: string, kind: string, bytes: Buffer, sequence: string, data: Json, operationId = randomUUID()): Promise<Accepted> {
    const receipt: Accepted = { formatVersion: 1, authorityId: this.authorityId, projectId, operationId, sequence: counter(sequence, true), kind, data };
    const envelope = JSON.stringify(receipt);
    await tx.query(`INSERT INTO ${S}.events(authority_id,account_id,project_id,project_seq,event_id,kind,principal_id,payload)
      VALUES($1,$2,$3,$4,$5,$6,$7,$8)`, [this.authorityId, ctx.accountId, projectId, sequence, operationId, kind, ctx.principalId, envelope]);
    await tx.query(`INSERT INTO ${S}.idempotency(authority_id,account_id,project_id,principal_id,idempotency_key,operation_id,
      operation_kind,request_sha256,request_canonical,accepted_seq,accepted_receipt) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)`,
    [this.authorityId, ctx.accountId, projectId, ctx.principalId, key, operationId, kind, sha(bytes), bytes, sequence, envelope]);
    await tx.query(`INSERT INTO ${S}.outbox(authority_id,account_id,project_id,outbox_id,event_seq,kind,destination,payload,status)
      VALUES($1,$2,$3,$4,$5,$6,'project-events',$7,'pending')`, [this.authorityId, ctx.accountId, projectId, randomUUID(), sequence, kind, envelope]);
    return receipt;
  }
  async #bump(tx: Transaction, p: Project): Promise<string> {
    const seq = next(p.event_seq);
    await tx.query(`UPDATE ${S}.projects SET event_seq=$4 WHERE authority_id=$1 AND account_id=$2 AND project_id=$3`, [p.authority_id, p.account_id, p.project_id, seq]);
    return seq;
  }
  async #roots(tx: Transaction, account: string, roots: [string, string][]): Promise<void> {
    for (const [root, kind] of roots) {
      const row = await tx.query(`SELECT 1 FROM ${S}.retained_roots WHERE account_id=$1 AND root_id=$2 AND kind=$3`, [account, root, kind]);
      if (!row.rows.length) fail("forbidden");
    }
  }
  async #handoff(tx: Transaction, ctx: TrustedContext, p: Project | null, projectId: string, witnessId: string, operationId: string): Promise<string> {
    const row = await tx.query(`SELECT * FROM ${S}.handoff_witnesses WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND witness_id=$4`, [this.authorityId, ctx.accountId, projectId, witnessId]);
    if (row.rows.length !== 1) fail("handoff_required");
    const witness = row.rows[0];
    await this.#roots(tx, ctx.accountId, [[witness.root_id, "handoff"]]);
    const pending = await tx.query(`SELECT 1 FROM ${S}.command_effects WHERE authority_id=$1 AND account_id=$2 AND project_id=$3
      AND state IN ('started','uncertain') LIMIT 1`, [this.authorityId, ctx.accountId, projectId]);
    if (pending.rows.length) fail("handoff_required");
    if (p && p.owner_engine_instance === null) {
      const history = await tx.query(`SELECT operation_id,quiescence_root FROM ${S}.ownership_receipts WHERE authority_id=$1
        AND account_id=$2 AND project_id=$3 AND owner_epoch=$4`, [this.authorityId, ctx.accountId, projectId, p.owner_epoch]);
      if (history.rows[0]?.operation_id !== witness.consumed_operation_id || history.rows[0]?.quiescence_root !== witness.root_id) fail("handoff_required");
      return witness.root_id;
    }
    if (witness.consumed_operation_id !== null || String(witness.previous_owner_epoch) !== (p?.owner_epoch ?? "0")
      || witness.previous_engine_instance !== (p?.owner_engine_instance ?? null)) fail("handoff_required");
    await tx.query(`UPDATE ${S}.handoff_witnesses SET consumed_operation_id=$5 WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND witness_id=$4`,
    [this.authorityId, ctx.accountId, projectId, witnessId, operationId]);
    return witness.root_id;
  }

  async admitRetainedRoot(trusted: TrustedContext, input: RootAdmission): Promise<Outcome<{ rootId: string }>> {
    const ctx = context(trusted); exact(input, ["rootId", "kind", "digest", "sizeBytes", "retentionReceipt"]);
    text(input.rootId); hash(input.digest);
    if (!["source", "environment", "gate-evidence", "build-contract", "handoff"].includes(input.kind)
      || typeof input.sizeBytes !== "string" || !/^(0|[1-9][0-9]{0,19})$/.test(input.sizeBytes) || BigInt(input.sizeBytes) > 18446744073709551615n) fail("invalid_request");
    const req = snapshot(input); const bytes = canonical(req.retentionReceipt, 65536);
    return this.#run(ctx, async tx => {
      await this.#authority(tx); await this.#accountGrant(tx, ctx, "retention:admit");
      await tx.query(`INSERT INTO ${S}.retained_roots(account_id,root_id,kind,algorithm,digest,size_bytes,receipt_canonical,receipt_sha256)
        VALUES($1,$2,$3,'sha256',$4,$5,$6,$7) ON CONFLICT DO NOTHING`, [ctx.accountId, req.rootId, req.kind, req.digest, req.sizeBytes, bytes, sha(bytes)]);
      const saved = await tx.query(`SELECT kind,digest,size_bytes::text,receipt_canonical FROM ${S}.retained_roots WHERE account_id=$1 AND root_id=$2`, [ctx.accountId, req.rootId]);
      const row = saved.rows[0]; if (!row || row.kind !== req.kind || row.digest !== req.digest || row.size_bytes !== req.sizeBytes || !row.receipt_canonical.equals(bytes)) fail("conflict");
      return { rootId: req.rootId };
    });
  }
  async admitHandoff(trusted: TrustedContext, input: HandoffAdmission): Promise<Outcome<{ witnessId: string }>> {
    const ctx = context(trusted); exact(input, ["projectId", "witnessId", "rootId", "previousOwnerEpoch", "previousEngineInstanceId", "legacyOrigin", "processesQuiesced", "effectsReconciled", "launchersDisabled", "credentialsFenced"]);
    uuid(input.projectId); uuid(input.witnessId); text(input.rootId); counter(input.previousOwnerEpoch); text(input.legacyOrigin);
    if (input.previousEngineInstanceId !== null) uuid(input.previousEngineInstanceId);
    if ([input.processesQuiesced, input.effectsReconciled, input.launchersDisabled, input.credentialsFenced].some(value => value !== true)) fail("handoff_required");
    const req = snapshot(input); const bytes = canonical(req);
    return this.#run(ctx, async tx => {
      await this.#authority(tx); await this.#accountGrant(tx, ctx, "handoff:admit");
      await tx.query("SELECT pg_advisory_xact_lock(hashtextextended($1,44))", [this.authorityId + ":" + ctx.accountId + ":" + req.projectId]);
      await this.#roots(tx, ctx.accountId, [[req.rootId, "handoff"]]);
      await tx.query(`INSERT INTO ${S}.handoff_witnesses(authority_id,account_id,project_id,witness_id,root_id,previous_owner_epoch,
        previous_engine_instance,legacy_origin,processes_quiesced,effects_reconciled,launchers_disabled,credentials_fenced,request_canonical)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,true,true,true,true,$9) ON CONFLICT DO NOTHING`,
      [this.authorityId, ctx.accountId, req.projectId, req.witnessId, req.rootId, req.previousOwnerEpoch, req.previousEngineInstanceId, req.legacyOrigin, bytes]);
      const saved = await tx.query(`SELECT request_canonical FROM ${S}.handoff_witnesses WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND witness_id=$4`, [this.authorityId, ctx.accountId, req.projectId, req.witnessId]);
      if (!saved.rows[0]?.request_canonical.equals(bytes)) fail("conflict");
      return { witnessId: req.witnessId };
    });
  }
  async bootstrapProject(trusted: TrustedContext, input: BootstrapRequest): Promise<Outcome<Accepted>> {
    const ctx = context(trusted); exact(input, ["projectId", "projectKey", "idempotencyKey", "witnessId", "head", "sourceRoot", "environmentRoot", "topics"]);
    uuid(input.projectId); uuid(input.witnessId); [input.projectKey, input.idempotencyKey, input.head, input.sourceRoot, input.environmentRoot].forEach(value => text(value));
    if (!Array.isArray(input.topics) || input.topics.length > 64 || !input.topics.includes("policy/verification") || new Set(input.topics).size !== input.topics.length) fail("invalid_request");
    input.topics.forEach(value => text(value)); const req = snapshot(input); const bytes = operationBytes(ctx, "project-bootstrap", req);
    return this.#run(ctx, async tx => {
      await this.#authority(tx); const principalKind = await this.#accountGrant(tx, ctx, "project:bootstrap");
      await tx.query("SELECT pg_advisory_xact_lock(hashtextextended($1,44))", [this.authorityId + ":" + ctx.accountId + ":" + req.projectId]);
      const existing = await tx.query(`SELECT project_id FROM ${S}.projects WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 FOR UPDATE`, [this.authorityId, ctx.accountId, req.projectId]);
      if (existing.rows.length) { const prior = await this.#prior(tx, ctx, req.projectId, req.idempotencyKey, "project-bootstrap", bytes); if (prior) return prior; fail("conflict"); }
      await this.#roots(tx, ctx.accountId, [[req.sourceRoot, "source"], [req.environmentRoot, "environment"]]);
      const operation = randomUUID(); const root = await this.#handoff(tx, ctx, null, req.projectId, req.witnessId, operation);
      await tx.query(`INSERT INTO ${S}.projects(authority_id,account_id,project_id,project_key,mode,owner_epoch,event_seq,head_vcs_commit,head_source_root,head_environment_root)
        VALUES($1,$2,$3,$4,'ready',1,1,$5,$6,$7)`, [this.authorityId, ctx.accountId, req.projectId, req.projectKey, req.head, req.sourceRoot, req.environmentRoot]);
      await tx.query(`INSERT INTO ${S}.ownership_receipts(authority_id,account_id,project_id,owner_epoch,operation_id,cause,quiescence_root,receipt_canonical)
        VALUES($1,$2,$3,1,$4,'created',$5,$6)`, [this.authorityId, ctx.accountId, req.projectId, operation, root, bytes]);
      await tx.query(`INSERT INTO ${S}.project_principals(authority_id,account_id,project_id,principal_id,principal_kind,access_version,operations,scopes)
        VALUES($1,$2,$3,$4,$5,1,$6,$7)`, [this.authorityId, ctx.accountId, req.projectId, ctx.principalId, principalKind, JSON.stringify(creatorOps), JSON.stringify(["project", ...req.topics.map(topic => "topic:" + topic)])]);
      return this.#record(tx, ctx, req.projectId, req.idempotencyKey, "project-bootstrap", bytes, "1", { head: req.head, ownerEpoch: "1" }, operation);
    });
  }
  async acquireOwner(trusted: TrustedContext, input: AcquireRequest): Promise<Outcome<Accepted>> {
    const ctx = context(trusted); exact(input, ["projectId", "idempotencyKey", "expectedOwnerEpoch", "witnessId"]);
    uuid(input.projectId); uuid(input.witnessId); text(input.idempotencyKey); counter(input.expectedOwnerEpoch, true);
    const req = snapshot(input); const bytes = operationBytes(ctx, "owner-acquire", req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#member(tx, ctx, req.projectId, "owner:acquire");
      const prior = await this.#prior(tx, ctx, req.projectId, req.idempotencyKey, "owner-acquire", bytes); if (prior) return prior;
      await this.#authority(tx); await this.#engine(tx, ctx);
      if (p.mode !== "ready" || p.owner_epoch !== req.expectedOwnerEpoch) fail("stale_fence");
      const clock = await tx.query("SELECT clock_timestamp() AS now,$1::timestamptz>clock_timestamp() AS live", [p.lease_until]);
      if (p.owner_engine_instance && clock.rows[0].live) fail("stale_fence");
      const epoch = next(p.owner_epoch); const sequence = next(p.event_seq); const operation = randomUUID();
      const root = await this.#handoff(tx, ctx, p, req.projectId, req.witnessId, operation);
      const lease = randomUUID();
      const updated = await tx.query(`UPDATE ${S}.projects SET owner_epoch=$4,owner_engine_instance=$5,lease_id=$6,
        lease_until=clock_timestamp()+interval '15 seconds',renew_serial=0,event_seq=$7
        WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 RETURNING lease_until,lease_until-interval '15 seconds' AS issued_at`,
      [this.authorityId, ctx.accountId, req.projectId, epoch, ctx.engine!.instanceId, lease, sequence]);
      await tx.query(`INSERT INTO ${S}.ownership_receipts(authority_id,account_id,project_id,owner_epoch,owner_engine_instance,operation_id,cause,quiescence_root,receipt_canonical)
        VALUES($1,$2,$3,$4,$5,$6,'claimed',$7,$8)`, [this.authorityId, ctx.accountId, req.projectId, epoch, ctx.engine!.instanceId, operation, root, bytes]);
      const grant: OwnerGrant = { formatVersion: 1, authorityId: this.authorityId, accountId: ctx.accountId, projectId: req.projectId,
        ownerEpoch: epoch, leaseId: lease, engineInstanceId: ctx.engine!.instanceId, machineBootId: ctx.engine!.bootId,
        renewalSerial: "0", requestId: req.idempotencyKey, issuedAt: updated.rows[0].issued_at, leaseUntil: updated.rows[0].lease_until, durationMs: 15000 };
      return this.#record(tx, ctx, req.projectId, req.idempotencyKey, "owner-acquire", bytes, sequence, snapshot(grant) as unknown as Json, operation);
    });
  }
  async renewOwner(trusted: TrustedContext, input: RenewRequest): Promise<Outcome<OwnerGrant>> {
    const ctx = context(trusted); exact(input, ["projectId", "requestId", "ownerEpoch", "leaseId", "expectedSerial"]);
    uuid(input.projectId); uuid(input.requestId); counter(input.expectedSerial); fence(input);
    const req = snapshot(input); const bytes = operationBytes(ctx, "owner-renew", req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#member(tx, ctx, req.projectId, "owner:renew");
      await this.#engine(tx, ctx);
      const prior = await tx.query(`SELECT request_canonical,owner_epoch::text,lease_id,renew_serial::text,issued_at,lease_until
        FROM ${S}.lease_renewals WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND request_id=$4`, [this.authorityId, ctx.accountId, req.projectId, req.requestId]);
      if (prior.rows.length && !prior.rows[0].request_canonical.equals(bytes)) fail("conflict");
      let row = prior.rows[0];
      if (!row) {
        const now = await this.#owner(tx, ctx, p, req); const serial = next(req.expectedSerial);
        const updated = await tx.query(`UPDATE ${S}.projects SET renew_serial=$8,lease_until=$7::timestamptz+interval '15 seconds'
          WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND owner_epoch=$4 AND lease_id=$5
            AND renew_serial=$6 AND lease_until>$7::timestamptz RETURNING owner_epoch::text,lease_id,renew_serial::text,lease_until`,
        [this.authorityId, ctx.accountId, req.projectId, req.ownerEpoch, req.leaseId, req.expectedSerial, now, serial]);
        if (!updated.rows.length) fail("stale_fence"); row = { ...updated.rows[0], issued_at: now };
        await tx.query(`INSERT INTO ${S}.lease_renewals(authority_id,account_id,project_id,request_id,grant_kind,owner_epoch,lease_id,
          renew_serial,engine_instance_id,request_canonical,issued_at,lease_until) VALUES($1,$2,$3,$4,'owner',$5,$6,$7,$8,$9,$10,$11)`,
        [this.authorityId, ctx.accountId, req.projectId, req.requestId, req.ownerEpoch, req.leaseId, serial, ctx.engine!.instanceId, bytes, now, row.lease_until]);
      }
      return { formatVersion: 1, authorityId: this.authorityId, accountId: ctx.accountId, projectId: req.projectId,
        ownerEpoch: row.owner_epoch, leaseId: row.lease_id, engineInstanceId: ctx.engine!.instanceId, machineBootId: ctx.engine!.bootId,
        renewalSerial: row.renew_serial, requestId: req.requestId, issuedAt: row.issued_at, leaseUntil: row.lease_until, durationMs: 15000 };
    });
  }
  async releaseOwner(trusted: TrustedContext, input: ReleaseRequest): Promise<Outcome<Accepted>> {
    const ctx = context(trusted); exact(input, ["projectId", "idempotencyKey", "ownerEpoch", "leaseId", "witnessId"]);
    uuid(input.projectId); uuid(input.witnessId); text(input.idempotencyKey); fence(input);
    const req = snapshot(input); const bytes = operationBytes(ctx, "owner-release", req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#member(tx, ctx, req.projectId, "owner:release");
      const prior = await this.#prior(tx, ctx, req.projectId, req.idempotencyKey, "owner-release", bytes); if (prior) return prior;
      await this.#owner(tx, ctx, p, req, false);
      const operation = randomUUID(); const root = await this.#handoff(tx, ctx, p, req.projectId, req.witnessId, operation);
      const epoch = next(p.owner_epoch); const sequence = await this.#bump(tx, p);
      await tx.query(`UPDATE ${S}.projects SET owner_epoch=$4,owner_engine_instance=NULL,lease_id=NULL,lease_until=NULL,renew_serial=0
        WHERE authority_id=$1 AND account_id=$2 AND project_id=$3`, [this.authorityId, ctx.accountId, req.projectId, epoch]);
      await tx.query(`INSERT INTO ${S}.ownership_receipts(authority_id,account_id,project_id,owner_epoch,operation_id,cause,quiescence_root,receipt_canonical)
        VALUES($1,$2,$3,$4,$5,'released',$6,$7)`, [this.authorityId, ctx.accountId, req.projectId, epoch, operation, root, bytes]);
      return this.#record(tx, ctx, req.projectId, req.idempotencyKey, "owner-release", bytes, sequence, { ownerEpoch: epoch }, operation);
    });
  }
  async acceptDecision(trusted: TrustedContext, input: DecisionRequest): Promise<Outcome<Accepted>> {
    const ctx = context(trusted); exact(input, ["projectId", "idempotencyKey", "ownerEpoch", "leaseId", "topic", "expectedVersion", "statement"]);
    uuid(input.projectId); text(input.idempotencyKey); text(input.topic); counter(input.expectedVersion); fence(input);
    const req = snapshot(input); const bytes = operationBytes(ctx, "decision-accept", req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#member(tx, ctx, req.projectId, "decision:accept", req.topic);
      const prior = await this.#prior(tx, ctx, req.projectId, req.idempotencyKey, "decision-accept", bytes); if (prior) return prior;
      await this.#owner(tx, ctx, p, req);
      const current = await tx.query(`SELECT version::text,decision_id FROM ${S}.topics WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND topic=$4`, [this.authorityId, ctx.accountId, req.projectId, req.topic]);
      if ((current.rows[0]?.version ?? "0") !== req.expectedVersion) fail("topic_conflict");
      const version = next(req.expectedVersion); const sequence = await this.#bump(tx, p); const operation = randomUUID();
      const receipt = await this.#record(tx, ctx, req.projectId, req.idempotencyKey, "decision-accept", bytes, sequence, { topic: req.topic, version }, operation);
      await tx.query(`INSERT INTO ${S}.decisions(authority_id,account_id,project_id,decision_id,topic,version,principal_id,scope,statement,previous_id,project_seq)
        VALUES($1,$2,$3,$4,$5,$6,$7,'project',$8,$9,$10)`, [this.authorityId, ctx.accountId, req.projectId, operation, req.topic, version, ctx.principalId, JSON.stringify(req.statement), current.rows[0]?.decision_id ?? null, sequence]);
      await tx.query(`INSERT INTO ${S}.topics(authority_id,account_id,project_id,topic,version,decision_id) VALUES($1,$2,$3,$4,$5,$6)
        ON CONFLICT(authority_id,account_id,project_id,topic) DO UPDATE SET version=EXCLUDED.version,decision_id=EXCLUDED.decision_id`, [this.authorityId, ctx.accountId, req.projectId, req.topic, version, operation]);
      return receipt;
    });
  }
  async admitVerification(trusted: TrustedContext, input: VerificationAdmission): Promise<Outcome<{ verificationId: string }>> {
    const ctx = context(trusted); exact(input, ["projectId", "ownerEpoch", "leaseId", "verificationId", "baseVersion", "baseHead", "testedHead", "sourceRoot", "environmentRoot", "gateReceiptRoot", "buildContractRoot", "topics"]);
    uuid(input.projectId); uuid(input.verificationId); fence(input); counter(input.baseVersion);
    [input.baseHead, input.testedHead, input.sourceRoot, input.environmentRoot, input.gateReceiptRoot, input.buildContractRoot].forEach(value => text(value));
    if (!input.topics || !Object.hasOwn(input.topics, "policy/verification") || Object.keys(input.topics).length > 64) fail("invalid_request");
    for (const [topic, version] of Object.entries(input.topics)) { text(topic); counter(version); }
    const req = snapshot(input); const bytes = canonical(req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#accountGrant(tx, ctx, "verification:admit");
      const prior = await tx.query(`SELECT admission_canonical FROM ${S}.verifications WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND verification_id=$4`, [this.authorityId, ctx.accountId, req.projectId, req.verificationId]);
      if (prior.rows.length) { if (!prior.rows[0].admission_canonical.equals(bytes)) fail("conflict"); return { verificationId: req.verificationId }; }
      await this.#owner(tx, ctx, p, req);
      if (p.integration_version !== req.baseVersion || p.head_vcs_commit !== req.baseHead) fail("head_conflict");
      const requiredTopics = await tx.query(`SELECT DISTINCT substring(scope FROM 7) AS topic FROM ${S}.project_principals,
        LATERAL jsonb_array_elements_text(scopes) AS granted(scope) WHERE authority_id=$1 AND account_id=$2 AND project_id=$3
        AND revoked_at IS NULL AND scope LIKE 'topic:%'`, [this.authorityId, ctx.accountId, req.projectId]);
      if (requiredTopics.rows.some(row => !Object.hasOwn(req.topics, row.topic))) fail("topic_conflict");
      await this.#roots(tx, ctx.accountId, [[req.sourceRoot, "source"], [req.environmentRoot, "environment"], [req.gateReceiptRoot, "gate-evidence"], [req.buildContractRoot, "build-contract"]]);
      await tx.query(`INSERT INTO ${S}.verifications(authority_id,account_id,project_id,verification_id,owner_epoch,base_version,base_head,tested_head,
        tested_source_root,tested_environment_root,gate_receipt_root,build_contract_root,admission_canonical) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)`,
      [this.authorityId, ctx.accountId, req.projectId, req.verificationId, req.ownerEpoch, req.baseVersion, req.baseHead, req.testedHead, req.sourceRoot, req.environmentRoot, req.gateReceiptRoot, req.buildContractRoot, bytes]);
      for (const [topic, version] of Object.entries(req.topics)) {
        const current = await tx.query(`SELECT version::text FROM ${S}.topics WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND topic=$4`, [this.authorityId, ctx.accountId, req.projectId, topic]);
        if ((current.rows[0]?.version ?? "0") !== version) fail("topic_conflict");
        await tx.query(`INSERT INTO ${S}.verification_topics VALUES($1,$2,$3,$4,$5,$6)`, [this.authorityId, ctx.accountId, req.projectId, req.verificationId, topic, version]);
      }
      return { verificationId: req.verificationId };
    });
  }
  async integrate(trusted: TrustedContext, input: IntegrationRequest): Promise<Outcome<Accepted>> {
    const ctx = context(trusted); exact(input, ["projectId", "idempotencyKey", "ownerEpoch", "leaseId", "expectedVersion", "expectedHead", "newHead", "sourceRoot", "environmentRoot", "verificationId"]);
    uuid(input.projectId); uuid(input.verificationId); text(input.idempotencyKey); fence(input); counter(input.expectedVersion);
    [input.expectedHead, input.newHead, input.sourceRoot, input.environmentRoot].forEach(value => text(value));
    const req = snapshot(input); const bytes = operationBytes(ctx, "integration-accept", req);
    return this.#run(ctx, async tx => {
      const p = await this.#project(tx, ctx, req.projectId); await this.#member(tx, ctx, req.projectId, "integration:accept");
      const prior = await this.#prior(tx, ctx, req.projectId, req.idempotencyKey, "integration-accept", bytes); if (prior) return prior;
      await this.#owner(tx, ctx, p, req); next(p.integration_version); next(p.event_seq);
      await this.#roots(tx, ctx.accountId, [[req.sourceRoot, "source"], [req.environmentRoot, "environment"]]);
      const result = await tx.query(`UPDATE ${S}.projects AS p SET head_vcs_commit=$9,head_source_root=$10,head_environment_root=$11,
        integration_version=integration_version+1,event_seq=event_seq+1
        WHERE p.authority_id=$1 AND p.account_id=$2 AND p.project_id=$3 AND p.mode='ready'
          AND p.owner_epoch=$4 AND p.owner_engine_instance=$5 AND p.lease_id=$6 AND p.lease_until>clock_timestamp()
          AND p.integration_version=$7 AND p.head_vcs_commit=$8 AND EXISTS (
            SELECT 1 FROM ${S}.verifications v WHERE v.authority_id=p.authority_id AND v.account_id=p.account_id AND v.project_id=p.project_id
              AND v.verification_id=$12 AND v.owner_epoch=p.owner_epoch AND v.base_version=p.integration_version AND v.base_head=p.head_vcs_commit
              AND v.tested_head=$9 AND v.tested_source_root=$10 AND v.tested_environment_root=$11
              AND EXISTS(SELECT 1 FROM ${S}.verification_topics required WHERE required.authority_id=v.authority_id AND required.account_id=v.account_id
                AND required.project_id=v.project_id AND required.verification_id=v.verification_id AND required.topic='policy/verification')
              AND NOT EXISTS(SELECT 1 FROM ${S}.project_principals member,LATERAL jsonb_array_elements_text(member.scopes) AS granted(scope)
                WHERE member.authority_id=p.authority_id AND member.account_id=p.account_id AND member.project_id=p.project_id
                  AND member.revoked_at IS NULL AND scope LIKE 'topic:%' AND NOT EXISTS(SELECT 1 FROM ${S}.verification_topics coverage
                    WHERE coverage.authority_id=v.authority_id AND coverage.account_id=v.account_id AND coverage.project_id=v.project_id
                      AND coverage.verification_id=v.verification_id AND coverage.topic=substring(scope FROM 7)))
              AND NOT EXISTS(SELECT 1 FROM ${S}.verification_topics vt LEFT JOIN ${S}.topics t ON t.authority_id=vt.authority_id
                AND t.account_id=vt.account_id AND t.project_id=vt.project_id AND t.topic=vt.topic
                WHERE vt.authority_id=v.authority_id AND vt.account_id=v.account_id AND vt.project_id=v.project_id
                  AND vt.verification_id=v.verification_id AND COALESCE(t.version,0)<>vt.expected_version))
          AND NOT EXISTS(SELECT 1 FROM ${S}.command_effects effect WHERE effect.authority_id=p.authority_id AND effect.account_id=p.account_id
            AND effect.project_id=p.project_id AND effect.effect_scope IN ('integration','delivery') AND effect.state IN ('started','uncertain'))
        RETURNING integration_version::text,event_seq::text`,
      [this.authorityId, ctx.accountId, req.projectId, req.ownerEpoch, ctx.engine!.instanceId, req.leaseId, req.expectedVersion, req.expectedHead, req.newHead, req.sourceRoot, req.environmentRoot, req.verificationId]);
      if (!result.rows.length) fail("head_conflict");
      const sequence = result.rows[0].event_seq; const version = result.rows[0].integration_version; const operation = randomUUID();
      const receipt = await this.#record(tx, ctx, req.projectId, req.idempotencyKey, "integration-accept", bytes, sequence, { version, head: req.newHead }, operation);
      await tx.query(`INSERT INTO ${S}.integration_commits(authority_id,account_id,project_id,integration_id,version,previous_version,owner_epoch,
        previous_head,new_head,source_root,environment_root,verification_id,project_seq) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)`,
      [this.authorityId, ctx.accountId, req.projectId, operation, version, req.expectedVersion, req.ownerEpoch, req.expectedHead, req.newHead, req.sourceRoot, req.environmentRoot, req.verificationId, sequence]);
      return receipt;
    });
  }
  async getReceipt(trusted: TrustedContext, projectId: string, key: string): Promise<Outcome<Accepted | null>> {
    const ctx = context(trusted); uuid(projectId); text(key);
    return this.#run(ctx, async tx => {
      await this.#project(tx, ctx, projectId); await this.#member(tx, ctx, projectId, "receipt:read");
      const row = await tx.query(`SELECT accepted_receipt FROM ${S}.idempotency WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND principal_id=$4 AND idempotency_key=$5`, [this.authorityId, ctx.accountId, projectId, ctx.principalId, key]);
      return row.rows[0]?.accepted_receipt ?? null;
    });
  }
}
