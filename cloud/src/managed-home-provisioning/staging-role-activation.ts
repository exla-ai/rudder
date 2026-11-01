import {inspect} from "node:util";
import {AuthorityPool,SCHEMA} from "../project-authority/pool.js";
import {budgets,validateConfig,type PostgresAuthorityConfig} from "../project-authority/config.js";
import {OriginalSdkRoleActivationPermit,assertOriginalSdkRoleActivationPermit} from "./compiler-qualification.js";
import {OriginalRuntimeRoleActivationPermit,assertOriginalRuntimeRoleActivationPermit} from "./runtime-qualification.js";
import {COMPILER_QUALIFICATION_MIGRATION_SHA256} from "./compiler-qualification-schema.js";
import {RUNTIME_QUALIFICATION_MIGRATION_SHA256} from "./runtime-qualification-schema.js";
import {STAGING_DATABASE_IDENTITY_SQL,stagingDatabaseIdentityData,type StagingRoleActivationFact} from "./staging-role-activation-data.js";
import {check,fail,canonical,hash,snapshot,validatePolicy,type ManagedHomePolicy} from "./types.js";

/** Restriction only: the actual original supervisor setup cutoff. This is NOT
 * an owner lease, Root enable clock or a way to renew any publication. */
export interface StagingSetupRestriction {remaining():number;}
type Permit=OriginalSdkRoleActivationPermit|OriginalRuntimeRoleActivationPermit;
type Attempt={permit:Permit;fact?:StagingRoleActivationFact;promise?:Promise<void>;known:boolean;unknown:boolean;};
/** Explicit trusted operator/admin enrollment. No HTTP/JSON constructor accepts
 * a role fact. The real publisher permit is consumed BEFORE all SQL effects;
 * every attempted/unknown promise and original publisher stays retained. */
export class StagingRoleActivationAdmin {
 readonly #pool:AuthorityPool;readonly #policy:ManagedHomePolicy;readonly #attempts:Attempt[]=[];
 constructor(private readonly admin:PostgresAuthorityConfig,ordinary:PostgresAuthorityConfig,policy:ManagedHomePolicy,private readonly setup:StagingSetupRestriction){
  validateConfig(admin);validateConfig(ordinary);this.#policy=validatePolicy(policy);
  const a=admin.connection,r=ordinary.connection;
  check(this.#policy.controlPlane?.profile==="qualification-v1"&&admin.authorityId===ordinary.authorityId&&a.user!==r.user&&a.host===r.host&&a.port===r.port&&a.database===r.database&&a.ca===r.ca,"unconfigured");
  this.#pool=new AuthorityPool({host:a.host,port:a.port,database:a.database,user:a.user,password:a.password,ssl:{rejectUnauthorized:true,servername:a.host,...(a.ca?{ca:a.ca}:{})},application_name:"rudder-original-staging-role-activation"},budgets(admin),"migration");
 }
 [inspect.custom](){return "StagingRoleActivationAdmin [original permits/admin/unknown effects retained]";}
 toJSON():never{fail("invalid_request");}
 async activateRetaining(permit:Permit):Promise<void>{
  this.setup.remaining();
  if(permit instanceof OriginalSdkRoleActivationPermit)assertOriginalSdkRoleActivationPermit(permit);else assertOriginalRuntimeRoleActivationPermit(permit);
  check(this.#attempts.length<2&&!this.#attempts.some(v=>v.permit===permit),"effect_unknown");
  const held:Attempt={permit,known:false,unknown:false};this.#attempts.push(held);
  // This claim does not inspect a SQL row to recreate an original COMMIT fact.
  const fact=permit.claimForAdminActivation();held.fact=fact;
  check(fact.authority_id===this.admin.authorityId&&fact.policy_sha256===hash(canonical(this.#policy)),"identity_changed");
  const role=fact.role;check(!this.#attempts.some(v=>v!==held&&v.fact?.role===role),"effect_unknown");
  const prefix=role==="sdk"?"managed_compiler":"managed_runtime",roles=role==="sdk"?"managed_compiler_sdk_roles":"managed_runtime_image_roles";
  const installHash=role==="sdk"?hash(canonical(this.#policy.compilerSdk)):hash(canonical(this.#policy));
  check(fact.installation_policy_sha256===installHash&&fact.completion_sha256===hash(canonical(fact.completion))&&canonical(fact.completion.prepared_role).equals(canonical(fact.prepared)),"identity_changed");
  const migration=role==="sdk"?COMPILER_QUALIFICATION_MIGRATION_SHA256:RUNTIME_QUALIFICATION_MIGRATION_SHA256;
  held.promise=(async()=>{
   const out=await this.#pool.run("",async tx=>{
    this.setup.remaining();await tx.query("SELECT pg_advisory_xact_lock(1824529,24)");this.setup.remaining();
    const identity=await tx.query(STAGING_DATABASE_IDENTITY_SQL);this.setup.remaining();
    check(identity.rows.length===1&&canonical(stagingDatabaseIdentityData(identity.rows[0])).equals(canonical(fact.database)),"identity_changed");
    const install=await tx.query(`SELECT state,policy_sha256,migration_sha256 FROM ${SCHEMA}.${prefix}_role_installation WHERE authority_id=$1 FOR UPDATE`,[fact.authority_id]);this.setup.remaining();
    check(install.rows.length===1&&install.rows[0].state==="frozen"&&install.rows[0].policy_sha256===installHash&&install.rows[0].migration_sha256===migration,"unconfigured");
    const p=fact.prepared;
    const rows=await tx.query(`SELECT r.*,c.completion_json,c.completion_sha256 FROM ${SCHEMA}.${roles} r JOIN ${SCHEMA}.${prefix}_role_completions c USING(authority_id,role_binding_sha256) WHERE r.authority_id=$1 AND r.role_binding_sha256=$2 FOR SHARE OF r,c`,[fact.authority_id,p.role_binding_sha256]);this.setup.remaining();
    check(rows.rows.length===1,"identity_changed");const row=rows.rows[0];
    for(const k of ["authority_id","role_binding_sha256","image","image_receipt_sha256","inventory_sha256","qualification_id","ready_request_sha256"])check(row[k]===p[k],"identity_changed");
    check(canonical(row.binding_json).equals(canonical(p.binding_json))&&row.completion_sha256===fact.completion_sha256&&canonical(row.completion_json).equals(canonical(fact.completion)),"identity_changed");
    if(role==="runtime"){
     // Exact runtime-role fact -> image tuple only. This neither publishes S3
     // objects nor pins/bootstrap/acquires a project or activates storage.
     const b=p.binding_json;check(b.role==="immutable-image-runtime-role-v1"&&b.image===this.#policy.image&&b.image_receipt_sha256===this.#policy.runtimeImageReceiptSha256&&b.manifest.digest===this.#policy.imagePreseed?.fullManifestSha256&&canonical(b.installer_input).equals(canonical(this.#policy.imagePreseed?.installerInput)),"identity_changed");
     await tx.query(`INSERT INTO ${SCHEMA}.managed_image_qualifications(authority_id,image,image_receipt_sha256,manifest_sha256,installer_input_json) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING`,[fact.authority_id,b.image,b.image_receipt_sha256,b.manifest.digest,JSON.stringify(b.installer_input)]);this.setup.remaining();
     const q=await tx.query(`SELECT image_receipt_sha256,manifest_sha256,installer_input_json FROM ${SCHEMA}.managed_image_qualifications WHERE authority_id=$1 AND image=$2 FOR SHARE`,[fact.authority_id,b.image]);this.setup.remaining();
     check(q.rows.length===1&&q.rows[0].image_receipt_sha256===b.image_receipt_sha256&&q.rows[0].manifest_sha256===b.manifest.digest&&canonical(q.rows[0].installer_input_json).equals(canonical(b.installer_input)),"conflict");
    }
    const changed=await tx.query(`UPDATE ${SCHEMA}.${prefix}_role_installation SET state='ready' WHERE authority_id=$1 AND state='frozen' AND policy_sha256=$2 RETURNING authority_id`,[fact.authority_id,installHash]);this.setup.remaining();check(changed.rowCount===1,"effect_unknown");
   });
   this.setup.remaining();check(out.status==="committed","effect_unknown");held.known=true;
  })();
  try{await held.promise;}catch{held.unknown=true;fail("effect_unknown");}
 }
 statusData(){return snapshot({scope:"original-staging-admin-transition-data-only",attempts:this.#attempts.map(a=>({role:a.fact?.role??null,known_commit:a.known,unknown:a.unknown,permit_retained:true})),source_authority:false,storage_ready_claim:false});}
}
