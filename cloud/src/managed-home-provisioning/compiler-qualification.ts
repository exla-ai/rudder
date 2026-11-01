import {STAGING_DATABASE_IDENTITY_SQL,stagingDatabaseIdentityData,type StagingDatabaseIdentity,type StagingRoleActivationFact} from "./staging-role-activation-data.js";
import {validateCompilerSdkCatalog} from "./compiler-sdk-catalog.js";
import {COMPILER_ROLE_COMPLETION_MIGRATION_SHA256} from "./compiler-qualification-completion-schema.js";
import {assertOriginalRuntimeQualificationPurpose,type RuntimeQualificationPurpose} from "./runtime-qualification.js";
import type {CommittedHostTransport} from "./transport.js";
import {inspect} from "node:util";
import {AuthorityPool,SCHEMA,type Transaction} from "../project-authority/pool.js";
import {originalTime,assertOriginalDisposableMachineCustody,type OriginalTime,type OriginalDisposableMachineCustody} from "./fly.js";
import type {CommittedRegistration,Envelope,RegistrationBinding} from "./registration.js";
import {ManagedHomeError,check,fail,exact,canonical,hash,snapshot,runtimeMountConfigData,type Intent,type BoundHome,type ManagedHomePolicy} from "./types.js";
import {COMPILER_QUALIFICATION_MIGRATION_SHA256} from "./compiler-qualification-schema.js";
import {authenticateCompilerQualificationReady,validateCompilerQualificationCandidate,validateCompilerQualificationScope,compilerRoleBindingData,
 publishedCompilerQualificationAck,type CompilerQualificationScope,type CompilerQualificationReady,type CompilerQualificationAck} from "./compiler-qualification-protocol.js";

// The actual registry retains the existing initial-registration cutoff;
// SDK publication receives that SAME conservative OriginalTime window.
const qualificationServices=new WeakSet<RootCompilerSdkQualifications>();
const PURPOSE_ISSUER=Symbol("actual committed disposable qualification reservation");
class CompilerQualificationPurpose {
 #committed=false;
 private constructor(issuer:symbol,readonly intent:Intent,readonly scope:CompilerQualificationScope,readonly original:OriginalTime,readonly runtimePurpose:RuntimeQualificationPurpose){check(issuer===PURPOSE_ISSUER);}
 static stage(issuer:symbol,i:Intent,s:CompilerQualificationScope,t:OriginalTime,r:RuntimeQualificationPurpose){assertOriginalRuntimeQualificationPurpose(r);r.assertOriginal(i);return new CompilerQualificationPurpose(issuer,i,s,t,r);}
 commitOriginalReceipt(issuer:symbol){check(issuer===PURPOSE_ISSUER&&!this.#committed,"effect_unknown");this.#committed=true;}
 assertOriginal(i:Intent){this.runtimePurpose.assertOriginal(i);check(this.#committed&&canonical(this.intent).equals(canonical(i)),"identity_changed");this.original.remaining();}
 [inspect.custom](){return "CompilerQualificationPurpose [committed disposable-only reservation]";}
 toJSON():never{fail("invalid_request");}
}
export type {CompilerQualificationPurpose};
interface RetainedReservation {purpose:CompilerQualificationPurpose;attempted:boolean;promise?:Promise<void>;unknown:boolean;}
interface RetainedPublication {activationDatabase?:StagingDatabaseIdentity;activationPermit?:OriginalSdkRoleActivationPermit;activationTaken?:boolean;home:BoundHome;leaseObserved:boolean;firstCommitKnown:boolean;completion?:CompletedRoleCommit;completionRecorded:boolean;prepared?:PreparedRoleData;purpose:CompilerQualificationPurpose;custody:OriginalDisposableMachineCustody;
 registration:CommittedRegistration;transport:CommittedHostTransport;request:CompilerQualificationReady;time:OriginalTime;started:boolean;
 ack?:Envelope<CompilerQualificationAck>;promise?:Promise<Envelope<CompilerQualificationAck>>;unknown:boolean;}
interface PreparedRoleData {authority_id:string;role_binding_sha256:string;image:string;image_receipt_sha256:string;inventory_sha256:string;binding_json:unknown;qualification_id:string;ready_request_sha256:string;}
interface RoleCompletionData {format_version:1;kind:"original-compiler-sdk-known-commit-v1";prepared_role:PreparedRoleData;
 qualification_scope:CompilerQualificationScope;request_sha256:string;original_ack_sha256:string;
 registration_binding_sha256:string;registration_proof_sha256:string;original_enable_constraint:unknown;selected_executables:unknown;}
const COMPLETION_ISSUER=Symbol("known original role COMMIT observed with actual current owners");
function preparedRoleData(row:PreparedRoleData):PreparedRoleData {return snapshot({authority_id:row.authority_id,role_binding_sha256:row.role_binding_sha256,image:row.image,image_receipt_sha256:row.image_receipt_sha256,inventory_sha256:row.inventory_sha256,binding_json:row.binding_json,qualification_id:row.qualification_id,ready_request_sha256:row.ready_request_sha256});}
function checkPublicationOriginals(held:RetainedPublication):void {
 held.transport.assertLocalCurrent();held.purpose.assertOriginal(held.home.intent);held.time.remaining();
 if(held.leaseObserved)held.custody.assertPublicationClockCurrent(held.time);
 held.transport.assertLocalCurrent();
}
/** No JSON/row/readback constructor. Issued synchronously ONLY after the first
 * actual COMMIT reply is known and every original owner/cutoff still passes.
 * It retains the original publication; its later recording is bookkeeping of
 * this already observed fact, never another live Root/lease/ACK authority. */
class CompletedRoleCommit {
 private constructor(issuer:symbol,private readonly original:RetainedPublication,private readonly data:RoleCompletionData){check(issuer===COMPLETION_ISSUER);}
 static observe(issuer:symbol,held:RetainedPublication):CompletedRoleCommit {
  check(issuer===COMPLETION_ISSUER&&held.firstCommitKnown&&held.leaseObserved&&held.prepared&&held.ack,"effect_unknown");checkPublicationOriginals(held);
  return new CompletedRoleCommit(issuer,held,snapshot({format_version:1,kind:"original-compiler-sdk-known-commit-v1",prepared_role:held.prepared,
   qualification_scope:held.purpose.scope,request_sha256:held.ack.body.request_sha256,original_ack_sha256:hash(canonical(held.ack)),
   registration_binding_sha256:hash(canonical(held.request.registration_binding)),registration_proof_sha256:held.request.registration_proof_sha256,
   original_enable_constraint:held.request.observation.original_enable_constraint,selected_executables:held.request.selected_executables}));
 }
 originalData(issuer:symbol,held:RetainedPublication):RoleCompletionData {check(issuer===COMPLETION_ISSUER&&this.original===held&&held.completion===this,"identity_changed");return snapshot(this.data);}
 toJSON():never{fail("invalid_request");}
}
function validateCompletionData(value:RoleCompletionData,prepared:PreparedRoleData,policy:ManagedHomePolicy):void {
 exact(value,["format_version","kind","prepared_role","qualification_scope","request_sha256","original_ack_sha256","registration_binding_sha256","registration_proof_sha256","original_enable_constraint","selected_executables"]);
 check(Object.keys(value).length===10&&value.format_version===1&&value.kind==="original-compiler-sdk-known-commit-v1"&&canonical(value.prepared_role).equals(canonical(prepared)),"identity_changed");
 validateCompilerQualificationScope(value.qualification_scope,policy,true);
 const {format_version,scope,qualification_id,qualification_nonce,...candidate}=value.qualification_scope;
 check(canonical(compilerRoleBindingData(candidate)).equals(canonical(prepared.binding_json))&&hash(canonical(prepared.binding_json))===prepared.role_binding_sha256,"identity_changed");
}

/** Fixed source-only role publisher. It cannot create Root observations or
 * project/lease/source authority. All owners and unknown commits stay retained;
 * exact historical readback never restores a reservation or delivery cutoff. */
const ROLE_ACTIVATION_ISSUER=Symbol("original known staging sdk publication activation");
const originalActivationPermits=new WeakSet<OriginalSdkRoleActivationPermit>();
/** An already completed original fact, NOT live Root/lease/ACK authority.
 * No JSON/row/restart constructor. Claiming it never renews an old clock. */
export class OriginalSdkRoleActivationPermit {
 #claimed=false;#held:RetainedPublication;#fact:StagingRoleActivationFact;
 private constructor(issuer:symbol,held:RetainedPublication,fact:StagingRoleActivationFact){
  check(issuer===ROLE_ACTIVATION_ISSUER,"identity_changed");this.#held=held;this.#fact=fact;originalActivationPermits.add(this);
 }
 static observe(issuer:symbol,held:RetainedPublication,authorityId:string,policy:ManagedHomePolicy):OriginalSdkRoleActivationPermit{
  check(issuer===ROLE_ACTIVATION_ISSUER&&policy.controlPlane?.profile==="qualification-v1"&&policy.ownerMode==="provider-fenced-v1"&&policy.sdkInitialSource==="sdk-fixed-suite-v1"
   &&held.firstCommitKnown&&held.completionRecorded&&held.completion&&held.prepared&&held.activationDatabase&&!held.unknown,"effect_unknown");
  checkPublicationOriginals(held);
  const completion=held.completion.originalData(COMPLETION_ISSUER,held);
  return new OriginalSdkRoleActivationPermit(issuer,held,snapshot({format_version:1,role:"sdk",authority_id:authorityId,
   policy_sha256:hash(canonical(policy)),installation_policy_sha256:hash(canonical(policy.compilerSdk)),database:held.activationDatabase,
   prepared:preparedRoleData(held.prepared),completion,completion_sha256:hash(canonical(completion))}));
 }
 claimForAdminActivation():StagingRoleActivationFact{
  check(originalActivationPermits.has(this)&&!this.#claimed&&this.#held.activationPermit===this
   &&this.#held.firstCommitKnown&&this.#held.completionRecorded&&!this.#held.unknown,"effect_unknown");
  this.#claimed=true;return snapshot(this.#fact);
 }
 [inspect.custom](){return "OriginalSdkRoleActivationPermit [original historical fact only]";}
 toJSON():never{fail("invalid_request");}
}
export function assertOriginalSdkRoleActivationPermit(value:unknown):asserts value is OriginalSdkRoleActivationPermit{
 check(value!==null&&typeof value==="object"&&originalActivationPermits.has(value as OriginalSdkRoleActivationPermit),"identity_changed");
}

export class RootCompilerSdkQualifications {
 #reservations=new Map<string,RetainedReservation>();#publications=new Map<string,RetainedPublication>();
 constructor(private readonly runtime:AuthorityPool,private readonly registration:AuthorityPool,private readonly authorityId:string,private readonly policy:ManagedHomePolicy){qualificationServices.add(this);}
 [inspect.custom](){return "RootCompilerSdkQualifications [retained original disposable publishers]";}
 toJSON():never{fail("invalid_request");}
 async #run<T>(pool:AuthorityPool,account:string,work:(tx:Transaction)=>Promise<T>):Promise<T>{let known:ManagedHomeError|undefined;
  try{const out=await pool.run(account,async tx=>{try{return await work(tx);}catch(e){if(e instanceof ManagedHomeError)known=e;throw e;}});check(out.status==="committed","effect_unknown");return out.value;}catch{if(known)throw known;fail("effect_unknown");}}
 async #installation(tx:Transaction,production:boolean){const r=await tx.query(`SELECT i.state,i.policy_sha256,i.migration_sha256,a.state AS authority_state,h.state AS home_state,h.policy_sha256 AS home_policy,v.sha256 AS version_sha256
   FROM ${SCHEMA}.managed_compiler_role_installation i JOIN ${SCHEMA}.authorities a USING(authority_id)
   JOIN ${SCHEMA}.managed_home_installation h USING(authority_id) CROSS JOIN ${SCHEMA}.managed_compiler_role_migrations v
   WHERE i.authority_id=$1 AND v.version=1`,[this.authorityId]);const row=r.rows[0];
  check(r.rows.length===1&&row.authority_state==="ready"&&row.home_state==="ready"&&row.home_policy===hash(canonical(this.policy))&&row.policy_sha256===hash(canonical(this.policy.compilerSdk))&&row.migration_sha256===COMPILER_QUALIFICATION_MIGRATION_SHA256&&row.version_sha256===COMPILER_QUALIFICATION_MIGRATION_SHA256&&(production?row.state==="ready":row.state==="frozen"||row.state==="ready"),"unconfigured");
  const completion=await tx.query(`SELECT sha256 FROM ${SCHEMA}.managed_compiler_role_completion_migrations WHERE version=1`);check(completion.rows[0]?.sha256===COMPILER_ROLE_COMPLETION_MIGRATION_SHA256,"unconfigured");}
 /** Selected production profile must find this NEW role row. Old image rows,
  * operator receipt hashes and a ready installation alone are insufficient.
  * Absent profile returns without a query, preserving legacy behavior. */
 async assertPublishedRole(policy:ManagedHomePolicy,time?:OriginalTime):Promise<void>{const m=policy.compilerSdk;if(!m)return;check(canonical(policy).equals(canonical(this.policy)),"identity_changed");time?.remaining();
  await this.#run(this.runtime,"",async tx=>{await this.#installation(tx,true);const r=await tx.query(`SELECT r.*,c.completion_sha256,c.completion_json FROM ${SCHEMA}.managed_compiler_sdk_roles r
   JOIN ${SCHEMA}.managed_compiler_role_completions c USING(authority_id,role_binding_sha256)
   WHERE r.authority_id=$1 AND r.role_binding_sha256=$2`,[this.authorityId,hash(canonical(compilerRoleBindingData({sdk:m})))]);check(r.rows.length===1,"unconfigured");
   const row=r.rows[0],b=row.binding_json;check(b.format_version===1&&b.role==="immutable-compiler-sdk-role-v1","identity_changed");const candidate={sdk:policy.compilerSdk!};
   const c={sdk:validateCompilerSdkCatalog(candidate.sdk)};check(canonical(b).equals(canonical(compilerRoleBindingData(c)))&&row.role_binding_sha256===hash(canonical(compilerRoleBindingData(c)))&&row.completion_sha256===hash(canonical(row.completion_json)),"identity_changed");
   validateCompletionData(row.completion_json,preparedRoleData(row),policy);time?.remaining();});time?.remaining();}
 /** Stage the generated intent BEFORE DB effects. No existing row or replayed
  * JSON can issue a purpose; only this original successful INSERT/COMMIT can. */
 async reserveRetaining(i:Intent,time:OriginalTime,runtimePurpose:RuntimeQualificationPurpose):Promise<CompilerQualificationPurpose>{check(i.authorityId===this.authorityId&&i.compilerQualification,"identity_changed");const scope=validateCompilerQualificationScope(i.compilerQualification,this.policy);time.remaining();
  check(!this.#reservations.has(scope.qualification_id)&&this.#reservations.size<8,"effect_unknown");
  const purpose=CompilerQualificationPurpose.stage(PURPOSE_ISSUER,i,scope,time,runtimePurpose),held:RetainedReservation={purpose,attempted:true,unknown:false};this.#reservations.set(scope.qualification_id,held);
  held.promise=this.#run(this.runtime,i.accountId,async tx=>{await this.#installation(tx,false);await tx.query("SELECT pg_advisory_xact_lock(hashtextextended($1,49))",[this.authorityId+":"+i.accountId]);const n=await tx.query(`SELECT count(*)::text AS n FROM ${SCHEMA}.managed_compiler_qualifications WHERE authority_id=$1 AND account_id=$2`,[this.authorityId,i.accountId]);check(Number(n.rows[0].n)<8,"busy");time.remaining();
   await tx.query(`INSERT INTO ${SCHEMA}.managed_compiler_qualifications(authority_id,account_id,qualification_id,home_id,process_owner,provision_nonce,request_sha256,scope_json,original_end) VALUES($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp()+($9::integer*interval '1 millisecond'))`,[this.authorityId,i.accountId,scope.qualification_id,i.homeId,i.processOwner,i.nonce,i.requestHash,JSON.stringify(scope),time.remaining()]);time.remaining();});
  try{await held.promise;purpose.commitOriginalReceipt(PURPOSE_ISSUER);time.remaining();return purpose;}catch{held.unknown=true;throw new ManagedHomeError("effect_unknown");}
 }
 async assertPurposeCurrent(purpose:CompilerQualificationPurpose,i:Intent,time:OriginalTime):Promise<void>{check(this.#reservations.get(purpose.scope.qualification_id)?.purpose===purpose,"identity_changed");purpose.assertOriginal(i);time.remaining();
  await this.#run(this.runtime,i.accountId,async tx=>{await this.#installation(tx,false);const r=await tx.query(`SELECT *,clock_timestamp()<original_end AS live FROM ${SCHEMA}.managed_compiler_qualifications WHERE authority_id=$1 AND account_id=$2 AND qualification_id=$3`,[this.authorityId,i.accountId,purpose.scope.qualification_id]);const q=r.rows[0];check(r.rows.length===1&&q.live&&q.home_id===i.homeId&&q.process_owner===i.processOwner&&q.provision_nonce===i.nonce&&q.request_sha256===i.requestHash&&canonical(q.scope_json).equals(canonical(purpose.scope)),"identity_changed");time.remaining();purpose.assertOriginal(i);});}
 async #currentRegistration(tx:Transaction,held:RetainedPublication){
  const {home,registration:actual,request,transport}=held;checkPublicationOriginals(held);
  await this.#installation(tx,false);checkPublicationOriginals(held);const i=home.intent;
  check(transport.homeId===i.homeId,"identity_changed");
  const rows=await tx.query(`SELECT r.ack_json,r.proof_json,h.intent_json,h.nonce,h.revoked_at,e.revoked_at AS engine_revoked,c.revoked_at AS credential_revoked,
   clock_timestamp()<c.expires_at AS credential_live,a.state AS authority_state,g.state AS home_state,
   e.engine_instance_id::text,e.machine_id,e.machine_installation_id::text,e.machine_boot_id,e.authenticated_key_id::text,c.token_sha256,c.principal_id
   FROM ${SCHEMA}.managed_home_registrations r JOIN ${SCHEMA}.managed_home_intents h USING(authority_id,account_id,home_id)
   JOIN ${SCHEMA}.engine_instances e ON e.account_id=r.account_id AND e.engine_instance_id=(r.challenge_json->'body'->>'engine_instance_id')::uuid
   JOIN ${SCHEMA}.authority_credentials c ON c.account_id=e.account_id AND c.credential_id=e.authenticated_key_id
   JOIN ${SCHEMA}.authorities a ON a.authority_id=r.authority_id JOIN ${SCHEMA}.managed_home_installation g ON g.authority_id=r.authority_id
   WHERE r.authority_id=$1 AND r.account_id=$2 AND r.home_id=$3 AND r.phase='enrolled' AND r.proof_sha256=$4 AND h.phase='routable' AND c.kind='engine'
   FOR SHARE OF r,h,e,c,a,g`,[this.authorityId,i.accountId,i.homeId,actual.proofSha256]);checkPublicationOriginals(held);const row=rows.rows[0];
  check(rows.rows.length===1&&row.nonce===i.nonce&&row.revoked_at===null&&row.engine_revoked===null&&row.credential_revoked===null&&row.credential_live&&row.authority_state==="ready"&&row.home_state==="ready"&&canonical(row.intent_json).equals(canonical(i))&&canonical(row.ack_json).equals(canonical(actual.ack)),"identity_changed");
  const a=actual.ack.body,o=request.observation;check(row.engine_instance_id===a.engine_instance_id&&row.machine_id===a.binding.machine_id&&row.machine_installation_id===a.binding.incarnation&&row.machine_boot_id===a.boot_id&&row.authenticated_key_id===a.credential_id&&row.token_sha256===actual.context.credentialSha256&&row.principal_id===actual.context.principalId,"identity_changed");check(request.root_nonce===a.root_nonce&&request.registration_proof_sha256===a.proof_sha256&&canonical(request.registration_binding).equals(canonical(a.binding))&&o.engine_instance_id===a.engine_instance_id&&o.boot_id===a.boot_id&&o.bootstrap_nonce===a.root_nonce&&o.service_pid_data===row.proof_json.evidence.service.pid,"identity_changed");
  const generation=await tx.query(`SELECT 1 FROM ${SCHEMA}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND home_id=$3 AND serial=$4 AND generation=$5 AND state='committed' FOR SHARE`,[this.authorityId,i.accountId,i.homeId,transport.serial,transport.generation]);checkPublicationOriginals(held);check(generation.rows.length===1,"identity_changed");
 }
 /** Called only by registry after actual Root HMAC + original socket checks.
  * One durable Started precedes the exact-original publication lease. Any
  * failure retains the request, creation witness, registration, time and ACK. */
 async publishRetaining(purpose:CompilerQualificationPurpose,custody:OriginalDisposableMachineCustody,home:BoundHome,actual:CommittedRegistration,transport:CommittedHostTransport,key:Buffer,input:unknown,time:OriginalTime):Promise<Envelope<CompilerQualificationAck>>{
  check(purpose instanceof CompilerQualificationPurpose&&this.#reservations.get(purpose.scope.qualification_id)?.purpose===purpose,"identity_changed");assertOriginalDisposableMachineCustody(custody,home);
  const request=authenticateCompilerQualificationReady(input,key,purpose.scope,actual.ack.body.binding,actual.proofSha256).body;
  check(request.root_nonce===actual.ack.body.root_nonce,"identity_changed");const id=purpose.scope.qualification_id;
  check(!this.#publications.has(id)&&this.#publications.size<8&&custody.matchesOriginal(home),"effect_unknown");purpose.assertOriginal(home.intent);time.remaining();transport.assertLocalCurrent();
  const held:RetainedPublication={home,purpose,custody,registration:actual,transport,request:snapshot(request,24*1024),time,started:false,leaseObserved:false,firstCommitKnown:false,completionRecorded:false,unknown:false};this.#publications.set(id,held);
  held.ack=publishedCompilerQualificationAck(key,request);
  held.promise=(async()=>{
   await transport.assertCurrent();checkPublicationOriginals(held);
   await this.#run(this.registration,home.intent.accountId,async tx=>{
    await this.#currentRegistration(tx,held);checkPublicationOriginals(held);
    const q=await tx.query(`UPDATE ${SCHEMA}.managed_compiler_qualifications SET state='started',ready_request_sha256=$4,ready_request_json=$5,registration_proof_sha256=$6 WHERE authority_id=$1 AND account_id=$2 AND qualification_id=$3 AND state='reserved' AND process_owner=$7 AND provision_nonce=$8 AND clock_timestamp()<original_end RETURNING scope_json`,[this.authorityId,home.intent.accountId,id,held.ack!.body.request_sha256,JSON.stringify(held.request),actual.proofSha256,home.intent.processOwner,home.intent.nonce]);checkPublicationOriginals(held);
    check(q.rowCount===1&&canonical(q.rows[0].scope_json).equals(canonical(purpose.scope)),"effect_unknown");
   });checkPublicationOriginals(held);held.started=true;
   await custody.beginPublicationLeaseRetaining(time);held.leaseObserved=true;checkPublicationOriginals(held);
   await custody.assertPublicationCurrent(time);checkPublicationOriginals(held);
   await transport.assertCurrent();checkPublicationOriginals(held);
   // This first COMMIT prepares immutable DATA. It is NOT production admission.
   await this.#run(this.registration,home.intent.accountId,async tx=>{
    await this.#currentRegistration(tx,held);checkPublicationOriginals(held);
    const q=await tx.query(`SELECT *,clock_timestamp()<original_end AS live FROM ${SCHEMA}.managed_compiler_qualifications WHERE authority_id=$1 AND account_id=$2 AND qualification_id=$3 FOR UPDATE`,[this.authorityId,home.intent.accountId,id]);checkPublicationOriginals(held);const row=q.rows[0];
    check(row?.state==="started"&&row.live&&row.process_owner===home.intent.processOwner&&row.provision_nonce===home.intent.nonce&&row.ready_request_sha256===held.ack!.body.request_sha256,"effect_unknown");
    const {format_version,scope,qualification_id,qualification_nonce,...candidate}=purpose.scope,binding=compilerRoleBindingData(candidate),digest=hash(canonical(binding));
    await tx.query(`INSERT INTO ${SCHEMA}.managed_compiler_sdk_roles(authority_id,role_binding_sha256,image,image_receipt_sha256,inventory_sha256,binding_json,qualification_id,ready_request_sha256) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING`,[this.authorityId,digest,candidate.sdk.cas_image,candidate.sdk.source_binding_sha256,candidate.sdk.inventory.digest,JSON.stringify(binding),id,held.ack!.body.request_sha256]);checkPublicationOriginals(held);
    const role=await tx.query(`SELECT * FROM ${SCHEMA}.managed_compiler_sdk_roles WHERE authority_id=$1 AND role_binding_sha256=$2`,[this.authorityId,digest]);checkPublicationOriginals(held);
    check(role.rows.length===1&&role.rows[0].role_binding_sha256===digest&&canonical(role.rows[0].binding_json).equals(canonical(binding)),"conflict");held.prepared=preparedRoleData(role.rows[0]);
    await tx.query(`UPDATE ${SCHEMA}.managed_compiler_qualifications SET state='published',ack_json=$4 WHERE authority_id=$1 AND account_id=$2 AND qualification_id=$3 AND state='started'`,[this.authorityId,home.intent.accountId,id,JSON.stringify(held.ack)]);checkPublicationOriginals(held);
   });
   // No await between these final original-owner checks and witness issuance.
   // In particular a lost/late COMMIT never produces this witness from a row.
   checkPublicationOriginals(held);held.firstCommitKnown=true;
   held.completion=CompletedRoleCommit.observe(COMPLETION_ISSUER,held);
   await this.#recordCompletedOriginal(held);checkPublicationOriginals(held);
   held.completionRecorded=true;
   if(this.policy.controlPlane?.profile==="qualification-v1"&&this.policy.ownerMode==="provider-fenced-v1"&&this.policy.sdkInitialSource==="sdk-fixed-suite-v1")held.activationPermit=OriginalSdkRoleActivationPermit.observe(ROLE_ACTIVATION_ISSUER,held,this.authorityId,this.policy);
   return snapshot(held.ack!);
  })();
  try{return await held.promise;}catch{held.unknown=true;fail("effect_unknown");}
 }
 async #recordCompletedOriginal(held:RetainedPublication):Promise<void>{
  check(held.completion,"effect_unknown");const data=held.completion.originalData(COMPLETION_ISSUER,held),prepared=data.prepared_role;
  // Dispatch is still under ALL originals. The row itself records the genuine
  // already observed known-COMMIT event. A later unknown confirmation COMMIT
  // may leave that valid HISTORICAL fact, but never recreates this owner/ACK.
  checkPublicationOriginals(held);
  await this.#run(this.registration,held.home.intent.accountId,async tx=>{
   checkPublicationOriginals(held);
   if(this.policy.controlPlane?.profile==="qualification-v1"&&this.policy.ownerMode==="provider-fenced-v1"&&this.policy.sdkInitialSource==="sdk-fixed-suite-v1"){
    const identity=await tx.query(STAGING_DATABASE_IDENTITY_SQL);checkPublicationOriginals(held);
    check(identity.rows.length===1,"identity_changed");held.activationDatabase=stagingDatabaseIdentityData(identity.rows[0]);
   }
   await tx.query(`INSERT INTO ${SCHEMA}.managed_compiler_role_completions(authority_id,role_binding_sha256,qualification_id,ready_request_sha256,completion_sha256,completion_json) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING`,[this.authorityId,prepared.role_binding_sha256,data.qualification_scope.qualification_id,data.request_sha256,hash(canonical(data)),JSON.stringify(data)]);checkPublicationOriginals(held);
   const row=await tx.query(`SELECT completion_sha256,completion_json FROM ${SCHEMA}.managed_compiler_role_completions WHERE authority_id=$1 AND role_binding_sha256=$2`,[this.authorityId,prepared.role_binding_sha256]);checkPublicationOriginals(held);
   check(row.rows.length===1&&row.rows[0].completion_sha256===hash(canonical(row.rows[0].completion_json)),"identity_changed");validateCompletionData(row.rows[0].completion_json,prepared,this.policy);
  });checkPublicationOriginals(held);
 }
 /** Read-only HISTORICAL DATA. Never retries Started, issues a purpose, resumes
  * provider custody or makes a late ACK into an original ready/lease owner. */
 async historicalResultData(account:string,qualificationId:string):Promise<unknown>{return this.#run(this.runtime,account,async tx=>{const r=await tx.query(`SELECT q.state,q.ready_request_sha256,q.ack_json,c.completion_json FROM ${SCHEMA}.managed_compiler_qualifications q LEFT JOIN ${SCHEMA}.managed_compiler_role_completions c ON c.authority_id=q.authority_id AND c.qualification_id=q.qualification_id WHERE q.authority_id=$1 AND q.account_id=$2 AND q.qualification_id=$3`,[this.authorityId,account,qualificationId]);check(r.rows.length===1,"not_found");return snapshot({scope:"historical-compiler-qualification-data-only",prepared_role_data:r.rows[0],original_ack_success:false});});}
 /** Borrowed once from this SAME live producer; never from historical rows. */
 takeOriginalRoleActivationPermit(qualificationId:string):OriginalSdkRoleActivationPermit|undefined{
  const held=this.#publications.get(qualificationId);if(!held?.activationPermit)return undefined;
  check(!held.activationTaken&&!held.unknown,"effect_unknown");held.activationTaken=true;return held.activationPermit;
 }
 hasRetainedOwners():boolean{return this.#reservations.size!==0||this.#publications.size!==0;}
}

/** Actual service-instance check only; never a role/ready permission callback. */
export function assertCompilerSdkQualificationService(value:unknown):asserts value is RootCompilerSdkQualifications {
 check(value!==null&&typeof value==="object"&&qualificationServices.has(value as RootCompilerSdkQualifications),"unconfigured");
}
