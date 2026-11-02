// Closed diagnostic projection only; no control startup, network or role grant.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';

const source=await fs.readFile(new URL('../examples/managed-runtime-image/qualify-runtime-role.mjs',import.meta.url),'utf8');
const begin=source.indexOf('function startupFailureData('),end=source.indexOf('const retain=');
assert(begin>=0&&end>begin);
const project=vm.runInNewContext(source.slice(begin,end)+'\nstartupFailureData;');
const data=(...args)=>JSON.parse(JSON.stringify(project(...args)));

test('startup check and original settled storage failure remain separate DATA',()=>{
 const result=data('storage-retention',false,{state:'failed',io_settled:true});
 assert.deepEqual(result,{format_version:1,error:'qualification_driver_start_failed',owners_retained:false,
  configured_activation:false,startup_check:'storage-retention',storage_retention:{state:'failed',io_settled:true}});
 assert.deepEqual(data('flow-construction',true,{state:'qualified',io_settled:false}).storage_retention,
  {state:'qualified',io_settled:false});
});

test('private labels and extra provider fields never appear in diagnostics',()=>{
 const secret='private-access-token-do-not-print';
 const result=data(secret,secret,{state:secret,io_settled:secret,token:secret,workload_role_arn:secret,policy_sha256:secret});
 assert.equal(JSON.stringify(result).includes(secret),false);
 assert.equal(result.startup_check,'unclassified');
 assert.equal(result.owners_retained,false);
 assert.deepEqual(result.storage_retention,{state:'unavailable',io_settled:null});
 assert.deepEqual(Object.keys(result.storage_retention),['state','io_settled']);
});

test('missing storage observations remain unavailable rather than settled',()=>{
 for(const retention of [undefined,null,{},false,1,'failed',{state:'failed'},{io_settled:true}]){
  const result=data('private-input',false,retention);
  assert.equal(result.configured_activation,false);
  assert.equal(result.storage_retention.state,['unconfigured','checking','qualified','failed'].includes(retention?.state)?retention.state:'unavailable');
  assert.equal(result.storage_retention.io_settled,typeof retention?.io_settled==='boolean'?retention.io_settled:null);
 }
});
