import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {RUNTIME_QUALIFICATION_MIGRATION_SHA256} from "./runtime-qualification-schema.js";
import {check,fail} from "./types.js";
/** Additive to the frozen v1 migration. A prepared role is DATA until a private
 * original known-COMMIT witness has actually been issued and durably recorded. */
export const RUNTIME_ROLE_COMPLETION_SQL=`
CREATE TABLE ${SCHEMA}.managed_runtime_role_completions(
 authority_id uuid NOT NULL,role_binding_sha256 text NOT NULL,
 qualification_id uuid NOT NULL,ready_request_sha256 text NOT NULL,
 completion_sha256 text NOT NULL,completion_json jsonb NOT NULL CHECK(pg_column_size(completion_json)<=16384),
 PRIMARY KEY(authority_id,role_binding_sha256),
 FOREIGN KEY(authority_id,role_binding_sha256) REFERENCES ${SCHEMA}.managed_runtime_image_roles,
 FOREIGN KEY(authority_id,qualification_id) REFERENCES ${SCHEMA}.managed_runtime_qualifications(authority_id,qualification_id));
ALTER TABLE ${SCHEMA}.managed_runtime_role_completions ENABLE ROW LEVEL SECURITY;
ALTER TABLE ${SCHEMA}.managed_runtime_role_completions FORCE ROW LEVEL SECURITY;
CREATE POLICY completion_read ON ${SCHEMA}.managed_runtime_role_completions FOR SELECT USING(true);
CREATE POLICY completion_insert ON ${SCHEMA}.managed_runtime_role_completions FOR INSERT WITH CHECK(EXISTS(
 SELECT 1 FROM ${SCHEMA}.managed_runtime_qualifications q WHERE q.authority_id=managed_runtime_role_completions.authority_id
 AND q.qualification_id=managed_runtime_role_completions.qualification_id AND q.account_id=current_setting('rudder.account_id',true)
 AND q.state='published' AND q.ready_request_sha256=managed_runtime_role_completions.ready_request_sha256));
CREATE TABLE ${SCHEMA}.managed_runtime_role_completion_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
`;
export const RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256=createHash("sha256").update(RUNTIME_ROLE_COMPLETION_SQL).digest("hex");
export async function migrateManagedRuntimeRoleCompletions(pool:AuthorityPool,runtimeRole:string,registrationRole:string){
 check(pool.role==="migration"&&runtimeRole!==registrationRole);for(const r of [runtimeRole,registrationRole])check(/^[a-z][a-z0-9_]{0,62}$/.test(r));
 return pool.run("",async tx=>{
  await tx.query("SELECT pg_advisory_xact_lock(1824529,11)");
  const original=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_runtime_role_migrations WHERE version=1`);check(original.rows[0]?.sha256===RUNTIME_QUALIFICATION_MIGRATION_SHA256,"unconfigured");
  const prior=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_runtime_role_completion_migrations"]);
  if(!prior.rows[0]?.name){await tx.query(RUNTIME_ROLE_COMPLETION_SQL);await tx.query(`INSERT INTO ${SCHEMA}.managed_runtime_role_completion_migrations VALUES(1,$1)`,[RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256]);}
  const version=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_runtime_role_completion_migrations WHERE version=1`);check(version.rows[0]?.sha256===RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256,"unconfigured");
  for(const role of [runtimeRole,registrationRole]){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");}
  const r='"'+runtimeRole+'"',a='"'+registrationRole+'"';
  await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_runtime_role_completions,${SCHEMA}.managed_runtime_role_completion_migrations FROM PUBLIC;
   GRANT SELECT ON ${SCHEMA}.managed_runtime_role_completions,${SCHEMA}.managed_runtime_role_completion_migrations TO ${r},${a};
   GRANT INSERT ON ${SCHEMA}.managed_runtime_role_completions TO ${a};`);
  return {state:"frozen-admission-unchanged" as const,migrationSha256:RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256};
 }).catch(()=>fail("unavailable"));
}
