// Pure DATA/model/API-boundary regressions. UNRUN; no PG/provider/process/Root
// credential/ACK, mounted runtime, role qualification, scheduler or model claim.
import test from 'node:test';
import assert from 'node:assert/strict';
import {createHmac,randomUUID} from 'node:crypto';
import {createManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/index.js';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {RootEngineRegistration,binding} from '../cloud/dist/managed-home-provisioning/registration.js';
import {managedHomeProtectedConfigData} from '../cloud/dist/managed-home-provisioning/protected-config.js';
import {canonical,hash,validatePolicy,controlOrigin,runtimeMountConfigData} from '../cloud/dist/managed-home-provisioning/types.js';
import {reserveFreshProjects,freshSpec} from '../cloud/dist/managed-home-provisioning/fresh.js';
import {machineConfig,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';

const ref=(kind,digest,size_bytes)=>({kind,algorithm:'sha256',digest:digest.repeat(64),size_bytes});
function policy(){return {organization:'fixture',region:'iad',image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),
 runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:'2'.repeat(64),entries:[{path:'/opt/rudder-managed-runtime/v1/test',sha256:'3'.repeat(64),size_bytes:1}]},
 runtimeImageReceiptSha256:'4'.repeat(64),providerReceiptSha256:'5'.repeat(64),durabilityReceiptSha256:'6'.repeat(64),freshVolumeReceiptSha256:'7'.repeat(64),nativeSha256:'8'.repeat(64),launcherSha256:'9'.repeat(64),serviceSha256:'a'.repeat(64),nodeSha256:'b'.repeat(64),sidecarPolicySha256:'c'.repeat(64),volumeGb:1,cpus:1,memoryMb:512,userName:'Fixture',userEmail:'fixture@example.invalid',
 imagePreseed:{fullManifestSha256:'d'.repeat(64),installerInput:ref('directory','e',32)}};}
function selected(){const p=policy();p.imagePreseed.runtimeMount={profile:'baked-cas-v1',inventory:ref('blob','f',16*1024*1024),
 image:p.image,imageReceiptSha256:p.runtimeImageReceiptSha256,fullManifestSha256:p.imagePreseed.fullManifestSha256,installerInput:structuredClone(p.imagePreseed.installerInput)};return p;}
function intent(p=policy()){
 const freshProjects=reserveFreshProjects(['Fresh fixture']),storeId=randomUUID(),nonce=randomUUID();
 return {authorityId:randomUUID(),accountId:'fixture-account',homeId:'mh-'+randomUUID(),commandKey:'create',requestHash:'a'.repeat(64),epoch:'1',nonce,
 appName:'rudder-mh-'+nonce.replaceAll('-',''),networkName:'rudder-mhn-'+randomUUID().replaceAll('-',''),storeId,incarnation:randomUUID(),processOwner:randomUUID(),policy:p,spec:freshSpec(freshProjects,storeId,p),freshProjects};
}
function home(p=selected()){
 const i=intent(p),v={id:'volume',name:'fresh',region:'iad',sizeGb:1};
 const m={id:'machine',instanceId:'instance-original',state:'started',name:i.homeId,region:'iad',configSha256:'e'.repeat(64),config:machineConfig(i,v,true)};
 return {intent:i,phase:'routable',tokenHash:'f'.repeat(64),acks:{app:{id:'app',name:i.appName,network:i.networkName,organization:p.organization,networkCidr:'fd00::/64'},volume:v,machine:m,configure:m,start:m}};
}
const packet=(key,domain,body)=>({body,mac:createHmac('sha256',key).update(domain).update(canonical(body,24*1024)).digest('hex')});
const constraint=()=>({format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'9007199254740993000'});
function evidence(b){const s={device:'1',inode:'2'},namespaces={mnt:s,user:s,net:s,pid:s},proc=(pid,image)=>({pid,start_ticks:'1',image_sha256:image,exe:s,namespaces});return {boot_id:randomUUID(),root:proc(101,b.launcher_sha256),engine:proc(102,b.native_sha256),service:proc(103,b.service_sha256),host_worker_tid:104,host_worker_start_ticks:'1',root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true};}

test('absent runtime descriptor preserves policy bytes and existing admission',()=>{
 const p=policy();assert.deepEqual(validatePolicy(p),p);assert.equal(runtimeMountConfigData(p),undefined);
 assert(!Object.hasOwn(validatePolicy(p).imagePreseed,'runtimeMount'));
 const i=intent(p),m={id:'machine'};assert(!managedHomeProtectedConfigData(i,m).toString().includes('runtime_mount'));
});
test('same-image descriptor validates16MiB raw Blob and emits only native profile/inventory DATA',()=>{
 const p=selected(),parsed=validatePolicy(p);assert.deepEqual(parsed,p);
 const i=intent(parsed),raw=managedHomeProtectedConfigData(i,{id:'machine'}),c=JSON.parse(raw);
 assert.deepEqual(c.image_preseed.runtime_mount,{profile:'baked-cas-v1',inventory:p.imagePreseed.runtimeMount.inventory});
 assert.deepEqual(Object.keys(c.image_preseed.runtime_mount).sort(),['inventory','profile']);
 assert.equal(c.image_preseed.image,p.image);assert.equal(c.image_preseed.full_manifest_sha256,p.imagePreseed.fullManifestSha256);assert.equal(c.image_preseed.image_receipt_sha256,p.runtimeImageReceiptSha256);
 assert.equal(c.root_ready,undefined);assert.equal(c.registration_key,undefined);
 // Returned DATA cannot mutate the validated input snapshot.
 c.image_preseed.runtime_mount.inventory.digest='a'.repeat(64);assert.equal(parsed.imagePreseed.runtimeMount.inventory.digest,'f'.repeat(64));
});
test('descriptor rejects malformed/foreign refs, wrong bound metadata and authority-like additions',()=>{
 for(const mutate of [p=>p.imagePreseed.runtimeMount=null,p=>p.imagePreseed.runtimeMount.profile='legacy-v1',p=>p.imagePreseed.runtimeMount.path='/arbitrary',p=>p.imagePreseed.runtimeMount.ready=true,
 p=>p.imagePreseed.runtimeMount.image='registry.fly.io/rudder-workers-exla@sha256:'+'2'.repeat(64),p=>p.imagePreseed.runtimeMount.imageReceiptSha256='a'.repeat(64),p=>p.imagePreseed.runtimeMount.fullManifestSha256='b'.repeat(64),p=>p.imagePreseed.runtimeMount.installerInput.digest='c'.repeat(64),
 p=>p.imagePreseed.runtimeMount.inventory.kind='directory',p=>p.imagePreseed.runtimeMount.inventory.algorithm='blake3',p=>p.imagePreseed.runtimeMount.inventory.digest='0'.repeat(64),p=>p.imagePreseed.runtimeMount.inventory.digest='A'.repeat(64),p=>p.imagePreseed.runtimeMount.inventory.size_bytes=0,p=>p.imagePreseed.runtimeMount.inventory.size_bytes=16777217,p=>p.imagePreseed.runtimeMount.inventory.size_bytes=1.5,p=>p.imagePreseed.runtimeMount.inventory.size_bytes=NaN,p=>p.imagePreseed.runtimeMount.inventory.uri='file:///x',p=>p.imagePreseed.runtimeMount.installerInput.size_bytes=65537,p=>p.imagePreseed.runtimeMount.installerInput.path='/x',p=>delete p.imagePreseed.runtimeMount.inventory]){
  const p=selected();mutate(p);assert.throws(()=>validatePolicy(p),{code:'invalid_request'});
 }
});
test('inventory is bound into canonical config SHA while registration stays21fields',()=>{
 const h=home(),one=managedHomeProtectedConfigData(h.intent,h.acks.machine),prior=structuredClone(h.intent);delete prior.policy.imagePreseed.runtimeMount;
 assert.notEqual(hash(one),hash(managedHomeProtectedConfigData(prior,h.acks.machine)));
 const changed=structuredClone(h.intent);changed.policy.imagePreseed.runtimeMount.inventory.digest='a'.repeat(64);
 assert.notEqual(hash(one),hash(managedHomeProtectedConfigData(changed,h.acks.machine)));
 const b=binding(h,hash(one));assert.equal(Object.keys(b).length,21);assert.equal(b.config_sha256,hash(one));
});
test('no caller QA flag or direct selected registry can replace the actual role consumer',async()=>{
 const config={mode:'postgres',policy:selected(),qualified:true};
 assert.throws(()=>createManagedHomeRegistry(config),{code:'invalid_request'});
 const direct=new ManagedHomeRegistry({}, {},selected(),Buffer.alloc(32,1));
 await assert.rejects(direct.provisionManagedHome('account','new',['Fresh']),{code:'unconfigured'});
});
test('legacy ready-like rows cannot bypass direct registry effect/registration role refusal',async()=>{
 let queries=0,effects=0;
 const store={authorityId:randomUUID(),lookupKey:async()=>{queries++;return {phase:'routable',runtime_ready:true};},close:async()=>{}},provider={execute:async()=>{effects++;throw Error('must not run');}};
 const api=new ManagedHomeRegistry(store,provider,selected(),Buffer.alloc(32,1),{begin:async()=>{effects++;},finish:async()=>{effects++;},close:async()=>{}});
 await assert.rejects(api.provisionManagedHome('account','new',['Fresh']),{code:'unconfigured'});
 await assert.rejects(api.beginOriginalEngineRegistration({},{}),{code:'identity_changed'});
 await assert.rejects(api.finishOriginalEngineRegistration({},{}),{code:'identity_changed'});
 assert.equal(queries,0);assert.equal(effects,0);await api.close();
});
test('actual registration BEGIN and FINISH authenticate the shared new config SHA before any PG transaction',async()=>{
 const h=home(),key=Buffer.alloc(32,11),expected=hash(managedHomeProtectedConfigData(h.intent,h.acks.machine));let queries=0;
 const reg=new RootEngineRegistration({run:async()=>{queries++;throw Error('deliberate model PG unavailable');}},h.intent.authorityId);
 const body={version:1,kind:'begin',home_id:h.intent.homeId,incarnation:h.intent.incarnation,config_sha256:expected,root_nonce:randomUUID(),constraint:constraint()};
 await assert.rejects(reg.begin(h,key,packet(key,'rudder_engine_registration_begin_v1\0',body),expected,originalTime(1000)),{code:'effect_unknown'});assert.equal(queries,1);
 const oldIntent=structuredClone(h.intent);delete oldIntent.policy.imagePreseed.runtimeMount;const old=hash(managedHomeProtectedConfigData(oldIntent,h.acks.machine));
 const wrong={...body,config_sha256:old};await assert.rejects(reg.begin(h,key,packet(key,'rudder_engine_registration_begin_v1\0',wrong),expected,originalTime(1000)),{code:'identity_changed'});assert.equal(queries,1);
 const b=binding(h,expected),proof={version:1,kind:'proof',challenge_id:randomUUID(),challenge_nonce:'b'.repeat(64),root_nonce:body.root_nonce,engine_instance_id:randomUUID(),credential_id:randomUUID(),binding:b,constraint:body.constraint,evidence:evidence(b)};
 await assert.rejects(reg.finish(h,key,packet(key,'rudder_engine_registration_proof_v1\0',proof),expected,originalTime(1000)),{code:'effect_unknown'});assert.equal(queries,2);
 const oldProof={...proof,binding:binding(h,old)};await assert.rejects(reg.finish(h,key,packet(key,'rudder_engine_registration_proof_v1\0',oldProof),expected,originalTime(1000)),{code:'identity_changed'});assert.equal(queries,2);
});

// Golden compatibility producer copied from the exact baseline #hostConfig.
// It is test-only and deliberately retains the PRE-runtime descriptor bytes.
function previousConfig(i,machine){return canonical({version:1,machine_id:machine.id,store_id:i.storeId,incarnation:i.incarnation,account_id:i.accountId,home_id:i.homeId,cloud_origin:controlOrigin(i.policy),state_directory:i.spec.stateDirectory,store_policy:i.spec.storePolicy,projects:i.spec.projects,runtime_profile:{FlyManagedImageV1:{user_name:i.spec.userName,user_email:i.spec.userEmail}},runtime_manifest:i.policy.runtimeManifest,...(i.policy.ownerLeaseSafety?{owner_lease_safety:{qualification_receipt_sha256:i.policy.ownerLeaseSafety.qualificationReceiptSha256,margin_milliseconds:i.policy.ownerLeaseSafety.marginMilliseconds,engine_rate_error_ppm:i.policy.ownerLeaseSafety.engineRateErrorPpm,database_rate_error_ppm:i.policy.ownerLeaseSafety.databaseRateErrorPpm,max_positive_clock_step_milliseconds:i.policy.ownerLeaseSafety.maxPositiveClockStepMilliseconds}}:{}),...(i.policy.imagePreseed?{image_preseed:{image:i.policy.image,image_receipt_sha256:i.policy.runtimeImageReceiptSha256,full_manifest_sha256:i.policy.imagePreseed.fullManifestSha256,...(i.policy.imagePreseed.batchTransport?{batch_transport:i.policy.imagePreseed.batchTransport}:{})}}:{})},32*1024);}
test('legacy protected bytes are unchanged across absent/V1/batch/origin/lease options',()=>{
 for(const preseed of ['none','v1','batch'])for(const origin of ['none','production-v1','qualification-v1'])for(const clock of [false,true]){
  const p=policy();if(preseed==='none')delete p.imagePreseed;
  if(preseed==='batch'){p.imagePreseed.batchTransport='batch-v1';p.imagePreseed.batchReceiptSha256='a'.repeat(64);}
  if(origin!=='none')p.controlPlane={profile:origin,qualificationReceiptSha256:'b'.repeat(64)};
  if(clock)p.ownerLeaseSafety={qualificationReceiptSha256:'c'.repeat(64),marginMilliseconds:250,engineRateErrorPpm:1000,databaseRateErrorPpm:1000,maxPositiveClockStepMilliseconds:100};
  const i=intent(validatePolicy(p)),m={id:'machine'};
  assert(managedHomeProtectedConfigData(i,m).equals(previousConfig(i,m)));
  assert(!managedHomeProtectedConfigData(i,m).toString().includes('runtime_mount'));
 }
});
