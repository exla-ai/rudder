import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA,RESTRICTED_ROLE_CHECK} from "../project-authority/pool.js";
import {STORAGE_MIGRATION_SHA256} from "./storage-schema.js";
import {check,fail,sha} from "./types.js";
export const imageBatchSql=`
-- These columns grant row-lock permission only, never authority mutation.
ALTER TABLE ${SCHEMA}.managed_home_registrations ADD COLUMN image_batch_lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.managed_home_installation ADD COLUMN image_batch_lock_version bigint NOT NULL DEFAULT 0;
ALTER TABLE ${SCHEMA}.managed_storage_installation ADD COLUMN image_batch_lock_version bigint NOT NULL DEFAULT 0;
CREATE TABLE ${SCHEMA}.managed_image_batch_installation(
 authority_id uuid PRIMARY KEY REFERENCES ${SCHEMA}.authorities,state text NOT NULL CHECK(state IN('frozen','ready','retired')),
 qualification_receipt_sha256 text NOT NULL,migration_sha256 text NOT NULL,image_batch_lock_version bigint NOT NULL DEFAULT 0);
CREATE TABLE ${SCHEMA}.managed_image_batch_transfers(
 authority_id uuid NOT NULL,account_id text NOT NULL,transfer_id uuid NOT NULL,request_nonce uuid NOT NULL,
 last_ordinal integer NOT NULL DEFAULT -1,last_key text COLLATE "C" NOT NULL DEFAULT '',active_batch uuid,
 PRIMARY KEY(authority_id,account_id,transfer_id),
 FOREIGN KEY(authority_id,account_id,transfer_id) REFERENCES ${SCHEMA}.managed_storage_transfers);
CREATE TABLE ${SCHEMA}.managed_image_batches(
 authority_id uuid NOT NULL,account_id text NOT NULL,transfer_id uuid NOT NULL,batch_id uuid NOT NULL,ordinal integer NOT NULL CHECK(ordinal>=0 AND ordinal<100000),
 header_sha256 text NOT NULL,header_json jsonb NOT NULL CHECK(pg_column_size(header_json)<=262144),
 state text NOT NULL CHECK(state IN('started','replied')),ack_json jsonb CHECK(pg_column_size(ack_json)<=4096),
 PRIMARY KEY(authority_id,account_id,transfer_id,batch_id),UNIQUE(authority_id,account_id,transfer_id,ordinal),
 FOREIGN KEY(authority_id,account_id,transfer_id) REFERENCES ${SCHEMA}.managed_image_batch_transfers);
CREATE TABLE ${SCHEMA}.managed_image_batch_migrations(version integer PRIMARY KEY,sha256 text NOT NULL);
${["managed_image_batch_transfers","managed_image_batches"].map(t=>`ALTER TABLE ${SCHEMA}.${t} ENABLE ROW LEVEL SECURITY;ALTER TABLE ${SCHEMA}.${t} FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${SCHEMA}.${t} USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));`).join("\n")}
`;
export const IMAGE_BATCH_MIGRATION_SHA256=createHash("sha256").update(imageBatchSql).digest("hex");
/** Explicit operator install only. No runtime DDL/ready/receipt replacement. */
export async function migrateManagedImageBatches(pool:AuthorityPool,authorityId:string,receipt:string,roles:readonly string[]){
 sha(receipt);check(pool.role==="migration"&&roles.length>0&&roles.length<=3);for(const role of roles)check(/^[a-z][a-z0-9_]{0,62}$/.test(role));
 return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,8)");const required=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_storage_transfers"]);check(required.rows[0].name,"unconfigured");const base=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_storage_migrations WHERE version=3`);check(base.rows[0]?.sha256===STORAGE_MIGRATION_SHA256,"unconfigured");
  const old=await tx.query("SELECT to_regclass($1)::text AS name",[SCHEMA+".managed_image_batch_migrations"]);if(!old.rows[0].name){await tx.query(imageBatchSql);await tx.query(`INSERT INTO ${SCHEMA}.managed_image_batch_migrations VALUES(1,$1)`,[IMAGE_BATCH_MIGRATION_SHA256]);await tx.query(`INSERT INTO ${SCHEMA}.managed_image_batch_installation(authority_id,state,qualification_receipt_sha256,migration_sha256) VALUES($1,'frozen',$2,$3)`,[authorityId,receipt,IMAGE_BATCH_MIGRATION_SHA256]);}
  const current=await tx.query(`SELECT m.sha256,i.qualification_receipt_sha256 FROM ${SCHEMA}.managed_image_batch_migrations m JOIN ${SCHEMA}.managed_image_batch_installation i ON i.authority_id=$1 WHERE m.version=1`,[authorityId]);check(current.rows.length===1&&current.rows[0].sha256===IMAGE_BATCH_MIGRATION_SHA256&&current.rows[0].qualification_receipt_sha256===receipt,"unconfigured");
  await tx.query(`REVOKE ALL ON ${SCHEMA}.managed_image_batch_installation,${SCHEMA}.managed_image_batch_transfers,${SCHEMA}.managed_image_batches,${SCHEMA}.managed_image_batch_migrations FROM PUBLIC`);
  for(const role of roles){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[SCHEMA,role]);check(safe.rows.length===1&&!safe.rows[0].unsafe,"unconfigured");const r='"'+role+'"';await tx.query(`GRANT SELECT ON ${SCHEMA}.managed_image_batch_installation,${SCHEMA}.managed_image_batch_migrations,${SCHEMA}.managed_image_batch_transfers,${SCHEMA}.managed_image_batches TO ${r};
   GRANT INSERT ON ${SCHEMA}.managed_image_batch_transfers,${SCHEMA}.managed_image_batches TO ${r};
   GRANT SELECT ON ${SCHEMA}.managed_home_registrations TO ${r};
   GRANT UPDATE(image_batch_lock_version) ON ${SCHEMA}.managed_home_registrations,${SCHEMA}.managed_home_installation,${SCHEMA}.managed_storage_installation,${SCHEMA}.managed_image_batch_installation TO ${r};
   GRANT UPDATE(last_ordinal,last_key,active_batch) ON ${SCHEMA}.managed_image_batch_transfers TO ${r};GRANT UPDATE(state,ack_json) ON ${SCHEMA}.managed_image_batches TO ${r};`);}
  return {version:1,state:"frozen-or-existing" as const};
 }).catch(()=>fail("unavailable"));
}
