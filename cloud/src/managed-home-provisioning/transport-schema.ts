import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {check,fail} from "./types.js";
export const TRANSPORT_SQL=`
-- SELECT FOR SHARE requires UPDATE privilege on at least one column. Keep
-- revocation, readiness and credential fields outside the registration role.
-- These otherwise unused columns grant row-lock permission only; their values
-- never authorize a transport, engine, lease or policy change.
ALTER TABLE ${SCHEMA}.authorities ADD COLUMN transport_lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.authority_credentials ADD COLUMN transport_lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.engine_instances ADD COLUMN transport_lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.managed_home_installation ADD COLUMN transport_lock_version bigint NOT NULL DEFAULT 0;
CREATE TABLE ${SCHEMA}.managed_host_transport_installation(authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,state text NOT NULL CHECK(state IN('frozen','ready','retired')),migration_sha256 text NOT NULL);
CREATE TABLE ${SCHEMA}.managed_host_transports(
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,engine_instance_id uuid NOT NULL,boot_id text NOT NULL,
 serial bigint NOT NULL CHECK(serial>0),generation uuid NOT NULL,attempt_nonce uuid NOT NULL,request_sha256 text NOT NULL,
 request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=16384),challenge_json jsonb NOT NULL CHECK(pg_column_size(challenge_json)<=16384),
 original_end timestamptz NOT NULL,state text NOT NULL CHECK(state IN('pending','committed','superseded')),commit_sha256 text,ack_json jsonb CHECK(pg_column_size(ack_json)<=16384),
 PRIMARY KEY(authority_id,account_id,home_id,serial),UNIQUE(authority_id,account_id,attempt_nonce));
CREATE UNIQUE INDEX managed_host_one_current_transport ON ${SCHEMA}.managed_host_transports(authority_id,account_id,home_id) WHERE state='committed';
ALTER TABLE ${SCHEMA}.managed_host_transports ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_host_transports FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.managed_host_transports USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE TABLE ${SCHEMA}.managed_host_transport_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
`;
export const TRANSPORT_MIGRATION_SHA256=createHash("sha256").update(TRANSPORT_SQL).digest("hex");
export async function migrateManagedHostTransports(pool:AuthorityPool,authorityId:string,role:string){check(pool.role==="migration"&&/^[a-z][a-z0-9_]{0,62}$/.test(role));return pool.run("",async tx=>{
 const renewal=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_owner_renewals"]);check(Boolean(renewal.rows[0]?.name),"unconfigured");
 await tx.query("SELECT pg_advisory_xact_lock(1824529,6)");const prior=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_host_transport_migrations"]);
 if(!prior.rows[0].name){await tx.query(TRANSPORT_SQL);await tx.query(`INSERT INTO ${SCHEMA}.managed_host_transport_migrations VALUES(1,$1)`,[TRANSPORT_MIGRATION_SHA256]);}
 const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_host_transport_migrations WHERE version=1`);check(version.rows[0]?.sha256===TRANSPORT_MIGRATION_SHA256,"unconfigured");
 await tx.query(`INSERT INTO ${SCHEMA}.managed_host_transport_installation VALUES($1,'frozen',$2) ON CONFLICT DO NOTHING`,[authorityId,TRANSPORT_MIGRATION_SHA256]);
 const safety=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safety.rows.length===1&&!safety.rows[0].unsafe,"unconfigured");
 const r='"'+role+'"';await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_host_transport_installation,${SCHEMA}.managed_host_transports,${SCHEMA}.managed_host_transport_migrations FROM PUBLIC;
 GRANT SELECT ON ${SCHEMA}.managed_host_transport_installation,${SCHEMA}.managed_host_transports,${SCHEMA}.managed_host_transport_migrations TO ${r};
 GRANT SELECT ON ${SCHEMA}.managed_owner_renewals TO ${r};
 GRANT UPDATE(transport_lock_version) ON ${SCHEMA}.authorities,${SCHEMA}.authority_credentials,${SCHEMA}.engine_instances,${SCHEMA}.managed_home_installation TO ${r};
 GRANT INSERT,DELETE ON ${SCHEMA}.managed_host_transports TO ${r};GRANT UPDATE(state,commit_sha256,ack_json) ON ${SCHEMA}.managed_host_transports TO ${r};`);return {state:"frozen-or-existing" as const};
 }).catch(()=>fail("unavailable"));}
