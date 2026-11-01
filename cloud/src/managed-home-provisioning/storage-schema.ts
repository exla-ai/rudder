import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {check,fail,sha} from "./types.js";
export const storageSql=`
CREATE TABLE ${SCHEMA}.managed_storage_installation(
 authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,state text NOT NULL CHECK(state IN('frozen','ready','retired')),
 storage_receipt_sha256 text NOT NULL, migration_sha256 text NOT NULL);
-- A separately qualified complete immutable image capture, never supplied by HTTP.
CREATE TABLE ${SCHEMA}.managed_image_qualifications(
 authority_id uuid NOT NULL REFERENCES ${SCHEMA}.authorities,image text NOT NULL,image_receipt_sha256 text NOT NULL,
 manifest_sha256 text NOT NULL,installer_input_json jsonb NOT NULL,
 PRIMARY KEY(authority_id,image));
CREATE TABLE ${SCHEMA}.managed_storage_transfers(
 authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,transfer_id uuid NOT NULL,
 project_id uuid NOT NULL,workspace_id text NOT NULL,prepare_id uuid NOT NULL,engine_instance_id uuid NOT NULL,boot_id text NOT NULL,
 scope text NOT NULL CHECK(scope IN('image','fresh')),command_key text NOT NULL,request_sha256 text NOT NULL,
 request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=24576),phase text NOT NULL CHECK(phase IN('reserved','uploading','published','bootstrapped','acquired')),
 original_end timestamptz NOT NULL,process_owner uuid NOT NULL,objects_reserved integer NOT NULL DEFAULT 0,bytes_reserved bigint NOT NULL DEFAULT 0,
 stage_json jsonb,publication_json jsonb,bootstrap_json jsonb,acquire_json jsonb,
 PRIMARY KEY(authority_id,account_id,transfer_id),UNIQUE(authority_id,account_id,home_id,command_key),
 FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${SCHEMA}.managed_home_fresh_projects);
CREATE UNIQUE INDEX one_fresh_transfer ON ${SCHEMA}.managed_storage_transfers(authority_id,account_id,project_id) WHERE scope='fresh';
CREATE TABLE ${SCHEMA}.managed_storage_effects(
 authority_id uuid NOT NULL,account_id text NOT NULL,transfer_id uuid NOT NULL,effect_key text NOT NULL,
 request_sha256 text NOT NULL,state text NOT NULL CHECK(state IN('started','replied')),ack_json jsonb CHECK(pg_column_size(ack_json)<=1048576),
 PRIMARY KEY(authority_id,account_id,transfer_id,effect_key),
 FOREIGN KEY(authority_id,account_id,transfer_id) REFERENCES ${SCHEMA}.managed_storage_transfers);
CREATE TABLE ${SCHEMA}.managed_image_publications(
 authority_id uuid NOT NULL,account_id text NOT NULL,image text NOT NULL,manifest_sha256 text NOT NULL,transfer_id uuid NOT NULL,
 installer_input_json jsonb NOT NULL,publication_json jsonb NOT NULL,objects_json jsonb NOT NULL CHECK(pg_column_size(objects_json)<=33554432),
 PRIMARY KEY(authority_id,account_id,image),FOREIGN KEY(authority_id,account_id,transfer_id) REFERENCES ${SCHEMA}.managed_storage_transfers);
CREATE TABLE ${SCHEMA}.managed_storage_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
${["managed_storage_transfers","managed_storage_effects","managed_image_publications"].map(t=>`ALTER TABLE ${SCHEMA}.${t} ENABLE ROW LEVEL SECURITY;ALTER TABLE ${SCHEMA}.${t} FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.${t} USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));`).join("\n")}
`;
export const STORAGE_MIGRATION_SHA256=createHash("sha256").update(storageSql).digest("hex");
/** Explicit install creates FROZEN admission. Runtime roles cannot qualify an
 * image, replace storage evidence, enroll credentials, or activate any row. */
export async function migrateManagedStorage(pool:AuthorityPool,authorityId:string,storageReceiptSha256:string,roles:readonly string[]){
 sha(storageReceiptSha256);check(pool.role==="migration"&&roles.length>=1&&roles.length<=3);for(const r of roles)check(/^[a-z][a-z0-9_]{0,62}$/.test(r));
 return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,4)");const old=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_storage_migrations"]);
  if(!old.rows[0].name){await tx.query(storageSql);await tx.query(`INSERT INTO ${SCHEMA}.managed_storage_migrations VALUES(3,$1)`,[STORAGE_MIGRATION_SHA256]);await tx.query(`INSERT INTO ${SCHEMA}.managed_storage_installation VALUES($1,'frozen',$2,$3)`,[authorityId,storageReceiptSha256,STORAGE_MIGRATION_SHA256]);}
  const row=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_storage_migrations WHERE version=3`);check(row.rows[0]?.sha256===STORAGE_MIGRATION_SHA256,"unconfigured");
  await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_storage_installation,${SCHEMA}.managed_image_qualifications,${SCHEMA}.managed_storage_transfers,${SCHEMA}.managed_storage_effects,${SCHEMA}.managed_image_publications,${SCHEMA}.managed_storage_migrations FROM PUBLIC`);
  for(const role of roles){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");const r='"'+role+'"';await tx.query(`GRANT SELECT ON ${SCHEMA}.managed_storage_installation,${SCHEMA}.managed_image_qualifications,${SCHEMA}.managed_storage_transfers,${SCHEMA}.managed_storage_effects,${SCHEMA}.managed_image_publications,${SCHEMA}.managed_storage_migrations TO ${r};
   GRANT INSERT ON ${SCHEMA}.managed_storage_transfers,${SCHEMA}.managed_storage_effects,${SCHEMA}.managed_image_publications TO ${r};
   GRANT UPDATE(phase,objects_reserved,bytes_reserved,stage_json,publication_json,bootstrap_json,acquire_json) ON ${SCHEMA}.managed_storage_transfers TO ${r};
   GRANT UPDATE(state,ack_json) ON ${SCHEMA}.managed_storage_effects TO ${r};`);}
  return {version:3,state:"frozen-or-existing" as const};
 }).catch(()=>fail("unavailable"));
}
