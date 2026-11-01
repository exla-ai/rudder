// All definitions UNRUN. Synthetic DATA/HMAC and negative owner boundaries
// exercise no provider, actual Root key/channel, mount or scheduler authority.
import test from 'node:test';
import assert from 'node:assert/strict';
import {createHmac,randomUUID} from 'node:crypto';
import {setTimeout as delay} from 'node:timers/promises';
import {RuntimeImageQualificationDriver} from '../cloud/dist/managed-home-provisioning/runtime-qualification-driver.js';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {managedHomeProtectedConfigData} from '../cloud/dist/managed-home-provisioning/protected-config.js';
import {RootRuntimeImageQualifications,RuntimeQualificationCutoff} from '../cloud/dist/managed-home-provisioning/runtime-qualification.js';
import {FlyManagedHomeProvider,assertOriginalDisposableMachineCustody,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {ManagedHomeRegistrationWire} from '../cloud/dist/managed-home-provisioning/wire.js';
import {canonical,hash,validatePolicy} from '../cloud/dist/managed-home-provisioning/types.js';
import {reserveFreshProjects,freshSpec} from '../cloud/dist/managed-home-provisioning/fresh.js';
import {RUNTIME_QUALIFICATION_DOMAINS,validateRuntimeQualificationCandidate,validateRuntimeQualificationScope,authenticateRuntimeQualificationReady,runtimeQualificationMac} from '../cloud/dist/managed-home-provisioning/runtime-qualification-protocol.js';
import {policy,candidate,scope,ready} from './fixtures/managed-runtime-qualification/data.mjs';
const fixtureKey=Buffer.alloc(32,11); // TEST ONLY. No production credential.
const packet=b=>({body:b,mac:runtimeQualificationMac(fixtureKey,'ready',b)});
function intent(p=policy()) {const storeId=randomUUID(),freshProjects=reserveFreshProjects(['Inert fixture']);return {authorityId:randomUUID(),accountId:'fixture-account',homeId:'mh-'+randomUUID(),commandKey:'fixture-key',requestHash:'a'.repeat(64),epoch:'1',nonce:randomUUID(),appName:'new-app',networkName:'new-network',storeId,incarnation:randomUUID(),processOwner:randomUUID(),policy:p,spec:freshSpec(freshProjects,storeId,p),freshProjects};}

test('qualification scope is strict same-image DATA and changes the ONE canonical config hash',()=>{
 const p=validatePolicy(policy()),i=intent(p),m={id:'machine'};
 const absent=managedHomeProtectedConfigData(i,m);assert.equal(JSON.parse(absent).runtime_qualification,undefined);
 i.runtimeQualification=scope(p);const raw=managedHomeProtectedConfigData(i,m);
 assert.notEqual(hash(raw),hash(absent));assert.deepEqual(JSON.parse(raw).runtime_qualification,i.runtimeQualification);
 assert.deepEqual(validateRuntimeQualificationCandidate(candidate(p),p),candidate(p));
 for(const mutate of [s=>s.inventory.kind='directory',s=>s.inventory.size_bytes=16777217,s=>s.manifest.size_bytes=33554433,s=>s.installer_input.size_bytes=65537,s=>s.image+=':foreign',s=>s.image_receipt_sha256='a'.repeat(64),s=>s.source_manifest_sha256='0'.repeat(64),s=>s.inventory.path='/tmp/x',s=>s.ready=true,s=>s.native_sha256='1'.repeat(64),s=>s.qualification_nonce=randomUUID().toUpperCase(),s=>delete s.buddy_sha256]){
  const s=scope(p);mutate(s);assert.throws(()=>validateRuntimeQualificationScope(s,p));
 }
});

test('25-field observation and four exact executable DATA records authenticate only their own domain',()=>{
 const b=ready(),e=packet(b);assert.equal(Object.keys(b.observation).length,25);assert.equal(Object.keys(b.registration_binding).length,21);
 assert.deepEqual(authenticateRuntimeQualificationReady(e,fixtureKey,b.scope,b.registration_binding,b.registration_proof_sha256),e);
 const independently=createHmac('sha256',fixtureKey).update('rudder_runtime_image_qualification_ready_v1\0').update(canonical(b,24576)).digest('hex');assert.equal(e.mac,independently);assert(RUNTIME_QUALIFICATION_DOMAINS.ready.endsWith('\0'));
 for(const e of [{body:b,mac:runtimeQualificationMac(fixtureKey,'ack',b)},{body:b,mac:'a'.repeat(64)},{body:b,mac:'A'.repeat(64)}])assert.throws(()=>authenticateRuntimeQualificationReady(e,fixtureKey,b.scope,b.registration_binding,b.registration_proof_sha256));
});

test('valid test MAC cannot hide a foreign original, reordered executable or late original cutoff',()=>{
 const original=ready();for(const mutate of [b=>b.qualification_nonce=randomUUID(),b=>b.observation.epoch='2',b=>b.observation.config_sha256='2'.repeat(64),b=>b.observation.inventory.digest='1'.repeat(64),b=>b.observation.selected_executables.reverse(),b=>b.selected_executables.reverse(),b=>b.selected_executables[0].installer_path='/arbitrary',b=>b.observation.source_authority=true,b=>delete b.observation.manifest,b=>b.observation.current=true,b=>b.remaining_milliseconds=30001,b=>b.observation.client_current_checked_boottime_nanoseconds=b.observation.original_enable_constraint.boottime_deadline_nanoseconds,b=>b.observation.native_received_boottime_nanoseconds='1']){
  const b=structuredClone(original);mutate(b);assert.throws(()=>authenticateRuntimeQualificationReady(packet(b),fixtureKey,original.scope,original.registration_binding,original.registration_proof_sha256));
 }
});

test('JSON/prototype/lookalike methods and inventory provider replies cannot mint actual created-machine custody',()=>{
 const home={intent:{runtimeQualification:{}},phase:'routable'};
 for(const x of [{},{matchesOriginal:()=>true,beginPublicationLeaseRetaining:async()=>{},assertPublicationCurrent:async()=>{}},JSON.parse('{"originalHttpAck":true}')])assert.throws(()=>assertOriginalDisposableMachineCustody(x,home),{code:'identity_changed'});
 const provider=new FlyManagedHomeProvider({request:async()=>{throw Error('must not request');}});
 assert.throws(()=>provider.captureOriginalDisposableMachine(home),{code:'identity_changed'});
});

test('raw proof/row/caller booleans cannot publish even with a syntactically valid test HMAC',async()=>{
 let calls=0;const pool={run:async()=>{calls++;throw Error('must not reach SQL');}},api=new RootRuntimeImageQualifications(pool,pool,randomUUID(),policy()),b=ready();
 const fakePurpose={scope:b.scope,assertOriginal:()=>{},original:originalTime(30000)};
 await assert.rejects(api.publishRetaining(fakePurpose,{matchesOriginal:()=>true},{},{},{},fixtureKey,packet(b),originalTime(30000)),{code:'identity_changed'});assert.equal(calls,0);assert.equal(api.hasRetainedOwners(),false);
});

test('legacy image qualification/ready-like rows cannot satisfy the new runtime-role consumer',async()=>{
 const queried=[];const pool={run:async(_account,work)=>({status:'committed',value:await work({query:async sql=>{queried.push(sql);return {rows:[{state:'ready',image_receipt_sha256:policy().runtimeImageReceiptSha256,manifest_sha256:policy().imagePreseed.fullManifestSha256}]};}})})};
 const api=new RootRuntimeImageQualifications(pool,pool,randomUUID(),policy());await assert.rejects(api.assertPublishedRole(policy()),{code:'unconfigured'});
 assert(queried.some(s=>s.includes('managed_runtime_role_installation')));assert(!queried.some(s=>s.includes('managed_image_qualifications')));
});

test('ABSENT profile adds zero role-ledger queries and preserves original receipt bytes',async()=>{
 const p=policy();delete p.imagePreseed.runtimeMount;let roleCalls=0,lookupCalls=0;
 const quals=new RootRuntimeImageQualifications({run:()=>{roleCalls++;throw Error('must not query');}},{},randomUUID(),p);await quals.assertPublishedRole(p);assert.equal(roleCalls,0);
 const i=intent(p),projectKeys=['Existing receipt'];i.requestHash=hash(canonical({accountId:i.accountId,commandKey:i.commandKey,projectKeys,policy:p}));const bound={intent:i,phase:'routable',acks:{},tokenHash:'f'.repeat(64)};
 const api=new ManagedHomeRegistry({lookupKey:async()=>{lookupCalls++;return bound;}},{},p,Buffer.alloc(32,1));
 const reply=await api.provisionManagedHome(i.accountId,i.commandKey,projectKeys);assert.equal(reply.homeId,i.homeId);assert.equal(reply.nativeIngressReady,false);assert.equal(lookupCalls,1);assert.equal(roleCalls,0);
});

test('actual initial clock survives repeated receipt windows; delayed/new HTTP clocks cannot renew it',async()=>{
 const original=originalTime(1000),cutoff=new RuntimeQualificationCutoff(original),first=cutoff.receiptWindow(30000),second=cutoff.receiptWindow(30000);
 assert(cutoff.matchesOriginal(original));assert(!cutoff.matchesOriginal(originalTime(30000)));assert.equal(first.end,original.end);assert.equal(second.end,original.end);
 await delay(1100);assert.throws(()=>first.remaining(),{code:'expired'});assert.throws(()=>second.remaining(),{code:'expired'});assert.throws(()=>cutoff.receiptWindow(30000),{code:'expired'});assert.throws(()=>JSON.stringify(cutoff),{code:'invalid_request'});
});

test('reopened registry/serialized home/new deadline cannot restore the original campaign or current socket',async()=>{
 const i=intent();i.runtimeQualification=scope(i.policy);const home={intent:i,phase:'routable',acks:{configure:{id:'machine'}},tokenHash:hash('rdrmh_'+'1'.repeat(64))};
 let beginCalls=0;const reg=new ManagedHomeRegistry({worker:async()=>home},{},i.policy,Buffer.alloc(32,1),{begin:async()=>{beginCalls++;}},{},{},{},{},candidate(i.policy));
 await assert.rejects(reg.authenticateManagedHomeWorker(i.homeId,'rdrmh_'+'1'.repeat(64)),{code:'identity_changed'});
 await assert.rejects(reg.publishRuntimeImageQualification({}, {},packet(ready())),{code:'identity_changed'});assert.equal(beginCalls,0);
});

test('failed/unknown qualification wire result stays retained and the same nonce is never replayed',async()=>{
 let calls=0;const wire=new ManagedHomeRegistrationWire({publishRuntimeImageQualification:async()=>{calls++;throw Error('unknown original COMMIT');}}),worker={homeId:'home',assertCredentialsCurrent:async()=>{}},b=ready();const msg={type:'runtime_image_qualification_ready_v1',rpc_id:b.observation.request_nonce,packet:packet(b)};
 assert.equal(await wire.receive(worker,msg,async()=>{throw Error('must not send');},{}),true);assert.equal(calls,1);
 await assert.rejects(wire.receive(worker,msg,async()=>{},{}),{code:'effect_unknown'});assert.equal(calls,1);
});

test('a generic role-permission callback cannot replace the actual ledger service',async()=>{
 let queries=0,effects=0;const fake={assertPublishedRole:async()=>{queries++;}},api=new ManagedHomeRegistry({lookupKey:async()=>{effects++;}},{},policy(),Buffer.alloc(32,1),undefined,undefined,undefined,undefined,fake);
 await assert.rejects(api.provisionManagedHome('fixture-account','new',['Fresh']),{code:'unconfigured'});assert.equal(queries,0);assert.equal(effects,0);
});

test('standalone driver rejects production/foreign origin before reading any provider credential',()=>{
 const c={policy:policy()};Object.defineProperty(c,'flyToken',{get(){throw Error('must not read token');}});
 assert.throws(()=>new RuntimeImageQualificationDriver(c,candidate(),'fixture-account','key'),{code:'unconfigured'});
});
