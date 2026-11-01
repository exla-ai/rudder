// Synthetic byte/HTTP-retention component definitions only. UNRUN here.
// No provider, Root/namespace, startup, role or token authority is fabricated.
import test from 'node:test';
import assert from 'node:assert/strict';
import {inspect} from 'node:util';
import {randomUUID} from 'node:crypto';
import {FlyHttpTransport,FlyManagedHomeProvider,machineConfig,machineAck,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {canonical,hash,CONFIG_SECRET,TOKEN_SECRET,REGISTRATION_SECRET} from '../cloud/dist/managed-home-provisioning/types.js';
const apiToken='TEST_ONLY_private_transport_credential_never_real';
function fixture(){
 const i={nonce:randomUUID(),homeId:'fixture-home',storeId:randomUUID(),appName:'rudder-secret-file-encoding',networkName:'fixture-network',policy:{organization:'fixture-org',region:'sjc',image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),cpus:1,memoryMb:512,volumeGb:1},runtimeQualification:{fixture_data_only:true}};
 const app={id:'app-original',name:i.appName,network:i.networkName,network_cidr:'fdaa:1::/48',organization:{slug:i.policy.organization}},volume={id:'vol_original',name:'rudder_'+i.nonce.replaceAll('-','').slice(0,23),region:i.policy.region,sizeGb:1};
 const config=machineConfig(i,volume,false),machine={id:'machine-original',instance_id:'instance-original',name:i.homeId,region:i.policy.region,state:'stopped',host_status:'ok',config};
 const acks={app:{id:app.id,name:app.name,network:app.network,networkCidr:app.network_cidr,organization:i.policy.organization},volume,machine:machineAck(machine,i,config,volume)};
 const http=new FlyHttpTransport(apiToken,128),provider=new FlyManagedHomeProvider(http);return {i,app,machine,acks,http,provider};
}
const examples=[
 {effect:'config_secret',name:CONFIG_SECRET,raw:JSON.stringify({version:1,fixture_data_only:true,user_name:'Fixture café 学習',nested:{text:'keep\nthis'}})+'\n'},
 {effect:'token_secret',name:TOKEN_SECRET,raw:'rdrmh_'+'1'.repeat(64)},
 {effect:'registration_secret',name:REGISTRATION_SECRET,raw:'d'.repeat(64)+'\n'},
];
function privateSummary(data,...values){const raw=typeof data==='string'?data:JSON.stringify(data);for(const v of [apiToken,...values])assert(!raw.includes(v),'private source or encoded wire bytes must not enter recovery/inspection DATA');}

test('all three secret-file bodies decode to exact original UTF8 bytes and match durable describe SHA',async t=>{
 const previous=globalThis.fetch;t.after(()=>{globalThis.fetch=previous;});
 for(const example of examples){
  const f=fixture(),planned=f.provider.describe(example.effect,f.i,f.acks,example.raw),wireValue=Buffer.from(example.raw,'utf8').toString('base64');let posts=0;
  globalThis.fetch=async(url,init)=>{
   const path=new URL(String(url)).pathname;
   if(init.method==='GET'&&path==='/v1/apps/'+f.i.appName)return Response.json(f.app);
   if(init.method==='GET'&&path==='/v1/apps/'+f.i.appName+'/machines/'+f.machine.id)return Response.json(f.machine);
   assert.equal(init.method,'POST');assert.equal(path,planned.path);posts++;
   assert.equal(init.headers.Authorization,'Bearer '+apiToken);const body=JSON.parse(init.body);
   assert.deepEqual(Object.keys(body),['value']);assert.equal(body.value,wireValue);assert.notEqual(body.value,example.raw);
   assert.deepEqual(Buffer.from(body.value,'base64'),Buffer.from(example.raw,'utf8'),'provider-decoded file bytes must exactly preserve Unicode/newlines/key text');
   assert.equal(Buffer.from(body.value,'base64').toString('base64'),body.value,'canonical standard base64');
   assert.equal(hash(Buffer.from(init.body)),planned.body_sha256,'describe must hash the exact HTTP bytes');
   assert.equal(hash(canonical(body)),planned.body_sha256);
   return Response.json({name:example.name,digest:'a'.repeat(16),version:7,value:wireValue},{status:201});
  };
  const ack=await f.provider.execute(example.effect,f.i,f.acks,example.raw,originalTime(2000));
  assert.deepEqual(ack,{name:example.name,digest:'a'.repeat(16),version:'7'});assert.equal(posts,1);
  privateSummary(planned,example.raw,wireValue);privateSummary(ack,example.raw,wireValue);
  privateSummary(f.provider.originalDisposableResponseData(f.i),example.raw,wireValue);privateSummary(inspect(f.provider),example.raw,wireValue);privateSummary(inspect(f.http),example.raw,wireValue);
 }
});
test('base64-looking registration text is encoded once as file bytes, never treated as pre-encoded contents',()=>{
 const f=fixture(),raw='d'.repeat(64)+'\n',planned=f.provider.describe('registration_secret',f.i,f.acks,raw);
 assert.equal(Buffer.byteLength(raw),65);assert.equal(planned.body_sha256,hash(canonical({value:Buffer.from(raw,'utf8').toString('base64')})));
 assert.notEqual(planned.body_sha256,hash(canonical({value:raw})));privateSummary(planned,raw,Buffer.from(raw).toString('base64'));
});
test('empty or missing private input cannot obtain a planned hash or reach a secret POST',async t=>{
 const previous=globalThis.fetch;t.after(()=>{globalThis.fetch=previous;});
 for(const example of examples)for(const value of ['',undefined]){
  const f=fixture();assert.throws(()=>f.provider.describe(example.effect,f.i,f.acks,value),{code:'invalid_request'});let posts=0;
  globalThis.fetch=async(url,init)=>{if(init.method==='POST'){posts++;assert.fail('no invalid secret mutation');}return Response.json(new URL(String(url)).pathname.endsWith('/machines/'+f.machine.id)?f.machine:f.app);};
  await assert.rejects(f.provider.execute(example.effect,f.i,f.acks,value,originalTime(2000)),{code:'invalid_request'});assert.equal(posts,0);
 }
});
