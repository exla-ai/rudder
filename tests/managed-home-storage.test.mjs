// Author tests UNRUN. Synthetic PG/transport rows never qualify physical PG/S3,
// image retention, engine registration, leases or runtime authority.
import test from 'node:test';import assert from 'node:assert/strict';import fs from 'node:fs/promises';import {createHmac,randomUUID} from 'node:crypto';
import {canonical,hash,validateSpec} from '../cloud/dist/managed-home-provisioning/types.js';
import {reserveFreshProjects,freshSpec} from '../cloud/dist/managed-home-provisioning/fresh.js';
import {validateStorageOperation,storageReference} from '../cloud/dist/managed-home-provisioning/storage-protocol.js';
import {RootEngineOperations} from '../cloud/dist/managed-home-provisioning/operations.js';
import {OPERATIONS_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/operations-schema.js';
import {originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {ManagedHomeRegistry} from '../cloud/dist/managed-home-provisioning/registry.js';
import {handleManagedStorageHttp} from '../cloud/dist/managed-home-provisioning/storage-http.js';
const ref=kind=>({kind,algorithm:'sha256',digest:'a'.repeat(64),size_bytes:10});
const begin=()=>({op:'BeginFreshRootsV1',workspace_id:'mp-fresh',prepare_id:randomUUID(),export_id:randomUUID(),head:'a'.repeat(40),source:ref('source_root'),environment:ref('environment_root')});
test('fresh homes explicitly share the machine-owned CAS without changing project UUIDs',()=>{
 const projects=reserveFreshProjects(['First','Second']),store=randomUUID();const spec=freshSpec(projects,store,{userName:'fixture',userEmail:'fixture@example.invalid'});
 assert.equal(spec.projects[0].cas,spec.stateDirectory+'/cas');assert.equal(spec.projects[1].cas,spec.projects[0].cas);assert.notEqual(spec.projects[0].prepare.namespace.project_id,spec.projects[1].prepare.namespace.project_id);
 const old=structuredClone(spec);old.projects[1].cas=old.projects[1].home+'/cas';assert.throws(()=>validateSpec(old),{code:'identity_changed'});
});
test('storage protocol rejects path, readiness, wrong root kind and rounded byte authority',()=>{
 assert.deepEqual(validateStorageOperation(begin()).op,'BeginFreshRootsV1');
 for(const change of [v=>v.source=ref('blob'),v=>v.environment=ref('source_root'),v=>v.retained=true,v=>v.source_path='/private/source',v=>v.prepare_id='old',v=>v.head='not-history']){const b=begin();change(b);assert.throws(()=>validateStorageOperation(b));}
 assert.throws(()=>storageReference({...ref('blob'),size_bytes:9007199254740993}));assert.throws(()=>storageReference({...ref('blob'),algorithm:'sha512'}));
});
test('shared native/cloud vectors use actual Cloud canonical and HMAC with wide IDs preserved',async()=>{
 const v=JSON.parse(await fs.readFile(new URL('./fixtures/managed-engine-hmac-vectors.json',import.meta.url),'utf8'));assert.equal(v.scope,'SYNTHETIC_TEST_DATA_ONLY_NO_AUTHORITY');assert.equal(v.vectors.length,7);
 for(const x of v.vectors){const bytes=canonical(x.body);assert.equal(bytes.toString(),x.canonical_utf8);assert.equal(hash(bytes),x.body_sha256);assert.equal(createHmac('sha256',Buffer.from(x.key_hex,'hex')).update(x.domain).update(bytes).digest('hex'),x.mac_hex);assert(x.domain.endsWith('\0'));}
 const credential=v.vectors.find(x=>x.phase==='credential');assert.equal(hash('rdre_'+credential.mac_hex),v.derived_engine_credential_sha256);
});
function fixture(operation=begin(),max=30000){const root=Buffer.alloc(32,7),engine=randomUUID(),credential=randomUUID(),nonce=randomUUID(),boot=randomUUID();const i={accountId:'account',homeId:'mh-original',storeId:randomUUID(),incarnation:randomUUID(),nonce:randomUUID()};
 const challenge={challenge_id:randomUUID(),engine_instance_id:engine,credential_id:credential,root_nonce:nonce};const sign=(key,domain,body)=>createHmac('sha256',key).update(domain).update(canonical(body)).digest('hex');const secret=Buffer.from('rdre_'+sign(root,'rudder_engine_registration_credential_v1\0',challenge));
 const body={version:1,kind:'engine_operation',account_id:i.accountId,home_id:i.homeId,store_id:i.storeId,incarnation:i.incarnation,engine_instance_id:engine,boot_id:boot,credential_id:credential,epoch:'1',root_nonce:nonce,registration_proof_sha256:'a'.repeat(64),request_nonce:randomUUID(),command_key:'original',operation,payload_sha256:hash(canonical(operation)),constraint:{format_version:1,maximum_milliseconds:max,boottime_deadline_nanoseconds:'999999999999'},remaining_milliseconds:max};
 const row={challenge_json:{body:challenge},proof_sha256:body.registration_proof_sha256,principal_id:'engine-principal',token_sha256:hash(secret),credential_id:credential,engine_instance_id:engine,machine_boot_id:boot};return {root,secret,row,home:{intent:i},packet:{body,mac:sign(secret,'rudder_engine_operation_request_v1\0',body)}};
}
function fakePool(f,{unknownStart=false}={}){let auth=true,ack;return {run:async(account,body)=>{if(!auth&&unknownStart)return {status:'uncertain'};const tx={query:async(sql,values)=>{
 if(sql.includes('managed_home_operations_migrations'))return {rows:[{sha256:OPERATIONS_MIGRATION_SHA256}]};if(sql.includes('FROM')&&sql.includes('managed_home_registrations')){auth=false;return {rows:[f.row]};}
 if(sql.includes('SELECT nonce'))return {rows:[{nonce:f.home.intent.nonce}]};if(sql.includes('SELECT request_json'))return {rows:ack?[{payload_sha256:f.packet.body.payload_sha256,engine_instance_id:f.packet.body.engine_instance_id,state:'replied',ack_json:ack}]:[]};if(sql.includes('count(*)'))return {rows:[{n:'0'}]};if(sql.includes('SET state=\'replied\''))ack=JSON.parse(values[4]);return {rows:[],rowCount:1};}};return {status:'committed',value:await body(tx)};}};}
test('forged Root storage MAC and ambiguous Started commit never call S3/service producer',async()=>{for(const uncertain of [false,true]){const f=fixture();let effects=0;const storage={perform:async()=>{effects++;return {};},completeStoredOperation:()=>{}};const api=new RootEngineOperations(fakePool(f,{unknownStart:uncertain}),randomUUID(),{},storage);const packet=uncertain?f.packet:{...f.packet,mac:'f'.repeat(64)};await assert.rejects(api.perform(f.home,f.root,packet,originalTime(1000)));assert.equal(effects,0);}});
test('only distinct image operations admit the larger original import budget',async()=>{const f=fixture(begin(),300000);let effects=0;const api=new RootEngineOperations(fakePool(f),randomUUID(),{}, {perform:async()=>{effects++;return {};}});await assert.rejects(api.perform(f.home,f.root,f.packet,originalTime(1000)));assert.equal(effects,0);
 const image={op:'BeginImagePreseedV1',workspace_id:'mp-fresh',manifest:ref('blob'),installer_input:ref('directory'),image_receipt_sha256:'b'.repeat(64)};assert.equal(validateStorageOperation(image).op,'BeginImagePreseedV1');});
test('actual HMAC verifier passes original engine context internally and saves one immutable ACK',async()=>{const f=fixture();let effects=0,completed=0;const storage={perform:async(home,context,request,time)=>{effects++;assert.equal(context.credentialSha256,hash(f.secret));assert.equal(context.engine.instanceId,f.packet.body.engine_instance_id);assert.equal(request.request_nonce,f.packet.body.request_nonce);time.remaining();return {scope:'fresh-root-transfer-only',state:'uploading',transfer_id:randomUUID()};},completeStoredOperation:()=>completed++};const api=new RootEngineOperations(fakePool(f),randomUUID(),{},storage);const ack=await api.perform(f.home,f.root,f.packet,originalTime(1000));assert.equal(ack.body.error,null);assert.equal(ack.body.request_sha256,hash(canonical(f.packet.body)));assert.equal(effects,1);assert.equal(completed,1);assert.deepEqual(await api.perform(f.home,f.root,f.packet,originalTime(1000)),ack);assert.equal(effects,1);});
test('streaming path cannot create a transfer from a bearer, URL or JSON context',async()=>{
 const registry=new ManagedHomeRegistry(),id=randomUUID();const req={method:'PUT',headers:{authorization:'Bearer rdrmh_'+'a'.repeat(64),'content-type':'application/octet-stream','content-length':'10'}};let output;const res={destroyed:false,headersSent:false,writeHead:()=>{},end:b=>output=JSON.parse(b)};
 assert.equal(await handleManagedStorageHttp(req,res,new URL('https://rudder-cloud-control.fly.dev/api/rudder/home/mh/fresh-roots/'+id+'/objects/blob/'+'a'.repeat(64)+'/10'),registry),true);assert.deepEqual(output,{code:'unconfigured'});
});
