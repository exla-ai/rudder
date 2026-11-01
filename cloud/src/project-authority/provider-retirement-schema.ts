// Additive restriction-only retirement ledger. No expiry, vacancy, replacement
// epoch, physical-fence assertion or reconciliation ACK is installed here.
import {createHash} from "node:crypto";
import {AuthorityPool,SCHEMA as S,RESTRICTED_ROLE_CHECK} from "./pool.js";
import {PROVIDER_OWNERSHIP_MIGRATION_SHA256} from "./provider-ownership-schema.js";
import {fail,uuid,type Outcome} from "./types.js";
const admissionTables=["commands","command_effects","capabilities","capability_targets","runs","run_attempts","decisions","topics","verifications","verification_topics","integration_commits"];
export const providerRetirementSql=`
ALTER TABLE ${S}.projects ADD COLUMN provider_retirement_id uuid;
CREATE TABLE ${S}.provider_owner_retirements(
 authority_id uuid NOT NULL,account_id text NOT NULL,project_id uuid NOT NULL,retirement_id uuid NOT NULL,
 action text NOT NULL CHECK(action IN('release','replace')),state text NOT NULL DEFAULT 'fence_required' CHECK(state IN('fence_required','fencing')),
 owner_epoch bigint NOT NULL CHECK(owner_epoch>=2),ownership_id uuid NOT NULL,engine_instance_id uuid NOT NULL,machine_boot_id text NOT NULL,
 provider_binding jsonb NOT NULL CHECK(jsonb_typeof(provider_binding)='object' AND pg_column_size(provider_binding)<=8192),
 previous_project_mode text NOT NULL CHECK(previous_project_mode IN('ready','frozen')),
 inventory_json jsonb NOT NULL CHECK(pg_column_size(inventory_json)<=262144 AND jsonb_typeof(inventory_json)='object' AND COALESCE(inventory_json->>'complete'='false',false) AND COALESCE(inventory_json->>'requiresNativeJournalReconciliation'='true',false)),
 fence_attempt_id uuid,PRIMARY KEY(authority_id,account_id,project_id,retirement_id),UNIQUE(authority_id,account_id,project_id,ownership_id),
 FOREIGN KEY(authority_id,account_id,project_id) REFERENCES ${S}.projects,
 CHECK((state='fence_required')=(fence_attempt_id IS NULL)));
ALTER TABLE ${S}.projects ADD CONSTRAINT provider_retirement_original_fk FOREIGN KEY(authority_id,account_id,project_id,provider_retirement_id) REFERENCES ${S}.provider_owner_retirements(authority_id,account_id,project_id,retirement_id);
ALTER TABLE ${S}.provider_owner_retirements ENABLE ROW LEVEL SECURITY;ALTER TABLE ${S}.provider_owner_retirements FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant ON ${S}.provider_owner_retirements USING(account_id=current_setting('rudder.account_id',true)) WITH CHECK(account_id=current_setting('rudder.account_id',true));
CREATE FUNCTION ${S}.guard_provider_retirement_record() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog AS $body$
DECLARE p record;BEGIN
 IF TG_OP='UPDATE' THEN
  IF (to_jsonb(NEW)-'state'-'fence_attempt_id') IS DISTINCT FROM (to_jsonb(OLD)-'state'-'fence_attempt_id') OR NOT(OLD.state='fence_required' AND NEW.state='fencing' AND OLD.fence_attempt_id IS NULL AND NEW.fence_attempt_id IS NOT NULL) THEN RAISE EXCEPTION 'retirement immutable or already dispatched';END IF;
 ELSE
  SELECT * INTO p FROM ${S}.projects WHERE authority_id=NEW.authority_id AND account_id=NEW.account_id AND project_id=NEW.project_id FOR UPDATE;
  IF NOT FOUND OR p.ownership_mode<>'provider-fenced-v1' OR p.provider_retirement_id IS NOT NULL OR p.mode NOT IN('ready','frozen') OR NEW.state<>'fence_required' OR NEW.fence_attempt_id IS NOT NULL
   OR ROW(p.owner_epoch,p.ownership_id,p.owner_engine_instance,p.provider_binding,p.mode) IS DISTINCT FROM ROW(NEW.owner_epoch,NEW.ownership_id,NEW.engine_instance_id,NEW.provider_binding,NEW.previous_project_mode)
   OR NEW.machine_boot_id IS DISTINCT FROM (p.provider_binding->>'machineBootId') THEN RAISE EXCEPTION 'retirement original differs';END IF;
 END IF;RETURN NEW;END $body$;
CREATE TRIGGER provider_retirement_record BEFORE INSERT OR UPDATE ON ${S}.provider_owner_retirements FOR EACH ROW EXECUTE FUNCTION ${S}.guard_provider_retirement_record();
CREATE FUNCTION ${S}.guard_provider_retirement_project() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog AS $body$
DECLARE r record;BEGIN
 IF OLD.provider_retirement_id IS NOT NULL THEN
  IF NEW.provider_retirement_id IS DISTINCT FROM OLD.provider_retirement_id OR NEW.mode<>'frozen'
   OR ROW(NEW.owner_epoch,NEW.owner_engine_instance,NEW.ownership_id,NEW.provider_binding,NEW.ownership_mode,NEW.head_vcs_commit,NEW.head_source_root,NEW.head_environment_root,NEW.integration_version)
    IS DISTINCT FROM ROW(OLD.owner_epoch,OLD.owner_engine_instance,OLD.ownership_id,OLD.provider_binding,OLD.ownership_mode,OLD.head_vcs_commit,OLD.head_source_root,OLD.head_environment_root,OLD.integration_version) THEN RAISE EXCEPTION 'retirement keeps original owner admission closed';END IF;
 ELSIF NEW.provider_retirement_id IS NOT NULL THEN
  SELECT * INTO r FROM ${S}.provider_owner_retirements WHERE authority_id=OLD.authority_id AND account_id=OLD.account_id AND project_id=OLD.project_id AND retirement_id=NEW.provider_retirement_id FOR SHARE;
  IF NOT FOUND OR OLD.ownership_mode<>'provider-fenced-v1' OR NEW.mode<>'frozen' OR r.state<>'fence_required'
   OR ROW(NEW.owner_epoch,NEW.ownership_id,NEW.owner_engine_instance,NEW.provider_binding,NEW.ownership_mode,NEW.head_vcs_commit,NEW.head_source_root,NEW.head_environment_root,NEW.integration_version)
    IS DISTINCT FROM ROW(OLD.owner_epoch,OLD.ownership_id,OLD.owner_engine_instance,OLD.provider_binding,OLD.ownership_mode,OLD.head_vcs_commit,OLD.head_source_root,OLD.head_environment_root,OLD.integration_version)
   OR ROW(r.owner_epoch,r.ownership_id,r.engine_instance_id,r.provider_binding) IS DISTINCT FROM ROW(OLD.owner_epoch,OLD.ownership_id,OLD.owner_engine_instance,OLD.provider_binding) THEN RAISE EXCEPTION 'retirement closure differs';END IF;
 END IF;RETURN NEW;END $body$;
CREATE TRIGGER provider_retirement_project BEFORE UPDATE ON ${S}.projects FOR EACH ROW EXECUTE FUNCTION ${S}.guard_provider_retirement_project();
CREATE FUNCTION ${S}.guard_provider_retirement_admission() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog AS $body$
DECLARE p record;a uuid;tenant text;project uuid;BEGIN
 IF TG_OP='DELETE' THEN a:=OLD.authority_id;tenant:=OLD.account_id;project:=OLD.project_id;ELSE a:=NEW.authority_id;tenant:=NEW.account_id;project:=NEW.project_id;END IF;
 SELECT ownership_mode INTO p FROM ${S}.projects WHERE authority_id=a AND account_id=tenant AND project_id=project;
 -- Mode is creation-fixed. Clock-mode writes acquire no new project lock.
 IF FOUND AND p.ownership_mode='provider-fenced-v1' THEN
  SELECT provider_retirement_id INTO p FROM ${S}.projects WHERE authority_id=a AND account_id=tenant AND project_id=project FOR UPDATE;
  IF p.provider_retirement_id IS NOT NULL THEN RAISE EXCEPTION 'provider retirement blocks new admission/effect mutation';END IF;
 END IF;
 IF TG_OP='DELETE' THEN RETURN OLD;END IF;RETURN NEW;END $body$;
${admissionTables.map(t=>`CREATE TRIGGER provider_retirement_admission BEFORE INSERT OR UPDATE OR DELETE ON ${S}.${t} FOR EACH ROW EXECUTE FUNCTION ${S}.guard_provider_retirement_admission();`).join("\n")}
CREATE TABLE ${S}.provider_retirement_migrations(version integer PRIMARY KEY,authority_id uuid NOT NULL REFERENCES ${S}.authorities,sha256 text NOT NULL);
`;
export const PROVIDER_RETIREMENT_MIGRATION_SHA256=createHash("sha256").update(providerRetirementSql).digest("hex");
export async function migrateProviderRetirement(pool:AuthorityPool,o:{authorityId:string;runtimeRoles:readonly string[]}):Promise<Outcome<{version:1;scope:"admission-closure-only"}>>{
 if(pool.role!=="migration"||o.runtimeRoles.length<1||o.runtimeRoles.length>8)fail("forbidden");uuid(o.authorityId);for(const role of o.runtimeRoles)if(!/^[a-z][a-z0-9_]{0,62}$/.test(role))fail("invalid_request");
 return pool.run("",async tx=>{
  await tx.query("SELECT pg_advisory_xact_lock(1824529,72)");const p=await tx.query(`SELECT authority_id,sha256 FROM ${S}.provider_ownership_migrations WHERE version=1`);if(p.rows[0]?.authority_id!==o.authorityId||p.rows[0].sha256!==PROVIDER_OWNERSHIP_MIGRATION_SHA256)fail("migration_conflict");
  const old=await tx.query("SELECT to_regclass($1)::text AS name",[S+".provider_retirement_migrations"]);if(!old.rows[0]?.name){await tx.query(providerRetirementSql);await tx.query(`INSERT INTO ${S}.provider_retirement_migrations VALUES(1,$1,$2)`,[o.authorityId,PROVIDER_RETIREMENT_MIGRATION_SHA256]);}
  const v=await tx.query(`SELECT authority_id,sha256 FROM ${S}.provider_retirement_migrations WHERE version=1`);if(v.rows[0]?.authority_id!==o.authorityId||v.rows[0].sha256!==PROVIDER_RETIREMENT_MIGRATION_SHA256)fail("migration_conflict");
  await tx.query(`REVOKE ALL ON ${S}.provider_owner_retirements,${S}.provider_retirement_migrations FROM PUBLIC;REVOKE ALL ON FUNCTION ${S}.guard_provider_retirement_record(),${S}.guard_provider_retirement_project(),${S}.guard_provider_retirement_admission() FROM PUBLIC;`);
  for(const name of new Set(o.runtimeRoles)){const safe=await tx.query(RESTRICTED_ROLE_CHECK,[S,name]);if(safe.rows.length!==1||safe.rows[0].unsafe)fail("role_rejected");const role='"'+name+'"';await tx.query(`GRANT SELECT ON ${S}.provider_owner_retirements,${S}.provider_retirement_migrations TO ${role};GRANT INSERT(authority_id,account_id,project_id,retirement_id,action,owner_epoch,ownership_id,engine_instance_id,machine_boot_id,provider_binding,previous_project_mode,inventory_json) ON ${S}.provider_owner_retirements TO ${role};GRANT UPDATE(state,fence_attempt_id) ON ${S}.provider_owner_retirements TO ${role};GRANT UPDATE(provider_retirement_id) ON ${S}.projects TO ${role};`);}
  return {version:1,scope:"admission-closure-only"};
 });
}
