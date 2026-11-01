//! Separate ownership architecture. No expiry or heartbeat permits regrant.
import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA as S,RESTRICTED_ROLE_CHECK} from "./pool.js";
import {MIGRATION_SHA256} from "./migration.js";
import {fail,uuid,type Outcome} from "./types.js";
export const providerOwnershipSql=`
ALTER TABLE ${S}.projects ADD COLUMN ownership_mode text NOT NULL DEFAULT 'clock-lease-v1' CHECK(ownership_mode IN('clock-lease-v1','provider-fenced-v1')),
 ADD COLUMN ownership_id uuid, ADD COLUMN provider_binding jsonb, ADD COLUMN provider_heartbeat_serial bigint NOT NULL DEFAULT 0 CHECK(provider_heartbeat_serial>=0);
DO $body$ DECLARE c record; n integer:=0; BEGIN
 FOR c IN SELECT conname FROM pg_constraint WHERE conrelid='${S}.projects'::regclass AND contype='c' AND pg_get_constraintdef(oid) LIKE '%owner_engine_instance%lease_id%lease_until%' LOOP
  EXECUTE format('ALTER TABLE ${S}.projects DROP CONSTRAINT %I',c.conname);n:=n+1;
 END LOOP;IF n<>1 THEN RAISE EXCEPTION 'original ownership shape differs';END IF;
END $body$;
ALTER TABLE ${S}.projects ADD CONSTRAINT ownership_shape_v2 CHECK(
 (ownership_mode='clock-lease-v1' AND ownership_id IS NULL AND provider_binding IS NULL AND provider_heartbeat_serial=0 AND
  ((owner_engine_instance IS NULL AND lease_id IS NULL AND lease_until IS NULL) OR(owner_engine_instance IS NOT NULL AND lease_id IS NOT NULL AND lease_until IS NOT NULL))) OR
 (ownership_mode='provider-fenced-v1' AND lease_id IS NULL AND lease_until IS NULL AND renew_serial=0 AND provider_binding IS NOT NULL AND jsonb_typeof(provider_binding)='object' AND pg_column_size(provider_binding)<=8192 AND
  ((owner_epoch=1 AND owner_engine_instance IS NULL AND ownership_id IS NULL AND provider_heartbeat_serial=0) OR(owner_epoch=2 AND owner_engine_instance IS NOT NULL AND ownership_id IS NOT NULL))));
CREATE FUNCTION ${S}.guard_provider_ownership() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog AS $body$ BEGIN
 IF NEW.ownership_mode IS DISTINCT FROM OLD.ownership_mode OR NEW.provider_binding IS DISTINCT FROM OLD.provider_binding THEN RAISE EXCEPTION 'ownership mode/binding immutable';END IF;
 IF OLD.ownership_mode='provider-fenced-v1' AND ROW(NEW.owner_epoch,NEW.owner_engine_instance,NEW.ownership_id) IS DISTINCT FROM ROW(OLD.owner_epoch,OLD.owner_engine_instance,OLD.ownership_id) THEN
  IF NOT(OLD.owner_epoch=1 AND OLD.owner_engine_instance IS NULL AND OLD.ownership_id IS NULL AND NEW.owner_epoch=2 AND NEW.owner_engine_instance IS NOT NULL AND NEW.ownership_id IS NOT NULL
   AND NEW.owner_engine_instance::text IS NOT DISTINCT FROM (OLD.provider_binding->>'engineInstanceId')) THEN RAISE EXCEPTION 'provider replacement unsupported';END IF;
 END IF;
 RETURN NEW;
END $body$;
CREATE TRIGGER provider_ownership_immutable BEFORE UPDATE ON ${S}.projects FOR EACH ROW EXECUTE FUNCTION ${S}.guard_provider_ownership();
CREATE TABLE ${S}.provider_owner_heartbeats(authority_id uuid NOT NULL,account_id text NOT NULL,project_id uuid NOT NULL,request_id uuid NOT NULL,
 ownership_id uuid NOT NULL,owner_epoch bigint NOT NULL,heartbeat_serial bigint NOT NULL,request_canonical bytea NOT NULL,grant_json jsonb NOT NULL CHECK(pg_column_size(grant_json)<=8192),
 PRIMARY KEY(authority_id,account_id,project_id,request_id),FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${S}.projects);
CREATE TABLE ${S}.managed_provider_owner_heartbeats(authority_id uuid NOT NULL,account_id text NOT NULL,home_id text NOT NULL,project_id uuid NOT NULL,
 request_id uuid NOT NULL,request_nonce uuid NOT NULL,ownership_id uuid NOT NULL,owner_epoch bigint NOT NULL,request_sha256 text NOT NULL,request_json jsonb NOT NULL CHECK(pg_column_size(request_json)<=24576),
 state text NOT NULL CHECK(state IN('started','replied')),ack_json jsonb CHECK(pg_column_size(ack_json)<=24576),
 PRIMARY KEY(authority_id,account_id,home_id,request_id),UNIQUE(authority_id,account_id,request_nonce),FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${S}.projects);
CREATE UNIQUE INDEX one_pending_provider_heartbeat ON ${S}.managed_provider_owner_heartbeats(authority_id,account_id,project_id,ownership_id) WHERE state='started';
ALTER TABLE ${S}.provider_owner_heartbeats ENABLE ROW LEVEL SECURITY;ALTER TABLE ${S}.provider_owner_heartbeats FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${S}.provider_owner_heartbeats USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
ALTER TABLE ${S}.managed_provider_owner_heartbeats ENABLE ROW LEVEL SECURITY;ALTER TABLE ${S}.managed_provider_owner_heartbeats FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${S}.managed_provider_owner_heartbeats USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE TABLE ${S}.provider_ownership_migrations(version integer PRIMARY KEY,authority_id uuid NOT NULL REFERENCES ${S}.authorities,sha256 text NOT NULL);
`;
export const PROVIDER_OWNERSHIP_MIGRATION_SHA256=createHash("sha256").update(providerOwnershipSql).digest("hex");
/** Explicit administrator install only. No ready/clock policy or existing project mode is changed. */
export async function migrateProviderOwnership(pool:AuthorityPool,options:{authorityId:string;runtimeRoles:readonly string[]}):Promise<Outcome<{version:1;mode:"provider-fenced-v1"}>>{
 if(pool.role!=="migration"||options.runtimeRoles.length<1||options.runtimeRoles.length>8)fail("forbidden");uuid(options.authorityId);for(const role of options.runtimeRoles)if(!/^[a-z][a-z0-9_]{0,62}$/.test(role))fail("invalid_request");
 return pool.run("",async tx=>{await tx.query("SELECT pg_advisory_xact_lock(1824529,71)");const prior=await tx.query(`SELECT sha256 FROM ${S}.migration_versions WHERE version=1`);if(prior.rows[0]?.sha256!==MIGRATION_SHA256)fail("migration_conflict");
  const old=await tx.query("SELECT to_regclass($1)::text AS name",[S+".provider_ownership_migrations"]);if(!old.rows[0].name){await tx.query(providerOwnershipSql);await tx.query(`INSERT INTO ${S}.provider_ownership_migrations VALUES(1,$1,$2)`,[options.authorityId,PROVIDER_OWNERSHIP_MIGRATION_SHA256]);}
  const version=await tx.query(`SELECT authority_id,sha256 FROM ${S}.provider_ownership_migrations WHERE version=1`);if(version.rows[0]?.authority_id!==options.authorityId||version.rows[0].sha256!==PROVIDER_OWNERSHIP_MIGRATION_SHA256)fail("migration_conflict");
  await tx.query(`REVOKE ALL ON ${S}.provider_owner_heartbeats,${S}.managed_provider_owner_heartbeats,${S}.provider_ownership_migrations FROM PUBLIC;REVOKE ALL ON FUNCTION ${S}.guard_provider_ownership() FROM PUBLIC;`);
  for(const role of new Set(options.runtimeRoles)){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[S,role]);if(safe.rows.length!==1||safe.rows[0].unsafe)fail("role_rejected");const r='"'+role+'"';await tx.query(`GRANT SELECT ON ${S}.provider_owner_heartbeats,${S}.managed_provider_owner_heartbeats,${S}.provider_ownership_migrations TO ${r};GRANT INSERT ON ${S}.provider_owner_heartbeats,${S}.managed_provider_owner_heartbeats TO ${r};GRANT UPDATE(state,ack_json) ON ${S}.managed_provider_owner_heartbeats TO ${r};GRANT UPDATE(ownership_id,provider_heartbeat_serial) ON ${S}.projects TO ${r};`);}
  return {version:1,mode:"provider-fenced-v1"};});
}
