import { createHash } from "node:crypto";
import { AuthorityPool, SCHEMA, RESTRICTED_ROLE_CHECK } from "../project-authority/pool.js";
import { check, fail } from "./types.js";

const tables=["managed_home_intents","managed_home_effects","managed_home_projects","managed_home_fresh_projects","managed_home_registrations","managed_home_commands","managed_home_outbox"];
export const schemaSql=`
CREATE TABLE ${SCHEMA}.managed_home_installation (
 authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,
 state text NOT NULL CHECK(state IN ('frozen','ready','retired')),
 policy_sha256 text NOT NULL CHECK(policy_sha256 ~ '^[0-9a-f]{64}$'), migration_sha256 text NOT NULL
);
CREATE TABLE ${SCHEMA}.managed_home_intents (
 authority_id uuid NOT NULL REFERENCES ${SCHEMA}.managed_home_installation,
 account_id text NOT NULL, home_id text NOT NULL, command_key text NOT NULL,
 request_sha256 text NOT NULL, epoch bigint NOT NULL CHECK(epoch=1), nonce uuid NOT NULL UNIQUE,
 app_name text NOT NULL UNIQUE, network_name text NOT NULL UNIQUE, process_owner uuid NOT NULL,
 intent_json jsonb NOT NULL CHECK(pg_column_size(intent_json)<=65536),
 token_cipher bytea NOT NULL CHECK(octet_length(token_cipher)<=512), token_sha256 text NOT NULL,
 registration_cipher bytea NOT NULL CHECK(octet_length(registration_cipher)<=512),
 phase text NOT NULL CHECK(phase IN ('pending','effect_unknown','resource_provisioned','routable')),
 revoked_at timestamptz,
 original_end timestamptz NOT NULL, created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 lock_version bigint NOT NULL DEFAULT 0,
 PRIMARY KEY(authority_id,account_id,home_id), UNIQUE(authority_id,account_id,command_key)
);
CREATE TABLE ${SCHEMA}.managed_home_effects (
 authority_id uuid NOT NULL, account_id text NOT NULL, home_id text NOT NULL,
 ordinal integer NOT NULL CHECK(ordinal BETWEEN 0 AND 7), kind text NOT NULL,
 request_sha256 text NOT NULL,request_json jsonb CHECK(pg_column_size(request_json)<=131072),state text NOT NULL CHECK(state IN ('planned','started','unknown','acknowledged')),
 original_ack jsonb CHECK(pg_column_size(original_ack)<=131072), ack_sha256 text,
 started_at timestamptz, PRIMARY KEY(authority_id,account_id,home_id,ordinal),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents,
 CHECK((state='acknowledged')=(original_ack IS NOT NULL))
);
CREATE TABLE ${SCHEMA}.managed_home_projects (
 authority_id uuid NOT NULL, account_id text NOT NULL, home_id text NOT NULL, workspace_id text NOT NULL,
 PRIMARY KEY(authority_id,account_id,workspace_id),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
-- Bounded exact-home locator only. SELECT grants no authentication or routing;
-- token hash and all mutable home/effect data remain in tenant-RLS tables.
CREATE TABLE ${SCHEMA}.managed_home_locators (
 authority_id uuid NOT NULL,home_id text NOT NULL,account_id text NOT NULL,
 PRIMARY KEY(authority_id,home_id),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
ALTER TABLE ${SCHEMA}.managed_home_locators ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_home_locators FORCE ROW LEVEL SECURITY;
CREATE POLICY locate ON ${SCHEMA}.managed_home_locators FOR SELECT USING(true);
CREATE POLICY reserve_locator ON ${SCHEMA}.managed_home_locators FOR INSERT WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE TABLE ${SCHEMA}.managed_home_fresh_projects (
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,workspace_id text NOT NULL,
 project_id uuid NOT NULL,prepare_id uuid NOT NULL,title text NOT NULL,project_key text NOT NULL,
 origin text NOT NULL CHECK(origin='fresh-v4-empty'),
 assignment_json jsonb CHECK(pg_column_size(assignment_json)<=8192),
 PRIMARY KEY(authority_id,account_id,project_id),UNIQUE(authority_id,account_id,workspace_id),UNIQUE(authority_id,account_id,project_key),
 FOREIGN KEY(authority_id,account_id,workspace_id) REFERENCES ${SCHEMA}.managed_home_projects
);
CREATE TABLE ${SCHEMA}.managed_home_registrations (
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,root_nonce uuid NOT NULL,
 begin_sha256 text NOT NULL,challenge_json jsonb NOT NULL CHECK(pg_column_size(challenge_json)<=24576),
 original_end timestamptz NOT NULL,phase text NOT NULL CHECK(phase IN ('pending','enrolled')),
 proof_sha256 text,proof_json jsonb CHECK(pg_column_size(proof_json)<=24576),ack_json jsonb CHECK(pg_column_size(ack_json)<=24576),
 PRIMARY KEY(authority_id,account_id,home_id),UNIQUE(authority_id,account_id,root_nonce),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
CREATE TABLE ${SCHEMA}.managed_home_commands (
 authority_id uuid NOT NULL, account_id text NOT NULL, home_id text NOT NULL, command_key text NOT NULL,
 payload_sha256 text NOT NULL, operation_json jsonb NOT NULL CHECK(pg_column_size(operation_json)<=32768),
 process_owner uuid NOT NULL, phase text NOT NULL CHECK(phase IN ('accepted','started','unknown','replied')),
 original_reply jsonb CHECK(pg_column_size(original_reply)<=65536), reply_sha256 text,
 original_end timestamptz NOT NULL, PRIMARY KEY(authority_id,account_id,home_id,command_key),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
CREATE TABLE ${SCHEMA}.managed_home_outbox (
 authority_id uuid NOT NULL, account_id text NOT NULL, home_id text NOT NULL, event_key text NOT NULL,
 kind text NOT NULL CHECK(kind IN ('provision','command')), payload_sha256 text NOT NULL,
 PRIMARY KEY(authority_id,account_id,home_id,event_key),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
CREATE TABLE ${SCHEMA}.managed_home_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
${tables.map(t=>`ALTER TABLE ${SCHEMA}.${t} ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.${t} FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.${t} USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));`).join("\n")}
`;
export const MIGRATION_SHA256=createHash("sha256").update(schemaSql).digest("hex");
/** Separate explicit admin migration. Never called by registry construction,
 * request handling or outbox draining. Deliberately creates FROZEN admission. */
export async function migrateManagedHomes(pool:AuthorityPool,authorityId:string,policyHash:string,runtimeRole:string,registrationRole?:string){
  check(pool.role==="migration"&&/^[a-z][a-z0-9_]{0,62}$/.test(runtimeRole));
  return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,2)");
    const exists=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_home_migrations"]);
    if(!exists.rows[0].name){await tx.query(schemaSql);await tx.query(`INSERT INTO ${SCHEMA}.managed_home_migrations VALUES(1,$1)`,[MIGRATION_SHA256]);
      await tx.query(`INSERT INTO ${SCHEMA}.managed_home_installation VALUES($1,'frozen',$2,$3)`,[authorityId,policyHash,MIGRATION_SHA256]);}
    const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_home_migrations WHERE version=1`);
    const installation=await tx.query(`SELECT policy_sha256,migration_sha256 FROM ${SCHEMA}.managed_home_installation WHERE authority_id=$1`,[authorityId]);
    check(version.rows[0]?.sha256===MIGRATION_SHA256&&installation.rows[0]?.policy_sha256===policyHash&&installation.rows[0]?.migration_sha256===MIGRATION_SHA256,"unconfigured");
    const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,runtimeRole]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");
    const role='"'+runtimeRole+'"';await tx.query(`GRANT USAGE ON SCHEMA ${SCHEMA} TO ${role};
      GRANT SELECT ON ${SCHEMA}.managed_home_installation,${SCHEMA}.managed_home_migrations TO ${role};
      REVOKE ALL ON ${SCHEMA}.managed_home_locators FROM PUBLIC;
      GRANT SELECT,INSERT ON ${SCHEMA}.managed_home_locators TO ${role};
      ${tables.map(t=>`REVOKE ALL ON ${SCHEMA}.${t} FROM PUBLIC;GRANT ${t==="managed_home_registrations"?"SELECT":"SELECT,INSERT"} ON ${SCHEMA}.${t} TO ${role};`).join("\n")}
      GRANT UPDATE(phase) ON ${SCHEMA}.managed_home_intents TO ${role};
      GRANT UPDATE(state,request_sha256,request_json,original_ack,ack_sha256,started_at) ON ${SCHEMA}.managed_home_effects TO ${role};
      GRANT UPDATE(phase,original_reply,reply_sha256) ON ${SCHEMA}.managed_home_commands TO ${role};`);
    await tx.query(`GRANT UPDATE(assignment_json) ON ${SCHEMA}.managed_home_fresh_projects TO ${role}`);
    if(registrationRole){check(/^[a-z][a-z0-9_]{0,62}$/.test(registrationRole)&&registrationRole!==runtimeRole);const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,registrationRole]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");const r='"'+registrationRole+'"';await tx.query(`GRANT USAGE ON SCHEMA ${SCHEMA} TO ${r};
      GRANT SELECT ON ${["authorities","managed_home_installation","managed_home_intents","managed_home_registrations","authority_credentials","engine_instances"].map(t=>SCHEMA+"."+t).join(",")} TO ${r};
      GRANT INSERT ON ${SCHEMA}.managed_home_registrations,${SCHEMA}.authority_credentials,${SCHEMA}.engine_instances TO ${r};
      GRANT UPDATE(phase,proof_sha256,proof_json,ack_json) ON ${SCHEMA}.managed_home_registrations TO ${r};
      GRANT UPDATE(lock_version) ON ${SCHEMA}.managed_home_intents TO ${r};`);}
    return {version:1,state:"frozen-or-existing" as const};
  }).catch(()=>fail("unavailable"));
}
