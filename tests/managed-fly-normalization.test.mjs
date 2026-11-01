// Actual pure config validator / owned-readback path. No network, provider
// resource, Root/native/Source authority or process is created by these tests.
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {machineConfig,machineAck,FlyManagedHomeProvider,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
// NONSECRET exact mount projections from actual original Fly observations.
// Both raw config objects OMIT env/services/auto_destroy, rather than containing
// literal null. Their init-only sleep process is NOT a managed process proof.
const observed=[
 {source:'/workspace/rudder-fresh-volume-run-a1/008-original-machine-observe.stdout.private',source_sha256:'26a6daabcb8b8c6ed24e7708fe476f4e8d9db2bd3aec84e085f67b6cc0ca13f6',mount:{encrypted:true,path:'/qualification-volume',size_gb:1,volume:'vol_4ql1one5jpoenqdr',name:'rudder_qv_46863c9a4286'}},
 {source:'/workspace/rudder-fresh-volume-run-a2/008-original-machine-observe.stdout.private',source_sha256:'3ce004672576d2cc2ce81c522566fd495243eae15eb3e7d0407216fe8bba868a',mount:{encrypted:true,path:'/qualification-volume',size_gb:1,volume:'vol_vgnqlpqy2jmd13z4',name:'rudder_qv_2130ae6e2bf2'}},
];
function fixture(sample=observed[0],configured=true){
 const i={appName:'rudder-normalization-fixture',homeId:'mh-'+randomUUID(),nonce:randomUUID(),storeId:randomUUID(),policy:{image:'registry.fly.io/rudder-workers-exla@sha256:'+'1'.repeat(64),region:'sjc',cpus:1,memoryMb:512,volumeGb:1}};
 const volume={id:sample.mount.volume,name:sample.mount.name,region:i.policy.region,sizeGb:sample.mount.size_gb},expected=machineConfig(i,volume,configured),config=structuredClone(expected);
 // Constructed managed request with ONLY the actually observed normalization
 // projected onto its fixed /workspace path. Not a whole physical managed ACK.
 config.mounts=[{...sample.mount,path:'/workspace'}];config.dns={skip_registration:true};for(const key of ['env','services','auto_destroy'])delete config[key];
 const body={id:'machine-original',instance_id:'instance-original',name:i.homeId,region:i.policy.region,host_status:'ok',state:'created',config};
 return {i,volume,expected,body};
}
for(const sample of observed)test('observed mount/default projection accepts exact original '+sample.mount.volume,()=>{
 const f=fixture(sample),before=structuredClone(f.body),ack=machineAck(f.body,f.i,f.expected,f.volume);
 assert.deepEqual(f.body,before);assert.deepEqual(ack.config,f.body.config);assert.equal(ack.configSha256,hash(canonical(f.body.config)));
 assert.notEqual(ack.configSha256,hash(canonical(f.expected)),'must hash actual provider shape, not request reconstruction');
 assert(!Object.hasOwn(ack.config,'env')&&!Object.hasOwn(ack.config,'services')&&!Object.hasOwn(ack.config,'auto_destroy'));
 assert.deepEqual(machineAck(f.body,f.i,ack.config,f.volume).config,f.body.config,'same already normalized expected config remains usable');
});
test('only known zero-valued env/services/auto_destroy representations are equivalent',()=>{
 for(const representation of ['request-empty','absent','null']){
  const f=fixture();
  if(representation==='request-empty'){f.body.config.env={};f.body.config.services=[];f.body.config.auto_destroy=false;}
  if(representation==='null'){f.body.config.env=null;f.body.config.services=null;f.body.config.auto_destroy=null;}
  assert.equal(machineAck(f.body,f.i,f.expected,f.volume).configSha256,hash(canonical(f.body.config)));
 }
 for(const [key,value]of [['env',{SECRET:'not-allowed'}],['env',[]],['env',false],['services',{}],['services',false],['services',[{ports:[{port:443}]}]],['auto_destroy',true],['auto_destroy',0]]){
  const f=fixture();f.body.config[key]=value;assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'},key);
 }
});
test('minimal legacy request mount still accepts without weakening expanded metadata',()=>{
 const f=fixture();f.body.config=structuredClone(f.expected);const ack=machineAck(f.body,f.i,f.expected,f.volume);assert.deepEqual(ack.config,f.expected);
});
test('expanded mount must bind exact original volume/name/size/encryption/path and no options',()=>{
 for(const [field,value]of [['volume','vol_foreign'],['name','foreign-original-name'],['size_gb',2],['size_gb','1'],['encrypted',false],['encrypted',null],['path','/foreign'],['snapshot_id','unobserved']]){
  const f=fixture();f.body.config.mounts[0][field]=value;assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'},field);
 }
 for(const change of [f=>{delete f.body.config.mounts[0].name;},f=>{f.body.config.mounts.push({...f.body.config.mounts[0]});},f=>{f.expected.mounts[0].volume='vol_foreign';},f=>{f.volume.name='foreign-argument';},f=>{f.volume.region='iad';},f=>{f.volume.sizeGb=2;}]){
  const f=fixture();change(f);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});
 }
});
test('process/init and nested env retain exact managed exec policy',()=>{
 const changes=[f=>{delete f.body.config.processes;},f=>{f.body.config.processes=null;},f=>{f.body.config.processes[0].env=null;},f=>{f.body.config.processes[0].env={PATH:'/foreign'};},f=>{f.body.config.processes[0].exec=['/bin/sh'];},f=>{f.body.config.processes[0].ignore_app_secrets=false;},f=>{f.body.config.init={exec:['/usr/bin/sleep','600']};},f=>{f.expected.processes[0].exec=['/bin/sh'];f.body.config.processes=structuredClone(f.expected.processes);}];
 for(const change of changes){const f=fixture();change(f);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});}
});
test('nonce/store/guest/machine identity and protected secret references stay strict',()=>{
 const changes=[f=>{f.body.name='foreign';},f=>{f.body.region='iad';},f=>{f.body.host_status='unavailable';},f=>{f.body.config.metadata['rudder-provision-nonce']=randomUUID();},f=>{f.body.config.metadata['rudder-store-id']=randomUUID();},f=>{f.body.config.guest.cpu_kind='shared';},f=>{f.body.config.files.pop();},f=>{f.body.config.files[0].raw_value='forbidden';},f=>{f.body.config.files[0].secret_name='FOREIGN';},f=>{f.body.config.files[0].mode=420;},f=>{f.body.config.command='/bin/sh';}];
 for(const change of changes){const f=fixture();change(f);assert.throws(()=>machineAck(f.body,f.i,f.expected,f.volume),{code:'identity_changed'});}
});
test('owned machine readback still requires same original id/instance and ACTUAL config hash',async()=>{
 const f=fixture(),owned=machineAck(f.body,f.i,f.expected,f.volume);let actual=f.body;
 const provider=new FlyManagedHomeProvider({request:async(method,path)=>{assert.equal(method,'GET');assert.equal(path,'/v1/apps/'+f.i.appName+'/machines/'+owned.id);return {status:200,body:actual};}});
 assert.deepEqual(await provider.observeMachine(f.i,owned,f.volume,true,originalTime(1000)),owned);
 for(const change of [v=>{v.id='foreign-id';},v=>{v.instance_id='foreign-instance';},v=>{v.config.env=null;}]){
  actual=structuredClone(f.body);change(actual);await assert.rejects(provider.observeMachine(f.i,owned,f.volume,true,originalTime(1000)),{code:'identity_changed'});
 }
});
