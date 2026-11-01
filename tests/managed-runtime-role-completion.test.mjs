// UNRUN. Actual production HMAC/source/transport classes + real loopback WS,
// with EXPLICIT model PG/Fly/Root evidence. No physical Root/PG/enablement claim.
import test from 'node:test';
import assert from 'node:assert/strict';
import {fixture} from './fixtures/managed-runtime-qualification/completion-model.mjs';
const linux={skip:process.platform!=='linux',timeout:20000};

for(const fragment of ['INSERT INTO rudder_project_authority.managed_runtime_image_roles',"SET state='published'",'SELECT * FROM rudder_project_authority.managed_runtime_image_roles']){
 test('actual original socket close while awaiting '+fragment+' refuses publication',linux,async t=>{
  const f=await fixture();t.after(()=>f.restore());let closed=false;
  f.pool.hook=async sql=>{if(!closed&&sql.includes(fragment)){closed=true;f.registry.closeManagedHostSocket(f.socket);}};
  await assert.rejects(f.publish(),{code:'effect_unknown'});assert(closed);assert.throws(()=>f.transport.assertLocalCurrent(),{code:'identity_changed'});
  assert.equal(f.pool.completion,null);await assert.rejects(f.source.assertPublishedRole(f.p),{code:'unconfigured'});
 });
}

test('close during successful first COMMIT leaves prepared DATA but no witness/admission/ACK',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());f.pool.afterCommit=async phase=>{if(phase==='prepared')f.close();};
 await assert.rejects(f.publish(),{code:'effect_unknown'});assert(f.pool.role&&f.pool.q.ack_json);assert.equal(f.pool.completion,null);
 await assert.rejects(f.source.assertPublishedRole(f.p),{code:'unconfigured'});
 const historical=await f.source.historicalResultData(f.home.intent.accountId,f.home.intent.runtimeQualification.qualification_id);
 assert.equal(historical.original_ack_success,false);assert(historical.prepared_role_data.ack_json);assert.equal(historical.prepared_role_data.completion_json,null);
 await assert.rejects(f.publish(),{code:'effect_unknown'});assert.equal(f.pool.completion,null);
});

test('lost original first COMMIT response cannot be promoted from its durable row',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());f.pool.afterCommit=async phase=>phase==='prepared'?'unknown':null;
 await assert.rejects(f.publish(),{code:'effect_unknown'});assert(f.pool.role);assert.equal(f.pool.completion,null);
 await assert.rejects(f.source.assertPublishedRole(f.p),{code:'unconfigured'});
 const events=f.pool.events.length;await assert.rejects(f.publish(),{code:'effect_unknown'});assert.equal(f.pool.events.length,events);
});

test('actual15s provider lease expiring during first COMMIT cannot return a30s success',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());const now=Date.now;
 try{f.pool.afterCommit=async phase=>{if(phase==='prepared'){const wall=now();Date.now=()=>wall+16000;}};
  await assert.rejects(f.publish(),{code:'effect_unknown'});assert(f.pool.role);assert.equal(f.pool.completion,null);
  // The outer30 still has time; it is the actual15s custody that must refuse.
  assert(f.original.remaining()>0);
 }finally{Date.now=now;}
 await assert.rejects(f.source.assertPublishedRole(f.p),{code:'unconfigured'});
});

test('known original COMMIT creates immutable completion only through the actual producer',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());const ack=await f.publish();assert.equal(ack.body.result.state,'published');assert(f.pool.role&&f.pool.completion);
 assert.equal(f.pool.completion.completion_json.prepared_role.role_binding_sha256,f.pool.role.role_binding_sha256);
 assert.equal(f.pool.completion.completion_json.request_sha256,ack.body.request_sha256);await f.source.assertPublishedRole(f.p);
});

test('close during completion INSERT retains known original witness but rolls back admission',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());let closed=false;
 f.pool.hook=async sql=>{if(!closed&&sql.startsWith('INSERT INTO')&&sql.includes('managed_runtime_role_completions')){closed=true;f.close();}};
 await assert.rejects(f.publish(),{code:'effect_unknown'});assert(f.pool.role);assert.equal(f.pool.completion,null);assert(closed);
 await assert.rejects(f.source.assertPublishedRole(f.p),{code:'unconfigured'});
});

test('unknown completion COMMIT may record real historical completion but never restores original ACK',linux,async t=>{
 const f=await fixture();t.after(()=>f.restore());f.pool.afterCommit=async phase=>phase==='completion'?'unknown':null;
 await assert.rejects(f.publish(),{code:'effect_unknown'});assert(f.pool.role&&f.pool.completion);
 // This row records an event that was actually observed BEFORE that second
 // write. It is valid artifact qualification DATA, independent of ACK delivery.
 await f.source.assertPublishedRole(f.p);
 const h=await f.source.historicalResultData(f.home.intent.accountId,f.home.intent.runtimeQualification.qualification_id);assert.equal(h.original_ack_success,false);assert(h.prepared_role_data.completion_json);
 const events=f.pool.events.length;await assert.rejects(f.publish(),{code:'effect_unknown'});assert.equal(f.pool.events.length,events);
});
