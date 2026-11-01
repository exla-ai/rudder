// Component DATA/model tests only. UNRUN by author; no real PG/provider/Root,
// credentials, configured sidecar, storage, models or authority qualification.
import test from 'node:test';
import assert from 'node:assert/strict';
import {createHmac,randomUUID} from 'node:crypto';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {RootEngineRegistration} from '../cloud/dist/managed-home-provisioning/registration.js';
import {machineConfig,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {canonical,hash,validatePolicy,controlOrigin,CONTROL_ORIGIN,QUALIFICATION_CONTROL_ORIGIN,EFFECTS} from '../cloud/dist/managed-home-provisioning/types.js';
import {validateScope,uploadControlOrigin,httpExchange} from '../cloud/worker/root-upload.mjs';

const policy=()=>({organization:'fixture',region:'iad',image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:'2'.repeat(64),entries:[{path:'/opt/rudder-managed-runtime/v1/test',sha256:'3'.repeat(64),size_bytes:1}]},runtimeImageReceiptSha256:'4'.repeat(64),providerReceiptSha256:'5'.repeat(64),durabilityReceiptSha256:'6'.repeat(64),freshVolumeReceiptSha256:'7'.repeat(64),nativeSha256:'8'.repeat(64),launcherSha256:'9'.repeat(64),serviceSha256:'a'.repeat(64),nodeSha256:'b'.repeat(64),sidecarPolicySha256:'c'.repeat(64),volumeGb:1,cpus:1,memoryMb:512,userName:'Fixture',userEmail:'fixture@example.invalid'});
const selected=profile=>({...policy(),controlPlane:{profile,qualificationReceiptSha256:'d'.repeat(64)}});
const scope=()=>({version:1,home_id:'fixture-home',workspace_id:'fixture-project',transfer_id:randomUUID(),request_nonce:randomUUID(),mode:'fresh',manifest:null,maximum_manifest_bytes:null,maximum_objects:128,maximum_bytes:4194304,maximum_object_bytes:1048576,constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'9007199254740993000'}});
const uploadConfig=origin=>({version:1,machine_id:'machine',store_id:'store',incarnation:'incarnation',account_id:'account',home_id:'fixture-home',cloud_origin:origin,projects:[{alias:'fixture-project'}]});

test('omitted profile preserves exact policy bytes and production; explicit qualification changes immutable hash',()=>{
  const p=policy();assert.deepEqual(validatePolicy(p),p);assert(!Object.hasOwn(validatePolicy(p),'controlPlane'));
  assert.equal(controlOrigin(p),CONTROL_ORIGIN);assert.equal(controlOrigin(validatePolicy(selected('production-v1'))),CONTROL_ORIGIN);
  const q=validatePolicy(selected('qualification-v1'));assert.equal(controlOrigin(q),QUALIFICATION_CONTROL_ORIGIN);
  assert.notEqual(hash(canonical(q)),hash(canonical(p)));assert.equal(QUALIFICATION_CONTROL_ORIGIN,'https://rudder-v4-qualification.fly.dev');
});
test('policy rejects arbitrary URL, unsupported profiles, zero receipts and credential fields',()=>{
  for(const controlPlane of [null,{},'qualification-v1',{profile:'https://attacker.invalid',qualificationReceiptSha256:'d'.repeat(64)},
    {profile:'qualification-v1',qualificationReceiptSha256:'0'.repeat(64)},{profile:'qualification-v1',qualificationReceiptSha256:'d'.repeat(64),origin:'https://attacker.invalid'},
    {profile:'qualification-v1',qualificationReceiptSha256:'d'.repeat(64),token:'FIXTURE_PRIVATE'}])assert.throws(()=>validatePolicy({...policy(),controlPlane}),{code:'invalid_request'});
});

class Store {
  authorityId=randomUUID();saved;lookups=0;started=[];
  async lookupKey(){this.lookups++;return this.saved&&structuredClone(this.saved);}
  async reserve(intent,_cipher,tokenHash){this.saved??={intent:structuredClone(intent),acks:{},phase:'pending',tokenHash};return structuredClone(this.saved);}
  async bound(){return structuredClone(this.saved);}
  async start(_i,e){this.started.push(e);}
  async originalAck(_i,e,a){this.saved.acks[e]=structuredClone(a);this.saved.phase=e==='start'?'routable':e==='configure'?'resource_provisioned':'pending';}
  async unknown(){throw Error('unexpected unknown');}async close(){}
}
class Provider {
  calls=[];configs=[];constructor(store){this.store=store;}
  describe(effect){return {effect};}
  async execute(e,i,acks,value){assert(this.store.started.includes(e));this.calls.push(e);if(e==='config_secret')this.configs.push(Buffer.from(value));
    if(e==='app')return {id:'app-original',name:i.appName,network:i.networkName,organization:i.policy.organization,networkCidr:'fd00::/64'};
    if(e==='volume')return {id:'volume-original',name:'fresh',region:'iad',sizeGb:1};
    if(e.endsWith('_secret'))return {name:e,digest:'e'.repeat(64),version:'1'};
    return {id:'machine-original',instanceId:e+'-instance',state:e==='start'?'started':'created',name:i.homeId,region:'iad',configSha256:'f'.repeat(64),config:machineConfig(i,acks.volume,e!=='machine')};
  }archiveKnownCompleted(){}
}
test('actual registry protected-config producer selects policy before effects and never rewrites old intent',async()=>{
  const db=new Store(),provider=new Provider(db),api=new ManagedHomeRegistry(db,provider,validatePolicy(policy()),Buffer.alloc(32,1));
  await api.provisionManagedHome('account','stable',['New']);const original=provider.configs[0],config=JSON.parse(original);
  assert.equal(config.cloud_origin,CONTROL_ORIGIN);assert.equal(config.projects.length,1);assert.deepEqual(provider.calls,[...EFFECTS]);
  assert(!Object.hasOwn(config,'controlPlane'));assert(!Object.hasOwn(config,'token'));assert(!Object.hasOwn(config,'registration_key'));
  const changed=new ManagedHomeRegistry(db,provider,validatePolicy(selected('qualification-v1')),Buffer.alloc(32,1));
  await assert.rejects(changed.provisionManagedHome('account','stable',['New']),{code:'conflict'});assert.equal(provider.configs.length,1);assert.deepEqual(provider.configs[0],original);
  const stageDb=new Store(),stageProvider=new Provider(stageDb),stage=new ManagedHomeRegistry(stageDb,stageProvider,validatePolicy(selected('qualification-v1')),Buffer.alloc(32,1));
  await stage.provisionManagedHome('account','new-stage',['New']);assert.equal(JSON.parse(stageProvider.configs[0]).cloud_origin,QUALIFICATION_CONTROL_ORIGIN);
  const invalidDb=new Store(),invalidProvider=new Provider(invalidDb),invalid=new ManagedHomeRegistry(invalidDb,invalidProvider,{...policy(),controlPlane:{profile:'foreign',qualificationReceiptSha256:'d'.repeat(64)}},Buffer.alloc(32,1));
  await assert.rejects(invalid.provisionManagedHome('account','invalid',['New']),{code:'invalid_request'});assert.equal(invalidDb.lookups,0);assert.equal(invalidProvider.calls.length,0);
  await api.close();await changed.close();await stage.close();await invalid.close();
});
test('registration rejects foreign key and production config hash before actual transaction',async()=>{
  const db=new Store(),provider=new Provider(db),api=new ManagedHomeRegistry(db,provider,validatePolicy(selected('qualification-v1')),Buffer.alloc(32,1));
  await api.provisionManagedHome('account','stage',['New']);const configSha=hash(provider.configs[0]),key=Buffer.alloc(32,3),home=db.saved;let queries=0;
  const registration=new RootEngineRegistration({run:async()=>{queries++;throw Error('fixture transaction unavailable');}},db.authorityId);
  const body={version:1,kind:'begin',home_id:home.intent.homeId,incarnation:home.intent.incarnation,config_sha256:configSha,root_nonce:randomUUID(),constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'999999999999'}};
  const signed=(b,k)=>({body:b,mac:createHmac('sha256',k).update('rudder_engine_registration_begin_v1\0').update(canonical(b)).digest('hex')});
  await assert.rejects(registration.begin(home,key,signed(body,Buffer.alloc(32,4)),configSha,originalTime(1000)),{code:'identity_changed'});
  const prod={...body,config_sha256:hash(canonical({...JSON.parse(provider.configs[0]),cloud_origin:CONTROL_ORIGIN}))};
  await assert.rejects(registration.begin(home,key,signed(prod,key),configSha,originalTime(1000)),{code:'identity_changed'});assert.equal(queries,0);
  await assert.rejects(registration.begin(home,key,signed(body,key),configSha,originalTime(1000)),{code:'effect_unknown'});assert.equal(queries,1);await api.close();
});
test('uploader origin derives only from matching protected config; wire stays V1 fresh/image and V2 batch',()=>{
  const fresh=scope(),manifest={kind:'blob',algorithm:'sha256',digest:'e'.repeat(64),size_bytes:1};
  const image={...fresh,mode:'image',manifest,maximum_manifest_bytes:33554432,maximum_objects:100000,maximum_bytes:2147483648,maximum_object_bytes:4194304,constraint:{...fresh.constraint,maximum_milliseconds:300000}};
  for(const s of [fresh,image,{...image,version:2,mode:'image_batch_v1'}]){assert.deepEqual(validateScope(s),s);assert.equal(uploadControlOrigin(uploadConfig(QUALIFICATION_CONTROL_ORIGIN),s),QUALIFICATION_CONTROL_ORIGIN);assert.throws(()=>validateScope({...s,cloud_origin:QUALIFICATION_CONTROL_ORIGIN}));}
  for(const patch of [{home_id:'another'},{projects:[{alias:'another'}]},{cloud_origin:'https://rudder-v4-qualification.fly.dev/'},{cloud_origin:'http://rudder-v4-qualification.fly.dev'},{cloud_origin:'https://user:pass@rudder-v4-qualification.fly.dev'},{cloud_origin:'https://rudder-v4-qualification.fly.dev.evil.invalid'}])assert.throws(()=>uploadControlOrigin({...uploadConfig(CONTROL_ORIGIN),...patch},fresh));
});
test('HTTP absolute/network-path origin escape is refused before token-bearing request',async()=>{
  for(const path of ['https://attacker.invalid/object','//attacker.invalid/object','/fixed?origin=foreign','/fixed#fragment']){let effects=0;
    await assert.rejects(httpExchange({path,method:'POST',length:0,contentType:'application/json',token:'FIXTURE_PRIVATE',check:()=>{},writer:()=>{},register:()=>{},request:()=>{effects++;throw Error();}}));assert.equal(effects,0);
  }
});
