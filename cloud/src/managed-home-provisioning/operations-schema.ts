import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {check,fail} from "./types.js";
export const operationsSql=`
CREATE TABLE ${SCHEMA}.managed_home_fresh_preparations (
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,project_id uuid NOT NULL,
 command_key text NOT NULL,engine_instance_id uuid NOT NULL,boot_id text NOT NULL,
 request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=24576),receipt_json jsonb NOT NULL CHECK(pg_column_size(receipt_json)<=16384),
 PRIMARY KEY(authority_id,account_id,project_id),UNIQUE(authority_id,account_id,home_id,command_key),
 FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${SCHEMA}.managed_home_fresh_projects
);
CREATE TABLE ${SCHEMA}.managed_home_engine_operations (
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,command_key text NOT NULL,
 engine_instance_id uuid NOT NULL,request_nonce uuid NOT NULL,payload_sha256 text NOT NULL,
 request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=24576),
 state text NOT NULL CHECK(state IN ('started','replied')),original_end timestamptz NOT NULL,
 ack_json jsonb CHECK(pg_column_size(ack_json)<=24576),ack_sha256 text,
 PRIMARY KEY(authority_id,account_id,home_id,command_key),UNIQUE(authority_id,account_id,request_nonce),
 FOREIGN KEY(authority_id,account_id,home_id) REFERENCES ${SCHEMA}.managed_home_intents
);
CREATE TABLE ${SCHEMA}.managed_home_operations_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
${["managed_home_fresh_preparations","managed_home_engine_operations"].map(t=>`ALTER TABLE ${SCHEMA}.${t} ENABLE ROW LEVEL SECURITY;ALTER TABLE ${SCHEMA}.${t} FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.${t} USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));`).join("\n")}
`;
export const OPERATIONS_MIGRATION_SHA256=createHash("sha256").update(operationsSql).digest("hex");
/** Separate explicit v2 install; v1 schema/hash and seals are unchanged. */
export async function migrateManagedHomeOperations(pool:AuthorityPool,runtimeRole:string,registrationRole:string){check(pool.role==="migration");for(const r of [runtimeRole,registrationRole])check(/^[a-z][a-z0-9_]{0,62}$/.test(r));check(runtimeRole!==registrationRole);
  return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,3)");const exists=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_home_operations_migrations"]);
    if(!exists.rows[0].name){await tx.query(operationsSql);await tx.query(`INSERT INTO ${SCHEMA}.managed_home_operations_migrations VALUES(2,$1)`,[OPERATIONS_MIGRATION_SHA256]);}
    const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_home_operations_migrations WHERE version=2`);check(version.rows[0]?.sha256===OPERATIONS_MIGRATION_SHA256,"unconfigured");
    for(const role of [runtimeRole,registrationRole]){const r=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(r.rows.length===1&&!r.rows[0].unsafe,"unconfigured");}
    const r='"'+runtimeRole+'"',g='"'+registrationRole+'"';await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_home_fresh_preparations,${SCHEMA}.managed_home_engine_operations,${SCHEMA}.managed_home_operations_migrations FROM PUBLIC;
      GRANT SELECT ON ${SCHEMA}.managed_home_fresh_preparations,${SCHEMA}.managed_home_operations_migrations TO ${r};
      GRANT INSERT ON ${SCHEMA}.managed_home_fresh_preparations TO ${r};
      GRANT SELECT ON ${SCHEMA}.managed_home_engine_operations,${SCHEMA}.managed_home_operations_migrations TO ${g};
      GRANT INSERT ON ${SCHEMA}.managed_home_engine_operations,${SCHEMA}.managed_home_outbox TO ${g};
      GRANT UPDATE(state,ack_json,ack_sha256) ON ${SCHEMA}.managed_home_engine_operations TO ${g};`);return {version:2,state:"explicit-install-only" as const};
  }).catch(()=>fail("unavailable"));}
