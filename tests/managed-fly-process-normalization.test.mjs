// Pure protocol tests. A6's observed init:{} / omitted process.env projection
// was reported by Root from authenticated Machine683421da319328. These fixtures
// reconstruct that shape on an inert request; they are not physical qualification.
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {machineConfig,machineAck,FlyManagedHomeProvider,FlyHttpTransport,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
function fixture(){
 const i={appName:'rudder-process-normalization',homeId:'mh-'+randomUUID(),nonce:randomUUID(),storeId:randomUUID(),networkName:'fixture-network',runtimeQualification:{fixture_data_only:true},policy:{organization:'fixture-org',image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),region:'sjc',cpus:1,memoryMb:512,volumeGb:1}};
 const volume={id:'vol_original_fixture',name:'rudder_'+i.nonce.replaceAll('-','').slice(0,23),region:i.policy.region,sizeGb:1},expected=machineConfig(i,volume,false),config=structuredClone(expected);
 config.init={};delete config.processes[0].env;for(const key of ['env','services','auto_destroy'])delete config[key];
 config.mounts=[{volume:volume.id,path:'/workspace',encrypted:true,size_gb:1,name:volume.name}];
 return {i,volume,expected,body:{id:'machine-original',instance_id:'instance-original',name:i.homeId,region:i.policy.region,host_status:'ok',state:'created',config}};
}
test('fresh and configured process vectors preserve the image env-clear entrypoint exactly',()=>{
 const f=fixture(),fixed=[{exec:['/usr/bin/env','-i','/usr/local/bin/rudder-machine-root'],env:{},ignore_app_secrets:true}];
 for(const configured of [false,true]){
  const config=machineConfig(f.i,f.volume,configured);assert.deepEqual(config.processes,fixed);assert.deepEqual(config.env,{});
  assert(!Object.hasOwn(config,'init'),'no competing init override');
  if(configured){assert.equal(config.files.length,3);assert(config.files.every(file=>file.secret_name&&file.mode===384&&!Object.hasOwn(file,'raw_value')));}
 }
 const ack=machineAck(f.body,f.i,f.expected,f.volume);assert.deepEqual(ack.config.processes[0].exec,fixed[0].exec);
 assert.equal(ack.configSha256,hash(canonical(f.body.config)),'actual response bytes still bind the config hash');
});
test('matching old direct launch, missing env-clear, extra args or alternate executables never redefine the allowlist',()=>{
 const root='/usr/local/bin/rudder-machine-root';
 for(const exec of [[root],['/usr/bin/env',root],['env','-i',root],['/usr/bin/env','-i',root,'extra'],['/usr/bin/env','-i','PATH=/foreign',root],['/bin/sh','-c','exec /usr/bin/env -i '+root]]){
  for(const sides of [['expected'],['actual'],['expected','actual']]){
   const f=fixture();for(const side of sides)(side==='expected'?f.expected:f.body.config).processes[0].exec=structuredClone(exec);
   assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'},sides.join('+')+'/'+JSON.stringify(exec));
  }
 }
});
test('observed A6 empty-init/omitted-process-env shape accepts and preserves raw bytes/hash',()=>{
 const f=fixture(),before=structuredClone(f.body),ack=machineAck(f.body,f.i,f.expected,f.volume);assert.deepEqual(f.body,before);assert.deepEqual(ack.config,before.config);
 assert.equal(ack.configSha256,hash(canonical(before.config)));assert.notEqual(ack.configSha256,hash(canonical(f.expected)));assert(!Object.hasOwn(ack.config.processes[0],'env'));assert.deepEqual(ack.config.init,{});
 assert.deepEqual(machineAck(f.body,f.i,ack.config,f.volume),ack,'already observed config is a valid expected binding');
});
test('observed null incomplete_config means absent while an actual incomplete config refuses',()=>{
 for(const value of [undefined,null]){const f=fixture();f.body.incomplete_config=value;assert.equal(machineAck(f.body,f.i,f.expected,f.volume).id,f.body.id);}
 for(const value of [false,0,'',{},[],{image:'pending'}]){const f=fixture();f.body.incomplete_config=value;assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});}
});
test('only observed empty-or-omitted init/process.env combinations are equivalent',()=>{
 for(const expectedInit of [false,true])for(const expectedEnv of [false,true])for(const actualInit of [false,true])for(const actualEnv of [false,true]){
  const f=fixture();if(expectedInit)f.expected.init={};if(!expectedEnv)delete f.expected.processes[0].env;if(!actualInit)delete f.body.config.init;if(actualEnv)f.body.config.processes[0].env={};
  const raw=canonical(f.body.config);assert.equal(machineAck(f.body,f.i,f.expected,f.volume).configSha256,hash(raw));assert.deepEqual(canonical(f.body.config),raw);
 }
});
test('nonempty/malformed init and process env are refused on both expected and actual sides',()=>{
 for(const side of ['expected','actual'])for(const field of ['init','env'])for(const value of [null,[],false,0,'', {PATH:'/foreign'}, {exec:['/bin/sh']}]){
  const f=fixture(),c=side==='expected'?f.expected:f.body.config;if(field==='init')c.init=value;else c.processes[0].env=value;
  assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'},side+'/'+field+'/'+JSON.stringify(value));
 }
});
test('fixed exec/ignore_app_secrets/process cardinality and exact extra-key checks remain strict',()=>{
 const mutations=[c=>{c.processes[0].exec=['/bin/sh'];},c=>{c.processes[0].exec.push('argument');},c=>{delete c.processes[0].exec;},c=>{c.processes[0].ignore_app_secrets=false;},c=>{c.processes[0].ignore_app_secrets=null;},c=>{delete c.processes[0].ignore_app_secrets;},c=>{c.processes[0].unexpected_empty={};},c=>{c.processes.push(structuredClone(c.processes[0]));},c=>{c.processes=[];},c=>{delete c.processes;}];
 for(const side of ['expected','actual'])for(const change of mutations){const f=fixture();change(side==='expected'?f.expected:f.body.config);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});}
 const f=fixture();f.expected.processes[0].exec=['/bin/sh'];f.body.config.processes=structuredClone(f.expected.processes);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'},'matching malicious substitutions cannot redefine the fixed process');
});
test('guest/image/nonce/store/expanded-volume metadata and ingress safeguards remain exact',()=>{
 const changes=[f=>{f.body.config.guest.cpus=2;},f=>{f.body.config.image='foreign-image';},f=>{f.body.config.metadata['rudder-provision-nonce']=randomUUID();},f=>{f.body.config.metadata['rudder-store-id']=randomUUID();},f=>{f.body.config.mounts[0].volume='vol_foreign';},f=>{f.body.config.mounts[0].name='foreign';},f=>{f.body.config.services=[{ports:[{port:443}]}];},f=>{f.body.config.env={SECRET:'forbidden'};},f=>{f.body.config.auto_destroy=true;},f=>{f.body.host_status='unreachable';}];
 for(const change of changes){const f=fixture();change(f);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});}
});
test('readback still rejects actual config-hash drift between equivalent empty representations',async()=>{
 const f=fixture(),owned=machineAck(f.body,f.i,f.expected,f.volume);let actual=f.body;const provider=new FlyManagedHomeProvider({request:async()=>({status:200,body:actual})});
 assert.deepEqual(await provider.observeMachine(f.i,owned,f.volume,false,originalTime(1000)),owned);
 for(const change of [c=>{delete c.init;},c=>{c.processes[0].env={};}]){actual=structuredClone(f.body);change(actual.config);await assert.rejects(provider.observeMachine(f.i,owned,f.volume,false,originalTime(1000)),{code:'identity_changed'});}
});
test('original Machine POST ID and valid instance survive later malformed optional config',async t=>{
 const previous=globalThis.fetch;t.after(()=>{globalThis.fetch=previous;});
 for(const invalidInstance of [false,true])for(const badConfig of ['metadata','absent']){
  const f=fixture();if(invalidInstance)f.body.instance_id='invalid instance';if(badConfig==='metadata')f.body.config.metadata['rudder-provision-nonce']='foreign';else delete f.body.config;
  const app={id:'app-original',name:f.i.appName,network:f.i.networkName,network_cidr:'fdaa:1::/48',organization:{slug:f.i.policy.organization}},acks={app:{id:app.id,name:app.name,network:app.network,networkCidr:app.network_cidr,organization:f.i.policy.organization},volume:f.volume};let posts=0;
  globalThis.fetch=async(url,init)=>{if(init.method==='GET')return Response.json(app);posts++;assert.equal(String(url),'https://api.machines.dev/v1/apps/'+f.i.appName+'/machines');return Response.json(f.body,{status:200});};
  const provider=new FlyManagedHomeProvider(new FlyHttpTransport('synthetic-private-protocol-fixture',128));await assert.rejects(provider.execute('machine',f.i,acks,undefined,originalTime(1000)),{code:'identity_changed'});
  const data=provider.originalDisposableResponseData(f.i),partial=data.original_responses.machine;assert.equal(posts,1);assert.deepEqual(partial,invalidInstance?{id:'machine-original'}:{id:'machine-original',instanceId:'instance-original'});assert(!Object.hasOwn(partial,'config'));assert.equal(data.transport_requests.at(-1).status,200);assert.equal(data.transport_requests.at(-1).body_complete,true);
 }
});

test('failed machine reads retain one bounded redacted diagnostic without becoming an ACK',async()=>{
 const f=fixture(),owned=machineAck(f.body,f.i,f.expected,f.volume);let body=structuredClone(f.body);
 const provider=new FlyManagedHomeProvider({request:async()=>({status:200,body})});
 const secret='TEST_ONLY_PRIVATE_CONFIG_VALUE';body.state='replacing';body.config.env={SECRET:secret};
 await assert.rejects(provider.observeMachine(f.i,owned,f.volume,false,originalTime(1000)),{code:'identity_changed'});
 let data=provider.originalDisposableResponseData(f.i);assert.equal(data.latest_machine_read.stage,'shape');assert.equal(data.latest_machine_read.state,'replacing');
 assert.match(data.latest_machine_read.configSha256,/^[a-f0-9]{64}$/);assert(!JSON.stringify(data).includes(secret));assert.equal(data.original_responses.configure,null);
 for(let n=0;n<10;n++){body.state=secret+n;await assert.rejects(provider.observeMachine(f.i,owned,f.volume,false,originalTime(1000)));}
 data=provider.originalDisposableResponseData(f.i);assert.equal(data.latest_machine_read.state,'unrecognized');assert(!JSON.stringify(data).includes(secret));
 body=structuredClone(f.body);await provider.observeMachine(f.i,owned,f.volume,false,originalTime(1000));
 assert.equal(provider.originalDisposableResponseData(f.i).latest_machine_read.stage,'verified');
});
