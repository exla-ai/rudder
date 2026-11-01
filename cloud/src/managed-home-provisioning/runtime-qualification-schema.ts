import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {TRANSPORT_MIGRATION_SHA256} from "./transport-schema.js";
import {check,fail,sha} from "./types.js";
export const RUNTIME_QUALIFICATION_SQL=`
CREATE TABLE ${SCHEMA}.managed_runtime_role_installation(
 authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,
 state text NOT NULL CHECK(state IN('frozen','ready','retired')),
 policy_sha256 text NOT NULL,migration_sha256 text NOT NULL);
CREATE TABLE ${SCHEMA}.managed_runtime_qualifications(
 authority_id uuid NOT NULL,account_id text NOT NULL,qualification_id uuid NOT NULL,
 home_id text NOT NULL,process_owner uuid NOT NULL,provision_nonce uuid NOT NULL,
 request_sha256 text NOT NULL,scope_json jsonb NOT NULL CHECK(pg_column_size(scope_json)<=8192),
 original_end timestamptz NOT NULL,state text NOT NULL DEFAULT 'reserved' CHECK(state IN('reserved','started','published')),
 ready_request_sha256 text,ready_request_json jsonb CHECK(pg_column_size(ready_request_json)<=24576),
 registration_proof_sha256 text,ack_json jsonb CHECK(pg_column_size(ack_json)<=8192),
 lock_version bigint NOT NULL DEFAULT 0,
 PRIMARY KEY(authority_id,account_id,qualification_id),UNIQUE(authority_id,qualification_id),UNIQUE(authority_id,account_id,home_id),
 CHECK((state='published')=(ack_json IS NOT NULL)),
 CHECK((state='reserved')=(ready_request_sha256 IS NULL)));
ALTER TABLE ${SCHEMA}.managed_runtime_qualifications ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_runtime_qualifications FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.managed_runtime_qualifications USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE TABLE ${SCHEMA}.managed_runtime_image_roles(
 authority_id uuid NOT NULL REFERENCES ${SCHEMA}.authorities,role_binding_sha256 text NOT NULL,
 image text NOT NULL,image_receipt_sha256 text NOT NULL,inventory_sha256 text NOT NULL,
 binding_json jsonb NOT NULL CHECK(pg_column_size(binding_json)<=8192),
 qualification_id uuid NOT NULL,ready_request_sha256 text NOT NULL,
 PRIMARY KEY(authority_id,role_binding_sha256),UNIQUE(authority_id,image,image_receipt_sha256,inventory_sha256),
 FOREIGN KEY(authority_id,qualification_id) REFERENCES ${SCHEMA}.managed_runtime_qualifications(authority_id,qualification_id));
ALTER TABLE ${SCHEMA}.managed_runtime_image_roles ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_runtime_image_roles FORCE ROW LEVEL SECURITY;
-- Only public immutable image-role DATA is global. Root proofs/account tokens
-- remain in the tenant-RLS ledger. Inserts require this tenant's started run.
CREATE POLICY role_read ON ${SCHEMA}.managed_runtime_image_roles FOR SELECT USING(true);
CREATE POLICY role_insert ON ${SCHEMA}.managed_runtime_image_roles FOR INSERT WITH CHECK(EXISTS(
 SELECT 1 FROM ${SCHEMA}.managed_runtime_qualifications q WHERE q.authority_id=managed_runtime_image_roles.authority_id
 AND q.qualification_id=managed_runtime_image_roles.qualification_id AND q.account_id=current_setting('rudder.account_id',true) AND q.state='started'));
CREATE TABLE ${SCHEMA}.managed_runtime_role_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
`;
export const RUNTIME_QUALIFICATION_MIGRATION_SHA256=createHash("sha256").update(RUNTIME_QUALIFICATION_SQL).digest("hex");
/** Explicit operator DDL only. FROZEN admits fixed disposable qualification,
 * never normal runtime provisioning. Production also needs the actual immutable
 * role row AND explicit ready installation. Old migrations remain byte-exact. */
export async function migrateManagedRuntimeImageQualifications(pool:AuthorityPool,authorityId:string,policySha256:string,runtimeRole:string,registrationRole:string){
 check(pool.role==="migration"&&runtimeRole!==registrationRole);sha(policySha256);
 for(const r of [runtimeRole,registrationRole])check(/^[a-z][a-z0-9_]{0,62}$/.test(r));
 return pool.run("",async tx=>{
  await tx.query("SELECT pg_advisory_xact_lock(1824529,10)");
  const transport=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_host_transport_migrations WHERE version=1`);
  check(transport.rows[0]?.sha256===TRANSPORT_MIGRATION_SHA256,"unconfigured");
  const prior=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_runtime_role_migrations"]);
  if(!prior.rows[0]?.name){await tx.query(RUNTIME_QUALIFICATION_SQL);await tx.query(`INSERT INTO ${SCHEMA}.managed_runtime_role_migrations VALUES(1,$1)`,[RUNTIME_QUALIFICATION_MIGRATION_SHA256]);}
  const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_runtime_role_migrations WHERE version=1`);check(version.rows[0]?.sha256===RUNTIME_QUALIFICATION_MIGRATION_SHA256,"unconfigured");
  await tx.query(`INSERT INTO ${SCHEMA}.managed_runtime_role_installation VALUES($1,'frozen',$2,$3) ON CONFLICT DO NOTHING`,[authorityId,policySha256,RUNTIME_QUALIFICATION_MIGRATION_SHA256]);
  const row=await tx.query(`SELECT policy_sha256,migration_sha256 FROM ${SCHEMA}.managed_runtime_role_installation WHERE authority_id=$1`,[authorityId]);check(row.rows[0]?.policy_sha256===policySha256&&row.rows[0]?.migration_sha256===RUNTIME_QUALIFICATION_MIGRATION_SHA256,"unconfigured");
  for(const role of [runtimeRole,registrationRole]){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");}
  const r='"'+runtimeRole+'"',a='"'+registrationRole+'"';
  await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_runtime_role_installation,${SCHEMA}.managed_runtime_qualifications,${SCHEMA}.managed_runtime_image_roles,${SCHEMA}.managed_runtime_role_migrations FROM PUBLIC;
   GRANT SELECT ON ${SCHEMA}.managed_runtime_role_installation,${SCHEMA}.managed_runtime_qualifications,${SCHEMA}.managed_runtime_image_roles,${SCHEMA}.managed_runtime_role_migrations TO ${r},${a};
   GRANT INSERT(authority_id,account_id,qualification_id,home_id,process_owner,provision_nonce,request_sha256,scope_json,original_end) ON ${SCHEMA}.managed_runtime_qualifications TO ${r};
   GRANT UPDATE(lock_version) ON ${SCHEMA}.managed_runtime_qualifications TO ${r};
   GRANT UPDATE(state,ready_request_sha256,ready_request_json,registration_proof_sha256,ack_json) ON ${SCHEMA}.managed_runtime_qualifications TO ${a};
   GRANT INSERT ON ${SCHEMA}.managed_runtime_image_roles TO ${a};`);
  return {state:"frozen-or-existing" as const,migrationSha256:RUNTIME_QUALIFICATION_MIGRATION_SHA256};
 }).catch(()=>fail("unavailable"));
}
