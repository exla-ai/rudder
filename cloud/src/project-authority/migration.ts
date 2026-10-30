import { createHash } from "node:crypto";
import { SCHEMA, AuthorityPool, RESTRICTED_ROLE_CHECK } from "./pool.js";
import { schemaSql } from "./schema.js";
import { fail, uuid, type Outcome } from "./types.js";

const additions = `
CREATE TABLE ${SCHEMA}.account_principals (
 authority_id uuid NOT NULL REFERENCES ${SCHEMA}.authorities,
 account_id text NOT NULL, principal_id text NOT NULL,
 operations jsonb NOT NULL CHECK(jsonb_typeof(operations)='array'), revoked_at timestamptz,
 PRIMARY KEY(authority_id,account_id,principal_id)
);
CREATE TABLE ${SCHEMA}.handoff_witnesses (
 authority_id uuid NOT NULL REFERENCES ${SCHEMA}.authorities,
 account_id text NOT NULL, project_id uuid NOT NULL, witness_id uuid NOT NULL,
 root_id text NOT NULL, previous_owner_epoch bigint NOT NULL CHECK(previous_owner_epoch>=0),
 previous_engine_instance uuid, legacy_origin text NOT NULL,
 processes_quiesced boolean NOT NULL CHECK(processes_quiesced),
 effects_reconciled boolean NOT NULL CHECK(effects_reconciled),
 launchers_disabled boolean NOT NULL CHECK(launchers_disabled),
 credentials_fenced boolean NOT NULL CHECK(credentials_fenced),
 request_canonical bytea NOT NULL, consumed_operation_id uuid,
 PRIMARY KEY(authority_id,account_id,project_id,witness_id),
 FOREIGN KEY(account_id,root_id) REFERENCES ${SCHEMA}.retained_roots,
 FOREIGN KEY(account_id,previous_engine_instance) REFERENCES ${SCHEMA}.engine_instances
);
ALTER TABLE ${SCHEMA}.verifications ADD COLUMN admission_canonical bytea NOT NULL;
ALTER TABLE ${SCHEMA}.authorities ADD COLUMN lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.authority_credentials ADD COLUMN lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.engine_instances ADD COLUMN lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.project_principals ADD COLUMN lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.account_principals ADD COLUMN lock_version bigint NOT NULL DEFAULT 0;
CREATE TABLE ${SCHEMA}.migration_versions(version integer PRIMARY KEY,sha256 text NOT NULL);
`;
const accountTables = ["authority_credentials", "engine_instances", "retained_roots", "projects", "ownership_receipts", "project_principals", "runs", "run_attempts", "lease_renewals", "events", "idempotency", "capabilities", "capability_targets", "commands", "command_effects", "effect_observations", "outbox", "mailboxes", "messages", "mailbox_deliveries", "decisions", "topics", "verifications", "verification_topics", "integration_commits", "cutovers", "account_principals", "handoff_witnesses"];
const policies = accountTables.map(table => `ALTER TABLE ${SCHEMA}.${table} ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.${table} USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));`).join("\n");
export const MIGRATION_SHA256 = createHash("sha256").update(schemaSql + additions + policies).digest("hex");

/** Explicit administrator-only operation. Creates a FROZEN incarnation; never activates authority. */
export async function migrateAuthority(pool: AuthorityPool, options: { authorityId: string; runtimeRole: string }): Promise<Outcome<{ version: 1; state: "frozen" | "existing" }>> {
  if (pool.role !== "migration") fail("forbidden");
  uuid(options.authorityId);
  if (!/^[a-z][a-z0-9_]{0,62}$/.test(options.runtimeRole)) fail("invalid_request");
  const role = '"' + options.runtimeRole + '"';
  return pool.run("", async tx => {
    await tx.query("SELECT pg_advisory_xact_lock(1824529,1)");
    const check = await tx.query("SELECT to_regnamespace($1)::text AS name", [SCHEMA]);
    let created = false;
    if (!check.rows[0].name) {
      await tx.query(schemaSql + additions + policies);
      await tx.query(`INSERT INTO ${SCHEMA}.migration_versions VALUES(1,$1)`, [MIGRATION_SHA256]);
      await tx.query(`INSERT INTO ${SCHEMA}.authorities(authority_id,state,protocol_version) VALUES($1,'frozen',1)`, [options.authorityId]);
      created = true;
    } else {
      const version = await tx.query(`SELECT sha256 FROM ${SCHEMA}.migration_versions WHERE version=1`);
      const authority = await tx.query(`SELECT authority_id FROM ${SCHEMA}.authorities WHERE state<>'retired'`);
      if (version.rows[0]?.sha256 !== MIGRATION_SHA256 || authority.rows.length !== 1 || authority.rows[0].authority_id !== options.authorityId) fail("migration_conflict");
    }
    const runtime = await tx.query(RESTRICTED_ROLE_CHECK, [SCHEMA, options.runtimeRole]);
    if (runtime.rows.length !== 1 || runtime.rows[0].unsafe) fail("role_rejected");
    await tx.query(`REVOKE ALL ON SCHEMA ${SCHEMA} FROM PUBLIC;
      REVOKE ALL ON ALL TABLES IN SCHEMA ${SCHEMA} FROM PUBLIC;
      REVOKE ALL ON ALL FUNCTIONS IN SCHEMA ${SCHEMA} FROM PUBLIC;
      REVOKE ALL ON SCHEMA ${SCHEMA} FROM ${role};
      REVOKE ALL ON ALL TABLES IN SCHEMA ${SCHEMA} FROM ${role};
      GRANT USAGE ON SCHEMA ${SCHEMA} TO ${role};
      GRANT SELECT ON ALL TABLES IN SCHEMA ${SCHEMA} TO ${role};
      GRANT INSERT ON ${["projects", "ownership_receipts", "project_principals", "retained_roots", "handoff_witnesses", "lease_renewals", "events", "idempotency", "outbox", "decisions", "topics", "verifications", "verification_topics", "integration_commits"].map(table => `${SCHEMA}.${table}`).join(",")} TO ${role};
      GRANT UPDATE(mode,owner_epoch,owner_engine_instance,lease_id,lease_until,renew_serial,event_seq,integration_version,head_vcs_commit,head_source_root,head_environment_root) ON ${SCHEMA}.projects TO ${role};
      GRANT UPDATE(version,decision_id) ON ${SCHEMA}.topics TO ${role};
      GRANT UPDATE(consumed_operation_id) ON ${SCHEMA}.handoff_witnesses TO ${role};`);
    for (const table of ["authorities", "authority_credentials", "engine_instances", "project_principals", "account_principals"]) {
      await tx.query(`GRANT UPDATE(lock_version) ON ${SCHEMA}.${table} TO ${role}`);
    }
    return { version: 1, state: created ? "frozen" : "existing" };
  });
}
