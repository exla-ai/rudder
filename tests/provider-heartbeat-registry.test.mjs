// Genuine production registry/worker issuance -> RootEngineOperations HMAC ->
// provider-heartbeat handler regression, with scripted PG/provider transports.
// TEST-ONLY keys/rows. No real PG/Fly/Root/Native/owner grant or lease is issued.
// The scoped storage handler deliberately refuses: this tests admission/routing,
// not a fabricated successful provider owner. Definitions UNRUN by author.
import test from 'node:test';import assert from 'node:assert/strict';
import {createCipheriv,createHmac,randomUUID} from 'node:crypto';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {RootEngineOperations} from '../cloud/dist/managed-home-provisioning/operations.js';
import {RootEngineRegistration} from '../cloud/dist/managed-home-provisioning/registration.js';
import {OPERATIONS_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/operations-schema.js';
import {PROVIDER_OWNERSHIP_MIGRATION_SHA256} from '../cloud/dist/project-authority/provider-ownership-schema.js';
import {canonical,hash,validatePolicy,ManagedHomeError} from '../cloud/dist/managed-home-provisioning/types.js';
const sign=(key,domain,body)=>createHmac('sha256',key).update(domain).update(canonical(body,24*1024)).digest('hex');
function fixture(){
 const authority=randomUUID(),project=randomUUID(),engine=randomUUID(),boot=randomUUID(),credentialId=randomUUID(),nonce=randomUUID();
 const privateRegistryKey=Buffer.alloc(32,17),testRootKey=Buffer.alloc(32,23),testBearer='rdrmh_'+'6'.repeat(64);
 const policy=validatePolicy({ownerMode:'provider-fenced-v1',organization:'fixture',region:'iad',image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),runtimeManifest:{profile_id:'fly-managed-image-v1',manifest_sha256:'2'.repeat(64),entries:[{path:'/opt/rudder-managed-runtime/v1/test',sha256:'3'.repeat(64),size_bytes:1}]},runtimeImageReceiptSha256:'4'.repeat(64),providerReceiptSha256:'5'.repeat(64),durabilityReceiptSha256:'6'.repeat(64),freshVolumeReceiptSha256:'7'.repeat(64),nativeSha256:'8'.repeat(64),launcherSha256:'9'.repeat(64),serviceSha256:'a'.repeat(64),nodeSha256:'b'.repeat(64),sidecarPolicySha256:'c'.repeat(64),volumeGb:1,cpus:1,memoryMb:512,userName:'Fixture',userEmail:'fixture@example.invalid'});
 const intent={authorityId:authority,accountId:'fixture-account',homeId:'mh-'+randomUUID(),commandKey:'original-create',requestHash:'a'.repeat(64),epoch:'1',nonce,appName:'fixture-app',networkName:'fixture-isolated-network',storeId:randomUUID(),incarnation:randomUUID(),processOwner:randomUUID(),policy,spec:{projects:[{alias:'workspace',prepare:{namespace:{project_id:project}}}]},freshProjects:[]};
 const machine={id:'original-machine',instanceId:'original-instance',configSha256:'b'.repeat(64)};
 const original={intent,phase:'routable',tokenHash:hash(testBearer),acks:{app:{id:'original-app'},volume:{id:'original-volume'},configure:machine,start:machine}};
 let bound=structuredClone(original);const counters={fly:0,auth:0,started:0,handler:0,saved:0,bound:0,cipher:0};let saved;
 const challenge={challenge_id:randomUUID(),engine_instance_id:engine,credential_id:credentialId,root_nonce:randomUUID()};
 const credential=Buffer.from('rdre_'+sign(testRootKey,'rudder_engine_registration_credential_v1\0',challenge));
 const registration={challenge_json:{body:challenge},proof_sha256:'d'.repeat(64),principal_id:'fixture-engine',token_sha256:hash(credential),credential_id:credentialId,engine_instance_id:engine,machine_boot_id:boot};
 const pool={run:async(account,body)=>{assert.equal(account,intent.accountId);const value=await body({query:async(sql,values)=>{
  if(sql.includes('managed_home_operations_migrations'))return {rows:[{sha256:OPERATIONS_MIGRATION_SHA256}]};
  if(sql.includes('SELECT r.challenge_json')){counters.auth++;return {rows:[structuredClone(registration)]};}
  if(sql.includes('provider_ownership_migrations'))return {rows:[{authority_id:authority,sha256:PROVIDER_OWNERSHIP_MIGRATION_SHA256}]};
  if(sql.includes('SELECT request_sha256'))return {rows:saved?[{request_sha256:saved.body.request_sha256,state:'replied',ack_json:saved}]:[]};
  if(sql.includes('SELECT 1 FROM')&&sql.includes('.projects'))return {rows:[{one:1}]};
  if(sql.includes('INSERT INTO')&&sql.includes('managed_provider_owner_heartbeats')){counters.started++;return {rowCount:1,rows:[]};}
  if(sql.includes("SET state='replied'")){saved=JSON.parse(values[4]);counters.saved++;return {rowCount:1,rows:[{request_id:values[3]}]};}
  throw Error('Unexpected fixture SQL boundary');
 }});return {status:'committed',value};}};
 const storage={heartbeatOriginalProviderOwner:async(home,ctx,op)=>{counters.handler++;assert.equal(home.intent.nonce,intent.nonce);assert.equal(ctx.accountId,intent.accountId);assert.equal(ctx.engine.instanceId,engine);assert.equal(ctx.engine.bootId,boot);assert.equal(ctx.engine.credentialId,credentialId);assert.equal(ctx.engine.credentialSha256,hash(credential));assert.equal(op.op,'HeartbeatProviderOwnerV1');throw new ManagedHomeError('unconfigured');}};
 const db={worker:async home=>{assert.equal(home,intent.homeId);return structuredClone(original);},bound:async(account,home)=>{assert.equal(account,intent.accountId);assert.equal(home,intent.homeId);counters.bound++;return structuredClone(bound);},registrationCipher:async i=>{assert.equal(i.nonce,intent.nonce);counters.cipher++;const iv=Buffer.alloc(12,7),cipher=createCipheriv('aes-256-gcm',privateRegistryKey,iv);cipher.setAAD(canonical({account:i.accountId,home:i.homeId,nonce:i.nonce,purpose:'registration'}));return Buffer.concat([iv,cipher.update(testRootKey.toString('hex')+'\n'),cipher.final(),cipher.getAuthTag()]);}};
 const provider={observeApp:async()=>{counters.fly++;throw Error('Heartbeat must not read Fly');},observeMachine:async()=>{counters.fly++;throw Error('Heartbeat must not read Fly');}};
 const operations=new RootEngineOperations(pool,authority,db,storage);
 const registry=new ManagedHomeRegistry(db,provider,policy,privateRegistryKey,new RootEngineRegistration(pool,authority),operations);
 const operation={op:'HeartbeatProviderOwnerV1',workspace_id:'workspace',owner_epoch:'2',ownership_id:randomUUID(),request_id:randomUUID(),expected_serial:'0'};
 const packet=op=>{const body={version:1,kind:'engine_operation',account_id:intent.accountId,home_id:intent.homeId,store_id:intent.storeId,incarnation:intent.incarnation,engine_instance_id:engine,boot_id:boot,credential_id:credentialId,epoch:'1',root_nonce:challenge.root_nonce,registration_proof_sha256:registration.proof_sha256,request_nonce:randomUUID(),command_key:'provider-hb:'+op.request_id,operation:op,payload_sha256:hash(canonical(op)),constraint:{format_version:1,maximum_milliseconds:2000,boottime_deadline_nanoseconds:'999999999999999999'},remaining_milliseconds:2000};return {body,mac:sign(credential,'rudder_engine_operation_request_v1\0',body)};};
 return {registry,counters,operation,packet,credential,intent,original,registration,
  worker:()=>registry.authenticateManagedHomeWorker(intent.homeId,testBearer),
  change:mutate=>mutate(bound)};
}
test('actual registry-issued worker and Root HMAC reach provider handler without live Fly observations',async()=>{
 const f=fixture(),worker=await f.worker(),packet=f.packet(f.operation);const result=await f.registry.performOriginalEngineOperation(worker,packet);
 assert.equal(result.body.error,'unconfigured');assert.equal(result.body.result,null,'no fabricated successful owner is returned');
 assert.equal(result.body.request_sha256,hash(canonical(packet.body)));assert.deepEqual(result.body.constraint,packet.body.constraint);
 assert.equal(result.mac,sign(f.credential,'rudder_engine_operation_ack_v1\0',result.body));
 assert.equal(f.counters.fly,0);assert.equal(f.counters.auth,1);assert.equal(f.counters.started,1);assert.equal(f.counters.handler,1);assert.equal(f.counters.saved,1);
});
test('committed heartbeat route preserves invalid-MAC and original-worker refusal',async()=>{
 const f=fixture(),worker=await f.worker(),bad={...f.packet(f.operation),mac:'f'.repeat(64)};
 await assert.rejects(f.registry.performOriginalEngineOperation(worker,bad));assert.equal(f.counters.fly,0);assert.equal(f.counters.handler,0);assert.equal(f.counters.started,0);assert.equal(f.counters.auth,1);
 await assert.rejects(f.registry.performOriginalEngineOperation({},f.packet(f.operation)),{code:'identity_changed'});assert.equal(f.counters.auth,1);
});
test('changed committed nonce or configured original instance refuses before Root dispatch',async()=>{
 for(const change of [b=>b.intent.nonce=randomUUID(),b=>b.tokenHash='0'.repeat(64),b=>b.acks.configure.instanceId='replacement']){
  const f=fixture(),worker=await f.worker();f.change(change);await assert.rejects(f.registry.performOriginalEngineOperation(worker,f.packet(f.operation)),{code:'identity_changed'});
  assert.equal(f.counters.fly,0);assert.equal(f.counters.auth,0);assert.equal(f.counters.cipher,0);assert.equal(f.counters.handler,0);
 }
});
test('existing clock-renew routing still avoids Fly and other metadata operations keep live provider checks',async()=>{
 const f=fixture(),worker=await f.worker(),renew={op:'RenewFreshOwnerV1',workspace_id:'workspace',owner_epoch:'2',lease_id:randomUUID(),request_id:randomUUID(),expected_serial:'0'};
 await assert.rejects(f.registry.performOriginalEngineOperation(worker,{...f.packet(renew),mac:'f'.repeat(64)}));assert.equal(f.counters.auth,1);assert.equal(f.counters.fly,0);
 const before=f.counters.auth;const status=f.packet({op:'FreshProjectStatusV1',workspace_id:'workspace'});await assert.rejects(f.registry.performOriginalEngineOperation(worker,status));
 assert.equal(f.counters.fly,1);assert.equal(f.counters.auth,before,'ordinary metadata operation must not bypass live provider checks');
});
