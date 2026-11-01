// Actual registry + provider lifecycle; EXPLICIT model DB/HTTP and published
// immutable-role admission. No PG/Fly/Root/native/Source/grant is fabricated.
// All execution UNRUN in the private source proposal.
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {FlyManagedHomeProvider,FlyHttpTransport} from '../cloud/dist/managed-home-provisioning/fly.js';
import {RootRuntimeImageQualifications} from '../cloud/dist/managed-home-provisioning/runtime-qualification.js';
import {RootCompilerSdkQualifications} from '../cloud/dist/managed-home-provisioning/compiler-qualification.js';
import {A2_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
import {validatePolicy,canonical,EFFECTS} from '../cloud/dist/managed-home-provisioning/types.js';
import {flyFetchFixture} from './managed-storage-pg-fixtures.mjs';
const linux={skip:process.platform!=='linux',timeout:20000}; // actual Fly lease clock reads procfs
const ref=(kind,digest,size_bytes)=>({kind,algorithm:'sha256',digest:digest.repeat(64),size_bytes});
function policy(){const image='registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),installer=ref('directory','d',230),inventory=ref('blob','e',1024);
 return {organization:'test-org',region:'iad',image,runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:'2'.repeat(64),entries:[{path:'/opt/rudder-managed-runtime/v1/test',sha256:'3'.repeat(64),size_bytes:1}]},runtimeImageReceiptSha256:'4'.repeat(64),providerReceiptSha256:'5'.repeat(64),durabilityReceiptSha256:'6'.repeat(64),freshVolumeReceiptSha256:'7'.repeat(64),nativeSha256:'8'.repeat(64),launcherSha256:'9'.repeat(64),serviceSha256:'a'.repeat(64),nodeSha256:'b'.repeat(64),sidecarPolicySha256:'c'.repeat(64),volumeGb:1,cpus:1,memoryMb:512,userName:'Fixture',userEmail:'fixture@example.invalid',
  controlPlane:{profile:'qualification-v1',qualificationReceiptSha256:'f'.repeat(64)},ownerMode:'provider-fenced-v1',sdkInitialSource:'sdk-fixed-suite-v1',compilerSdk:structuredClone(A2_COMPILER_SDK),
  imagePreseed:{fullManifestSha256:'6'.repeat(64),installerInput:installer,runtimeMount:{profile:'baked-cas-v1',inventory,image,imageReceiptSha256:'4'.repeat(64),fullManifestSha256:'6'.repeat(64),installerInput:installer}}};}
class Ledger {
 authorityId=randomUUID();rows=new Map();events=[];lookups=0;
 key(a,k){return a+'\0'+k;}
 async lookupKey(a,k){this.lookups++;return structuredClone(this.rows.get(this.key(a,k)));}
 row(i){const row=this.rows.get(this.key(i.accountId,i.commandKey));assert(row);return row;}
 async reserve(i,cipher,tokenHash,registrationCipher){assert(cipher.length>28&&registrationCipher.length>28);const key=this.key(i.accountId,i.commandKey);
  if(!this.rows.has(key))this.rows.set(key,{intent:structuredClone(i),acks:{},phase:'pending',tokenHash});return structuredClone(this.rows.get(key));}
 async bound(account,home){const r=[...this.rows.values()].find(r=>r.intent.accountId===account&&r.intent.homeId===home);assert(r);return structuredClone(r);}
 async start(i,e,descriptor){assert(descriptor);assert.equal(this.row(i).intent.nonce,i.nonce);this.events.push('started:'+e);}
 async originalAck(i,e,ack){const row=this.row(i);row.acks[e]=structuredClone(ack);row.phase=e==='start'?'routable':e==='configure'?'resource_provisioned':'pending';this.events.push('ack:'+e);}
 async unknown(i,e){this.events.push('unknown:'+e);if(!this.row(i).acks[e])this.row(i).phase='effect_unknown';}
 async close(){this.closed=true;}
}
function setup(t,change=()=>{}){
 const p=policy();change(p);validatePolicy(p);const db=new Ledger(),http=flyFetchFixture(),previous=globalThis.fetch;globalThis.fetch=http.fetch;t.after(()=>{globalThis.fetch=previous;});
 const provider=new FlyManagedHomeProvider(new FlyHttpTransport('synthetic-managed-pg-provider-only')),archived=[];const archive=provider.archiveKnownCompleted.bind(provider);
 provider.archiveKnownCompleted=i=>{archived.push(i.nonce);return archive(i);};
 const gates={runtime:0,sdk:0},unusedPool={run:async()=>{throw Error('published immutable-role DATA gate is explicitly modeled in this lifecycle unit');}};
 // Genuine service instances keep the production service brand. ONLY their
 // durable public immutable-role admission is modeled; no original Root ACK,
 // publisher purpose, Native client/floor, engine or Source owner is minted.
 const runtime=new RootRuntimeImageQualifications(unusedPool,unusedPool,db.authorityId,p),sdk=new RootCompilerSdkQualifications(unusedPool,unusedPool,db.authorityId,p);
 runtime.assertPublishedRole=async(actual,original)=>{assert(canonical(actual).equals(canonical(p)));original?.remaining();gates.runtime++;};
 sdk.assertPublishedRole=async(actual,original)=>{assert(canonical(actual).equals(canonical(p)));original?.remaining();gates.sdk++;};
 const create=()=>new ManagedHomeRegistry(db,provider,p,Buffer.alloc(32,1),undefined,undefined,undefined,undefined,runtime,undefined,sdk);
 const api=create();return {p,db,http,provider,archived,gates,api,create};
}
test('actual provision completion retains exact staging original and returns only redacted custody',linux,async t=>{
 const f=setup(t),receipt=await f.api.provisionManagedHome('fixture-account','stable',['Fresh']);assert.equal(receipt.phase,'routable');assert.equal(receipt.nativeIngressReady,false);assert.equal(receipt.taskReady,false);
 assert.deepEqual(f.db.events.filter(e=>e.startsWith('ack:')),EFFECTS.map(e=>'ack:'+e));assert.equal(f.archived.length,0);assert(f.gates.runtime>0&&f.gates.sdk>0);
 const data=await f.api.stagingOrdinaryProvisionCustodyData('fixture-account','stable'),row=[...f.db.rows.values()][0];
 assert.equal(data.home_id,receipt.homeId);assert.equal(data.provision_nonce,row.intent.nonce);assert.equal(data.process_owner,row.intent.processOwner);
 assert.equal(data.app.id,row.acks.app.id);assert.equal(data.volume.id,row.acks.volume.id);assert.equal(data.started.id,row.acks.start.id);assert.equal(data.started.instance_id,row.acks.start.instanceId);
 assert.equal(data.teardown_authority,false);assert.equal(data.owners_retained,true);const text=JSON.stringify(data),home=f.http.forHome(receipt.homeId);assert(home);
 for(const name of ['RUDDER_HOST_CONFIG_V1','RUDDER_HOST_TOKEN_V1','RUDDER_ROOT_REGISTRATION_V1']){const secret=home.secrets.get(name);assert(secret);assert(!text.includes(secret.trim()));}
 assert(!text.includes('rdrmh_')&&!text.includes('"config":')&&!text.includes('registrationKey')&&!text.includes('cipher'));
 const creates=f.http.counts.app;assert.deepEqual(await f.api.provisionManagedHome('fixture-account','stable',['Fresh']),receipt);assert.equal(f.http.counts.app,creates);assert.equal(f.archived.length,0);
 await assert.rejects(f.api.stagingOrdinaryProvisionCustodyData('foreign-account','stable'),{code:'not_found'});
 await assert.rejects(f.api.stagingOrdinaryProvisionCustodyData('fixture-account','foreign-key'),{code:'not_found'});
 // A second registry can read the completed receipt but owns no original.
 const reopened=f.create();assert.deepEqual(await reopened.provisionManagedHome('fixture-account','stable',['Fresh']),receipt);
 await assert.rejects(reopened.stagingOrdinaryProvisionCustodyData('fixture-account','stable'),{code:'not_found'});
 await assert.rejects(f.api.close(),{code:'effect_unknown'});
});
for(const [name,change]of [['default production origin',p=>{delete p.controlPlane;}],['no fixed source suite',p=>{delete p.sdkInitialSource;}],['no provider-fenced mode',p=>{delete p.ownerMode;}]]){
 test('ordinary '+name+' preserves actual archive and refuses staging getter',linux,async t=>{
  const f=setup(t,change);const receipt=await f.api.provisionManagedHome('fixture-account','stable',['Fresh']);assert.equal(receipt.phase,'routable');assert.equal(f.archived.length,1);
  const creates=f.http.counts.app;assert.deepEqual(await f.api.provisionManagedHome('fixture-account','stable',['Fresh']),receipt);assert.equal(f.http.counts.app,creates);
  await assert.rejects(f.api.stagingOrdinaryProvisionCustodyData('fixture-account','stable'),{code:'not_found'});await f.api.close();assert.equal(f.db.closed,true);
 });
}
test('partial ordinary original in unqualified context is refused BEFORE row lookup',linux,async t=>{
 const f=setup(t,p=>{delete p.ownerMode;});globalThis.fetch=async(url,options)=>{if(options.method==='POST'&&new URL(url).pathname.endsWith('/machines'))throw Error('explicit fixture unknown original machine create');return f.http.fetch(url,options);};
 await assert.rejects(f.api.provisionManagedHome('fixture-account','partial',['Fresh']),{code:'effect_unknown'});assert.equal(f.archived.length,0);
 const lookups=f.db.lookups;await assert.rejects(f.api.stagingOrdinaryProvisionCustodyData('fixture-account','partial'),{code:'not_found'});assert.equal(f.db.lookups,lookups);
});
test('foreign durable intent cannot replace the held original behind a matching key',linux,async t=>{
 const f=setup(t);await f.api.provisionManagedHome('fixture-account','stable',['Fresh']);const row=[...f.db.rows.values()][0];row.intent.nonce=randomUUID();
 await assert.rejects(f.api.stagingOrdinaryProvisionCustodyData('fixture-account','stable'),{code:'identity_changed'});assert.equal(f.archived.length,0);
});
test('completed staging originals remain charged to the EXISTING16 carrier bound',linux,async t=>{
 const f=setup(t);for(let n=0;n<16;n++)await f.api.provisionManagedHome('fixture-account','bounded-'+n,['Fresh']);assert.equal(f.archived.length,0);assert.equal(f.http.counts.app,16);
 await assert.rejects(f.api.provisionManagedHome('fixture-account','seventeenth',['Fresh']),{code:'busy'});assert.equal(f.http.counts.app,16);assert.equal(f.db.rows.size,16);
 assert((await f.api.stagingOrdinaryProvisionCustodyData('fixture-account','bounded-0')).started);assert((await f.api.stagingOrdinaryProvisionCustodyData('fixture-account','bounded-15')).started);
});
test('qualified partial unknown retains original app/volume and never claims machine creation',linux,async t=>{
 const f=setup(t);globalThis.fetch=async(url,options)=>{if(options.method==='POST'&&new URL(url).pathname.endsWith('/machines'))throw Error('explicit fixture unknown original machine create');return f.http.fetch(url,options);};
 await assert.rejects(f.api.provisionManagedHome('fixture-account','partial',['Fresh']),{code:'effect_unknown'});
 const data=await f.api.stagingOrdinaryProvisionCustodyData('fixture-account','partial');assert(data.app&&data.volume);assert.equal(data.machine,null);assert.equal(data.started,null);
 assert.deepEqual(data.pending_effect,{effect:'machine',failed:true,original_ack_observed:false});assert.equal(data.teardown_authority,false);assert.equal(f.archived.length,0);
});
