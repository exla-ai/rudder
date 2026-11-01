// Restriction-only API mechanics with explicit model SQL. No physical fence,
// Root/native owner, typed real-pool request, reconciliation or epoch transfer.
import test from 'node:test';import assert from 'node:assert/strict';import {randomUUID} from 'node:crypto';
import {ProjectAuthority,OriginalProviderFenceRequest,assertOriginalProviderFenceRequest} from '../cloud/dist/project-authority/authority.js';
import {PROVIDER_RETIREMENT_MIGRATION_SHA256} from '../cloud/dist/project-authority/provider-retirement-schema.js';
const ctx={accountId:'fixture',principalId:'service',credentialId:'service-key',credentialSha256:'a'.repeat(64)};
function fixture({uncertain=false,overflow=false}={}){
 const authority=randomUUID(),project=randomUUID(),engine=randomUUID(),boot=randomUUID(),ownership=randomUUID();
 const binding={engineInstanceId:engine,machineBootId:boot,appName:'original-app',networkName:'original-network',machineId:'original-machine',instanceId:'original-instance',incarnation:randomUUID(),storeId:randomUUID(),configSha256:'b'.repeat(64),machineConfigSha256:'c'.repeat(64)};
 const p={authority_id:authority,account_id:ctx.accountId,project_id:project,mode:'ready',ownership_mode:'provider-fenced-v1',owner_epoch:'2',owner_engine_instance:engine,ownership_id:ownership,provider_binding:binding,provider_retirement_id:null,event_seq:'3',head_vcs_commit:'base',integration_version:'0'};
 const state={p,prior:null,events:[],uncertain,queries:0},pool={run:async(account,work)=>{assert.equal(account,ctx.accountId);const value=await work({query:async(sql,a=[])=>{
  state.queries++;state.events.push(sql);
  const row=v=>({rows:[v],rowCount:1});
  if(sql.includes('provider_retirement_migrations'))return row({authority_id:authority,sha256:PROVIDER_RETIREMENT_MIGRATION_SHA256});
  if(sql.includes('SELECT state,protocol_version'))return row({state:'ready',protocol_version:1});
  if(sql.includes('SELECT principal_id,kind,expires_at'))return row({principal_id:ctx.principalId,kind:'service',expires_at:'fixture-only'});
  if(sql.includes('timestamptz>clock_timestamp()'))return row({live:true});
  if(sql.includes('SELECT operations,scopes'))return row({operations:['owner:release','receipt:read'],scopes:['project']});
  if(sql.includes('SELECT operations FROM'))return row({operations:['owner:fence']});
  if(sql.includes('FROM rudder_project_authority.projects')&&sql.includes('FOR UPDATE'))return row(structuredClone(p));
  if(sql.startsWith('SELECT request_canonical,operation_kind,accepted_receipt'))return state.prior?row(state.prior):{rows:[],rowCount:0};
  if(sql.includes('SELECT machine_id,machine_installation_id'))return row({machine_id:binding.machineId,machine_installation_id:binding.incarnation,machine_boot_id:boot});
  if(sql.startsWith('SELECT (SELECT count'))return row({effects:overflow?'70':'0',outbox:overflow?'90':'0'});
  if(sql.startsWith('SELECT command_id')||sql.startsWith('SELECT outbox_id'))return {rows:[],rowCount:0};
  if(sql.startsWith('INSERT INTO rudder_project_authority.provider_owner_retirements')){state.retirement=a[3];return {rows:[],rowCount:1};}
  if(sql.startsWith('UPDATE rudder_project_authority.projects SET provider_retirement_id')){p.provider_retirement_id=a[3];p.mode='frozen';p.event_seq=a[4];return {rows:[],rowCount:1};}
  if(sql.startsWith('INSERT INTO rudder_project_authority.idempotency')){state.prior={request_canonical:a[8],operation_kind:a[6],accepted_receipt:JSON.parse(a[10])};return {rows:[],rowCount:1};}
  if(sql.startsWith('INSERT INTO rudder_project_authority.events')||sql.startsWith('INSERT INTO rudder_project_authority.outbox'))return {rows:[],rowCount:1};
  throw Error('unmodeled retirement SQL');
 }});if(state.uncertain){state.uncertain=false;return {status:'uncertain',reconcile:'retry_same_operation'};}return {status:'committed',value};}};
 const api=new ProjectAuthority(authority,pool),request={projectId:project,idempotencyKey:'retire-one',ownershipMode:'provider-fenced-v1',ownerEpoch:'2',ownershipId:ownership,action:'replace'};
 return {api,request,state,original:structuredClone(p)};
}
test('retirement intent rejects caller physical/reconciliation/target proof fields before SQL',async()=>{
 const f=fixture();for(const extra of [{physicalFenceObserved:true},{effectsReconciled:true},{provider:{}},{newEngineInstanceId:randomUUID()}])await assert.rejects(f.api.beginProviderOwnerRetirement(ctx,{...f.request,...extra}),{code:'invalid_request'});
 assert.equal(f.state.queries,0);
});
test('model known COMMIT freezes admission and preserves durable old owner tuple/head',async()=>{
 const f=fixture(),out=await f.api.beginProviderOwnerRetirement(ctx,f.request);assert.equal(out.status,'committed');const d=out.value.data;
 assert.equal(f.state.p.mode,'frozen');for(const k of ['owner_epoch','ownership_id','owner_engine_instance','ownership_mode','provider_binding','head_vcs_commit','integration_version'])assert.deepEqual(f.state.p[k],f.original[k]);
 assert.equal(d.scope,'provider-owner-admission-closed-only');assert.equal(d.previousProjectMode,'ready');assert.equal(d.physicalFenceObserved,false);assert.equal(d.effectsReconciled,false);assert.equal(d.ownerTransferred,false);assert.equal(d.inventory.complete,false);
 assert(f.state.events.findIndex(x=>x.startsWith('INSERT INTO rudder_project_authority.provider_owner_retirements'))<f.state.events.findIndex(x=>x.startsWith('UPDATE rudder_project_authority.projects SET provider_retirement_id')));
 // Model COMMIT DATA is deliberately insufficient for a real original request.
 assert.throws(()=>f.api.takeOriginalProviderFenceRequest(ctx,f.request.projectId,f.request.idempotencyKey),{code:'handoff_required'});
});
test('unknown original COMMIT cannot be promoted by retry or serialized receipt',async()=>{
 const f=fixture({uncertain:true});assert.equal((await f.api.beginProviderOwnerRetirement(ctx,f.request)).status,'uncertain');assert.equal(f.state.p.mode,'frozen');const queries=f.state.queries;
 await assert.rejects(f.api.beginProviderOwnerRetirement(ctx,f.request),{code:'handoff_required'});assert.equal(f.state.queries,queries);
 assert.throws(()=>f.api.takeOriginalProviderFenceRequest(ctx,f.request.projectId,f.request.idempotencyKey),{code:'handoff_required'});assert.throws(()=>assertOriginalProviderFenceRequest(structuredClone(f.state.prior.accepted_receipt)),{code:'forbidden'});
});
test('overflow inventory stays explicitly incomplete without freeing ownership',async()=>{
 const f=fixture({overflow:true}),r=await f.api.beginProviderOwnerRetirement(ctx,f.request);assert.equal(r.value.data.inventory.overflow,true);assert.equal(r.value.data.inventory.effectCount,'70');assert.equal(r.value.data.inventory.outboxCount,'90');assert.equal(r.value.data.inventory.requiresNativeJournalReconciliation,true);assert.equal(f.state.p.owner_epoch,'2');
});
test('JSON/prototype/symbol constructors cannot create original physical-fence requests',()=>{
 for(const data of [{},{physicalFenceObserved:true},Object.create(OriginalProviderFenceRequest.prototype)])assert.throws(()=>assertOriginalProviderFenceRequest(data),{code:'forbidden'});
 assert.throws(()=>OriginalProviderFenceRequest.issue(Symbol('caller'),{},()=>{},()=>{},()=>{}),{code:'forbidden'});
});
