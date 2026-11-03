// ROOT-RUN test adapter for actual PG/TLS + actual original publishers.
// Caller supplies fresh component campaigns from the retained real PG harness;
// Root/Fly/clock proof fixture DATA remains explicitly NONPHYSICAL. The adapter
// never constructs a permit, inserts success rows or invokes a model/provider.
import assert from 'node:assert/strict';
import {STAGING_DATABASE_IDENTITY_SQL} from '../cloud/dist/managed-home-provisioning/staging-role-activation-data.js';
import {StagingRoleActivationAdmin} from '../cloud/dist/managed-home-provisioning/staging-role-activation.js';
import {createHmac,randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {TRANSPORT_DOMAINS} from '../cloud/dist/managed-home-provisioning/transport.js';
import {A2_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/index.js';
import {compilerSdkSemanticMembersData} from '../cloud/dist/managed-home-provisioning/compiler-sdk-semantic-data.js';
import {compilerQualificationMac} from '../cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
const S='rudder_project_authority';
const held=[];
const sign=(key,domain,body)=>createHmac('sha256',key).update(domain).update(canonical(body,24576)).digest('hex');
const packet=(key,domain,body)=>({body,mac:sign(key,domain,body)});

function fixtureSdkProof(sdk,constraint,rootStamp){
 const semantic=compilerSdkSemanticMembersData({kind:'directory',algorithm:'sha256',digest:hash('SYNTHETIC SDK prepared'),size_bytes:230},{kind:'blob',algorithm:'sha256',digest:hash('SYNTHETIC compiler proof buck2'),size_bytes:1024},1024,0o555);
 const toolchains={};for(const[name,path]of Object.entries({buck2:'buck2',rustc:'prepared/tools/rust/bin/rustc',node:'prepared/tools/node',typescript:'prepared/tools/node',python:'prepared/tools/python/bin/python3'})){
  const digest=hash('SYNTHETIC compiler proof '+(name==='typescript'?'node':name));toolchains[name]={version:semantic.directory.digest,closure:structuredClone(semantic.directory),executable:{kind:'blob',algorithm:'sha256',digest,size_bytes:1024},executable_path:path,
   configuration:{executable_raw_sha256:digest,...(name==='typescript'?{compiler_entrypoint:'prepared/tools/typescript/lib/tsc.js'}:{})}};
 }
 const child=(text='')=>({exit_code:0,stdout_sha256:hash(text),stderr_sha256:hash(''),stdout_bytes:Buffer.byteLength(text),stderr_bytes:0});
 return {mount:{semantic,floor:{device:1,inode:2,mount_id:124,directory:true},toolchains},probe:{format_version:1,kind:'original-sdk-fixed-helper-complete-v1',helper_pid:105,helper_start_ticks:'9007199254740997',original_constraint:constraint,
  workload:{format_version:1,kind:'actual-fixed-rust-library-typescript-sdk-workload-v1',network_namespace:'net:[2001]',user_namespace:'user:[2002]',mount_namespace:'mnt:[2003]',pid_namespace:'pid:[2004]',
   started_boottime_nanoseconds:String(rootStamp-2000000n),completed_boottime_nanoseconds:String(rootStamp-1000000n),
   sdk_python:{device:'1',inode:'9007199254740998',sha256:toolchains.python.configuration.executable_raw_sha256,size_bytes:1024},compiler_capabilities:Object.fromEntries(['CapEff','CapPrm','CapInh','CapAmb','CapBnd'].map(k=>[k,'0000000000000000'])),
   buck2_static:true,sdk_root_device:'1',sdk_root_readonly:true,buck2:child('SYNTHETIC SDK Buck2 output'),rust:child(),rust_result:child(),typescript:child(),typescript_result:child('rudder-sdk-typescript-ok\n'),
   rust_artifact_sha256:hash('SYNTHETIC Rust artifact'),typescript_artifact_sha256:hash('SYNTHETIC TypeScript artifact'),models:false,source_authority:false,normal_retirement_authority:false}}};
}

export async function fixtureSdkCampaign(d,budget,key,{fixture,pair,retained}){
 const account='compiler-sdk-role-fixture',receipt=await d.registry.provisionRuntimeImageQualification(account,key),home=fixture.fly.forHome(receipt.provision.homeId);assert(home);
 const scope=receipt.compilerScope;assert(scope&&scope.scope==='disposable-fixed-compiler-sdk-only');const {sdk,...runtimeScope}=scope;runtimeScope.scope='disposable-runtime-floor-and-original-client-only';assert.deepEqual(runtimeScope,receipt.scope);assert.deepEqual(sdk,A2_COMPILER_SDK);
 const config=home.secrets.get('RUDDER_HOST_CONFIG_V1'),rootKey=Buffer.from(home.secrets.get('RUDDER_ROOT_REGISTRATION_V1').trim(),'hex'),token=home.secrets.get('RUDDER_HOST_TOKEN_V1');
 const protectedConfig=JSON.parse(config);assert.deepEqual(protectedConfig.compiler_sdk,A2_COMPILER_SDK);assert.deepEqual(protectedConfig.compiler_qualification,scope);assert.deepEqual(protectedConfig.runtime_qualification,receipt.scope);
 const worker=await d.registry.authenticateManagedHomeWorker(receipt.provision.homeId,token),original=await budget(30000),begin={version:1,kind:'begin',home_id:receipt.provision.homeId,incarnation:JSON.parse(config).incarnation,config_sha256:hash(config),root_nonce:randomUUID(),constraint:original.constraint},registrationStarted=performance.now();
 const challenge=await d.registry.beginOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_begin_v1\0',begin));
 const ns=Object.fromEntries(['mnt','user','net','pid'].map(n=>[n,{device:'1',inode:'9007199254740994'}])),proc=(pid,image)=>({pid,start_ticks:'9007199254740995',image_sha256:image,exe:{device:'1',inode:String(pid+9000)},namespaces:ns});
 const evidence={boot_id:randomUUID(),root:proc(101,d.policy.launcherSha256),engine:proc(102,d.policy.nativeSha256),service:proc(103,d.policy.serviceSha256),host_worker_tid:104,host_worker_start_ticks:'9007199254740996',root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true};
 const admitted=await d.registry.finishOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_proof_v1\0',{...challenge.body,kind:'proof',evidence}));retained.push(admitted);await admitted.assertOriginalCurrent();const registration=admitted.acknowledgementData();
 assert.equal(registration.mac,sign(rootKey,'rudder_engine_registration_ack_v1\0',registration.body));
 const derivation=Object.fromEntries(['challenge_id','engine_instance_id','credential_id','root_nonce'].map(k=>[k,challenge.body[k]])),credential=Buffer.from('rdre_'+sign(rootKey,'rudder_engine_registration_credential_v1\0',derivation));
 const socket=await pair(d,worker),t=await budget(5000),start={version:1,kind:'host_transport_begin',registration:registration.body,serial:'1',generation:randomUUID(),attempt_nonce:randomUUID(),constraint:t.constraint,remaining_milliseconds:t.remaining()};
 const ch=await d.registry.beginManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.begin,start));assert.equal(ch.mac,sign(credential,TRANSPORT_DOMAINS.challenge,ch.body));
 const commit={...ch.body,kind:'host_transport_commit',native_acceptance_sha256:hash('fixture native accepted')},active=await d.registry.commitManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.commit,commit));retained.push(active.owner);
 assert.equal(active.packet.mac,sign(credential,TRANSPORT_DOMAINS.ack,active.packet.body));
 await d.registry.confirmManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.confirm,{version:1,kind:'host_transport_confirm',registration:registration.body,serial:active.packet.body.serial,generation:active.packet.body.generation,attempt_nonce:active.packet.body.attempt_nonce,commit_sha256:active.packet.body.commit_sha256}));await active.owner.assertCurrent();
 // The actual fixture still finishes inside its stricter original 30s clock.
 // Only the explicitly synthetic Root DATA uses the fixed SDK wire domain.
 const enable=await budget(30000),sdkConstraint={...enable.constraint,domain:'compiler_sdk_qualification_v1',maximum_milliseconds:240000},end=BigInt(enable.constraint.boottime_deadline_nanoseconds),stamp=end-29900000000n;
 const selected=['package/dist/native/rudder-native','package/dist/native/rudder-jj','usr/local/bin/rudder-root-storage','usr/local/bin/rudder-machine-root'].map((installer_path,i)=>({installer_path,sha256:[d.candidate.native_sha256,d.candidate.buddy_sha256,d.candidate.service_sha256,d.candidate.launcher_sha256][i],size_bytes:1024+i}));
 const observation={format_version:1,kind:'runtime-image-ready-observation-data-v1',scope:'original-enable-ack-and-current-original-client-only',request_nonce:randomUUID(),bootstrap_nonce:begin.root_nonce,engine_instance_id:registration.body.engine_instance_id,epoch:registration.body.binding.epoch,mount_id_data:'123',original_enable_constraint:sdkConstraint,root_ack_observed_boottime_nanoseconds:String(stamp),native_received_boottime_nanoseconds:String(stamp+1n),native_issued_boottime_nanoseconds:String(stamp+2n),client_current_checked_boottime_nanoseconds:String(stamp+3n),registration_proof_sha256:registration.body.proof_sha256,config_sha256:hash(config),boot_id:evidence.boot_id,service_pid_data:103,source_authority:false,helper_enrolled:false,scheduler_ready:false,normal_retirement_authority:false,inventory:d.candidate.inventory,manifest:d.candidate.manifest,installer_input:d.candidate.installer_input,selected_executables:selected};
 const body={version:1,kind:'compiler_sdk_qualification_ready',qualification_id:receipt.scope.qualification_id,qualification_nonce:receipt.scope.qualification_nonce,registration_binding:registration.body.binding,root_nonce:begin.root_nonce,registration_proof_sha256:registration.body.proof_sha256,scope,observation,selected_executables:selected,remaining_milliseconds:enable.remaining(),sdk_proof:fixtureSdkProof(scope.sdk,sdkConstraint,stamp)};
 const request={body,mac:compilerQualificationMac(rootKey,'ready',body)},held={d,account,receipt,worker,socket,registration,registrationStarted,request,enable,rootKey,credential};retained.push(held);
 held.publish=()=>d.registry.publishRuntimeImageQualification(worker,socket.socket,request);held.rows=async()=>{const q=(await d.query(`SELECT state,ack_json FROM ${S}.managed_compiler_qualifications WHERE qualification_id=$1`,[receipt.scope.qualification_id])).rows[0];return {q,role:(await d.query(`SELECT * FROM ${S}.managed_compiler_sdk_roles`)).rows[0]??null,completion:(await d.query(`SELECT * FROM ${S}.managed_compiler_role_completions`)).rows[0]??null};};
 held.historical=()=>d.registry.runtimeImageQualificationResultData(account,receipt.scope.qualification_id);return held;
}


/** Campaign factory contract:
 * - full immutable qualification-v1/provider-fenced/source-suite policy bound
 *   BEFORE fresh DB migrations (all roles remain frozen; base admission real)
 * - originalPublish(): real service/registry publisher after original fixture
 *   registration+committed loopback socket, actual PG COMMITs; no direct row seed
 * - takePermit(): SAME registry account/key getter; configs/admin/query retained
 * - storage/image schema installed frozen for runtime tuple assertion
 * - originalSetup.remaining actual nonrenewed local fixture restriction
 * - loseActivationCommit(work): real PG COMMIT forwarded once then reject its
 *   reply on this exact admin connection (known guard phase required)
 * - otherFreshAuthorityConfigs(): SAME authorityId/policy but DISTINCT real
 *   fresh PG database, both admin/runtime protected configs (never original DB)
 * - rows(): ONLY immutable prepared/completion DATA, excludes install state
 * Entry points in both existing real-PG followups now supply this contract.
 * ALL new scenarios remain UNRUN in this source-only proposal.
 */
export async function qualifyOriginalStagingActivations({freshCampaign,role}){
 assert(['sdk','runtime'].includes(role));
 const checks=[];
 {
  const c=await freshCampaign(role,'known');held.push(c);await c.originalPublish();
  const permit=c.takePermit();assert(permit);held.push(permit);
  assert.throws(()=>c.takePermit());
  const admin=new StagingRoleActivationAdmin(c.adminConfig,c.runtimeConfig,c.policy,c.originalSetup);held.push(admin);
  const savedData=JSON.parse(JSON.stringify({role,completion_json:(await c.rows()).completion_json}));
  await assert.rejects(admin.activateRetaining(savedData));
  const before=hash(canonical(await c.rows()));await admin.activateRetaining(permit);
  const prefix=role==='sdk'?'managed_compiler':'managed_runtime';
  assert.equal((await c.query(`SELECT state FROM ${S}.${prefix}_role_installation WHERE authority_id=$1`,[c.authorityId])).rows[0].state,'ready');
  assert.equal(hash(canonical(await c.rows())),before,'activation must preserve immutable prepared/completion bytes');
  await assert.rejects(admin.activateRetaining(permit));
  if(role==='runtime'){
   const row=(await c.query(`SELECT image,image_receipt_sha256,manifest_sha256,installer_input_json FROM ${S}.managed_image_qualifications WHERE authority_id=$1`,[c.authorityId])).rows[0];
   assert.deepEqual(row,{image:c.policy.image,image_receipt_sha256:c.policy.runtimeImageReceiptSha256,manifest_sha256:c.policy.imagePreseed.fullManifestSha256,installer_input_json:c.policy.imagePreseed.installerInput});
   assert.equal((await c.query(`SELECT count(*)::text AS n FROM ${S}.managed_image_publications WHERE authority_id=$1`,[c.authorityId])).rows[0].n,'0','image tuple is not an S3 publication');
  }
  checks.push(role+' known original permit only, immutable rows preserved, one shot');
  const lost=await freshCampaign(role,'activation-commit-lost');held.push(lost);await lost.originalPublish();const original=lost.takePermit();held.push(original);
  const uncertain=new StagingRoleActivationAdmin(lost.adminConfig,lost.runtimeConfig,lost.policy,lost.originalSetup);held.push(uncertain);
  await lost.loseActivationCommit(()=>assert.rejects(uncertain.activateRetaining(original)));
  assert.equal(uncertain.statusData().attempts[0].known_commit,false);assert.equal(uncertain.statusData().attempts[0].unknown,true);
  await assert.rejects(uncertain.activateRetaining(original));assert.throws(()=>lost.takePermit());
  // Actual row may have committed; no query/resubmission recreates the owner.
  assert.equal((await lost.query(`SELECT state FROM ${S}.${prefix}_role_installation WHERE authority_id=$1`,[lost.authorityId])).rows[0].state,'ready');
  checks.push(role+' real forwarded COMMIT/lost reply stays unknown and consumed');
  const wrong=await freshCampaign(role,'wrong-database');held.push(wrong);await wrong.originalPublish();const wrongPermit=wrong.takePermit();held.push(wrongPermit);
  const foreign=await wrong.otherFreshAuthorityConfigs();held.push(foreign);
  const refused=new StagingRoleActivationAdmin(foreign.admin,foreign.runtime,wrong.policy,wrong.originalSetup);held.push(refused);
  await wrong.observeForeignDatabaseRefusal(()=>assert.rejects(refused.activateRetaining(wrongPermit)));
  assert.equal((await wrong.query(`SELECT state FROM ${S}.${prefix}_role_installation WHERE authority_id=$1`,[wrong.authorityId])).rows[0].state,'frozen');
  checks.push(role+' exact original PG database/postmaster identity required');
 }
 return {scope:'actual-PG-original-staging-activation-component-only',role,checks,physicalRoot:false,physicalCompilerSdk:false,providerOwnerQualified:false,actualFly:false,actualS3:false,sourceAuthority:false,clockSafety:false,ownersRetained:true};
}

/** Connect an EXISTING actual original fixture publication, never row DATA, to
 * the admin transition. All pools/PG clients/packets/WS stay retained by caller. */
export function activationCampaignOwner({role,d,c,pg,foreignDatabase,retained}){
 assert(['sdk','runtime'].includes(role));const prefix=role==='sdk'?'managed_compiler':'managed_runtime';let foreignObserved;
 const owner={authorityId:d.authorityId,policy:d.policy,adminConfig:d.config('postgres'),runtimeConfig:d.config('rudder_managed_runtime'),originalSetup:c.enable,
  originalPublish:()=>c.publish(),takePermit:()=>d.registry.takeOriginalRoleActivationPermit(c.account,c.receipt.provision.commandKey),query:(sql,args=[])=>d.query(sql,args),
  rows:async()=>{const actual=await c.rows();return {role:actual.role,completion:actual.completion};},
  otherFreshAuthorityConfigs:async()=>{
   const other=foreignDatabase();assert(other&&other!==d&&other.name!==d.name&&other.createKnown);
   // Actual different existing fixture DB, not an invented database identity.
   // Match authority DATA only so the real PG identity guard is what refuses.
   foreignObserved=other;return {admin:{...other.config('postgres'),authorityId:d.authorityId},runtime:{...other.config('rudder_managed_runtime'),authorityId:d.authorityId}};
  },
  observeForeignDatabaseRefusal:async work=>{
   assert(foreignObserved);const previous=pg.Client.prototype.query;let identityObserved=false,installationRead=false;const io=[];retained.push(io);
   pg.Client.prototype.query=function(query,...args){
    const sql=typeof query==='string'?query:query?.text,eligible=this.connectionParameters.database===foreignObserved.name&&this.connectionParameters.user==='postgres';
    const actual=previous.call(this,query,...args);
    if(eligible&&sql===STAGING_DATABASE_IDENTITY_SQL){io.push(this,actual);return actual.then(value=>{assert.equal(value.rows.length,1);assert.equal(value.rows[0].database,foreignObserved.name);assert.notEqual(value.rows[0].database,d.name);identityObserved=true;return value;});}
    if(eligible&&typeof sql==='string'&&sql.includes(`${S}.${prefix}_role_installation`))installationRead=true;
    return actual;
   };
   try{await work();assert(identityObserved&&!installationRead,'actual foreign PG identity must refuse BEFORE installation/row fallback');}
   finally{pg.Client.prototype.query=previous;}
  },
  loseActivationCommit:async work=>{
   const previous=pg.Client.prototype.query,phase=new WeakMap();let updateKnown=false,commitKnown=false,fired=false;const io=[];retained.push(io);
   pg.Client.prototype.query=function(query,...args){
    const sql=typeof query==='string'?query:query?.text,eligible=this.connectionParameters.database===d.name&&this.connectionParameters.user==='postgres';
    const actual=previous.call(this,query,...args);
    if(eligible&&typeof sql==='string'&&sql.startsWith(`UPDATE ${S}.${prefix}_role_installation SET state='ready'`)){
     io.push(this,actual);return actual.then(value=>{assert.equal(value.rowCount,1);updateKnown=true;phase.set(this,'actual-activation-update-acknowledged');return value;});
    }
    // Explicit phase presence prevents undefined===undefined from faulting an
    // unrelated BEGIN/read COMMIT. Execute the SAME real server COMMIT first.
    if(eligible&&sql==='COMMIT'&&phase.get(this)==='actual-activation-update-acknowledged'&&!fired){
     fired=true;io.push(actual);return actual.then(()=>{commitKnown=true;throw Object.assign(Error('fixture lost original admin COMMIT reply after actual server commit'),{code:'ECONNRESET'});});
    }
    return actual;
   };
   try{await work();assert(updateKnown&&commitKnown&&fired,'must actually forward exact admin UPDATE/COMMIT before losing reply');}
   finally{pg.Client.prototype.query=previous;}
  }
 };
 retained.push(owner);return owner;
}

/** Runtime's SDK admission prerequisite is produced, completed and explicitly
 * activated by its OWN original SDK publisher in the SAME actual fresh DB. */
export async function activateSdkPrecursorRetaining({d,budget,fixture,pair,retained}){
 const sd=d.sdkPrecursor;assert(sd&&sd.registry!==d.registry&&sd.name===d.name);
 const c=await fixtureSdkCampaign(sd,budget,'activation-sdk-prerequisite',{fixture,pair,retained});
 const state={campaign:c,permit:null,admin:null,promise:null,known:false};sd.activation=state;retained.push(state);
 await c.publish();state.permit=sd.registry.takeOriginalRoleActivationPermit(c.account,c.receipt.provision.commandKey);assert(state.permit);
 state.admin=new StagingRoleActivationAdmin(sd.config('postgres'),sd.config('rudder_managed_runtime'),sd.policy,c.enable);
 state.promise=state.admin.activateRetaining(state.permit);await state.promise;state.known=true;
 assert.equal((await sd.query(`SELECT state FROM ${S}.managed_compiler_role_installation WHERE authority_id=$1`,[sd.authorityId])).rows[0].state,'ready');
 for(const table of ['managed_runtime_role_installation','managed_owner_clock_installation','managed_storage_installation'])assert.equal((await sd.query(`SELECT state FROM ${S}.${table} WHERE authority_id=$1`,[sd.authorityId])).rows[0].state,'frozen');
 return state;
}
