// Component sequencing with real session ownership and synthetic registry/
// transport data. These tests issue no provider, Root, profile or model grants.
import assert from 'node:assert/strict';
import test from 'node:test';
import {setImmediate as turn} from 'node:timers/promises';
import {randomUUID} from 'node:crypto';
import {HomeControlSessions} from '../cloud/dist/home-control-sessions.js';
import {originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {preparePositiveIngressRetaining,validatePositiveIngressInput} from '../cloud/dist/managed-home-provisioning/staging-positive-ingress-driver.js';

const input={openCommandKey:'original-open',startCommandKey:'original-start',model:'fixture-model',profileId:'fixture-profile',text:'fixture task'};
const fence={run_id:'original-run',run_epoch:'4',attempt_id:'original-attempt'};
function fixture(hooks={},selection=input){
 const socket={open:true},homeId='fixture-home',sent=[],operations=new Map();let expired=false;
 let session;
 const sessions=new HomeControlSessions({isOpen:s=>s.open,send:(s,body)=>{
  assert.equal(s,socket);const message=JSON.parse(body);sent.push(message);
  const respond=result=>sessions.reply(session,message.id,result);
  if(hooks.send?.(message,respond)===false)return true;
  queueMicrotask(()=>respond(message.method==='managed.receiptV1'?
   {receipt:{state:'completed',result:message.params.lookup_command_key===input.openCommandKey?
    {phase:'project_ready',project_alias:'fixture-project'}:{phase:'task_queued',project_alias:'fixture-project',...fence}},
    task_observation:message.params.lookup_command_key===input.startCommandKey?{...fence,backend_ready_last_observed:true,uncertain_observed:false}:null}:
   {receipt:{state:'accepted'}}));return true;
 }});
 session=sessions.attach(homeId,socket);
 const route={homeId,projectAlias:'fixture-project',assertRoutableCurrent:async()=>{}};
 const owner={homeId,socket,assertLocalCurrent(){assert(socket.open);},async assertCurrent(){await hooks.current?.();}};
 const registry={async assertManagedHomeSocketMatchesRoute(s,r){assert.equal(s,socket);assert.equal(r,route);},
  dispatchManagedHomeCommand(r,key,op,send){assert.equal(r,route);
   if(op.op!=='ReceiptV1'){assert(!operations.has(key),'mutation replayed');operations.set(key,op);}
   return send(op.op==='ReceiptV1'?'managed.receiptV1':op.op==='StartTaskV1'?'managed.startTaskV1':op.op==='StopTaskV1'?'managed.stopTaskV1':'managed.openPreparedProjectV1',
    op.op==='ReceiptV1'?{lookup_command_key:op.lookup_command_key}:{command_key:key,...op},20000);
  }};
 const driver=preparePositiveIngressRetaining({registry,route,sessions,session,socket,owner,
  setup:{remaining(){if(expired)throw Error('expired original setup');return 60000;}},driver:originalTime(60000)},selection);
 return {driver,sessions,session,socket,sent,operations,expire(){expired=true;}};
}

test('Open and Start each dispatch once on the same session, after the exact original replies',async()=>{
 let finishOpen;const f=fixture({send(message,respond){if(message.method==='managed.openPreparedProjectV1'){finishOpen=respond;return false;}}});
 const run=f.driver.runRetaining();await turn();assert.deepEqual([...f.operations.keys()],[input.openCommandKey]);
 assert.equal(f.sessions.reply(f.session,'foreign-response',{}),false);
 finishOpen({receipt:{state:'accepted'}});await run;
 assert.deepEqual(f.sent.map(x=>x.method),['managed.openPreparedProjectV1','managed.receiptV1','managed.startTaskV1','managed.receiptV1']);
 const status=f.driver.statusData();assert.equal(status.backend_ready_last_observed,true);assert.deepEqual(status.fence,fence);
 assert.equal(status.lifecycle_complete,false);assert.equal(status.original_helper_join_proved,false);
 assert.throws(()=>f.driver.runRetaining());assert.throws(()=>JSON.stringify(f.driver));
});

test('replacement socket after Open cannot receive Start or receipt replay',async()=>{
 let f;f=fixture({send(message,respond){if(message.method==='managed.openPreparedProjectV1'){
  queueMicrotask(()=>{respond({receipt:{state:'accepted'}});f.sessions.attach('fixture-home',{open:true});});return false;
 }}});
 await assert.rejects(f.driver.runRetaining(),/incomplete/);
 assert.deepEqual(f.sent.map(x=>x.method),['managed.openPreparedProjectV1']);assert.equal(f.operations.size,1);
});

test('unknown Start write retains the original promise and does not retry or observe a replacement',async()=>{
 let f;f=fixture({send(message){if(message.method==='managed.startTaskV1'){
  queueMicrotask(()=>f.sessions.close(f.session));return false;
 }}});
 await assert.rejects(f.driver.runRetaining(),/incomplete/);
 assert.equal(f.operations.size,2);assert.equal(f.sent.filter(x=>x.method==='managed.startTaskV1').length,1);
 const status=f.driver.statusData();assert.equal(status.commands[1].dispatch_unknown,true);assert.equal(status.commands[1].receipt_queries,0);
});

test('a foreign task observation cannot satisfy the retained launch fence',async()=>{
 const f=fixture({send(message,respond){if(message.method==='managed.receiptV1'&&message.params.lookup_command_key===input.startCommandKey){
  queueMicrotask(()=>respond({receipt:{state:'completed',result:{phase:'task_queued',project_alias:'fixture-project',...fence}},
   task_observation:{...fence,attempt_id:'foreign-attempt',backend_ready_last_observed:true,uncertain_observed:false}}));return false;
 }}});
 await assert.rejects(f.driver.runRetaining(),/incomplete/);assert.equal(f.driver.statusData().backend_ready_last_observed,false);
});

test('expiry during asynchronous owner validation prevents the first send',async()=>{
 let f;f=fixture({async current(){f.expire();await turn();}});
 await assert.rejects(f.driver.runRetaining(),/incomplete/);assert.equal(f.sent.length,0);
});

test('explicit selection is closed and rejects invalid command identities before staging effects',()=>{
 assert.throws(()=>validatePositiveIngressInput({...input,startCommandKey:input.openCommandKey}));
 assert.throws(()=>validatePositiveIngressInput({...input,extra:'unsupported'}));
 assert.throws(()=>validatePositiveIngressInput({...input,text:' '}));
 assert.throws(()=>validatePositiveIngressInput({...input,model:'fixture\nmodel'}));
 assert.throws(()=>validatePositiveIngressInput({...input,stopCommandKey:input.startCommandKey}));
});

test('Stop targets the original Start fence once and waits for actual join and enrollment release observations',async()=>{
 const commandId=randomUUID(),selection={...input,stopCommandKey:'original-stop'};
 let stopped=false,finishJoin;const f=fixture({send(message,respond){
  if(message.method==='managed.stopTaskV1'){
   assert.equal(message.params.lookup_command_key,input.startCommandKey);assert.equal(message.params.command_id,commandId);
   for(const key of Object.keys(fence))assert.equal(message.params[key],fence[key]);stopped=true;
   queueMicrotask(()=>respond({receipt:{state:'accepted'}}));return false;
  }
  if(message.method==='managed.receiptV1'&&message.params.lookup_command_key===selection.stopCommandKey){
   queueMicrotask(()=>respond({receipt:{state:'completed',result:{phase:'stop_requested',project_alias:'fixture-project',...fence,stop_requested_observed:true}}}));return false;
  }
  if(message.method==='managed.receiptV1'&&message.params.lookup_command_key===input.startCommandKey){
   const data={receipt:{command_id:commandId,state:'completed',result:{phase:'task_queued',project_alias:'fixture-project',...fence}},
    task_observation:{command_id:commandId,...fence,row_status:stopped?'stopped':'running',backend_ready_last_observed:true,uncertain_observed:false,
     enrollment_submitted_observed:true,enrollment_released_observed:false,helper_joined_observed:null,stop_requested_observed:stopped,validated_checkpoint_observed:stopped}};
   if(stopped)finishJoin=()=>respond({...data,task_observation:{...data.task_observation,enrollment_released_observed:true,helper_joined_observed:true}});
   else queueMicrotask(()=>respond(data));return false;
  }
 }},selection);
 const run=f.driver.runRetaining();await turn();assert.equal(typeof finishJoin,'function');
 assert.equal(f.driver.statusData().lifecycle_complete,false,'a stopped row and stop ACK do not prove a physical join');
 finishJoin();await run;
 const status=f.driver.statusData();assert.equal(status.lifecycle_complete,true);assert.equal(status.original_helper_join_proved,true);
 assert.equal(status.enrollment_release_proved,true);assert.equal(status.checkpoint_accepted,true);assert.equal(status.qualified,false);
 assert.equal(status.retirement_complete,false);assert.equal(f.sent.filter(v=>v.method==='managed.stopTaskV1').length,1);
 assert.deepEqual([...f.operations.keys()],[input.openCommandKey,input.startCommandKey,selection.stopCommandKey]);
});

test('an unknown Stop write retains the original dispatch without replay',async()=>{
 const commandId=randomUUID();let f;f=fixture({send(message,respond){
  if(message.method==='managed.receiptV1'&&message.params.lookup_command_key===input.startCommandKey){
   queueMicrotask(()=>respond({receipt:{command_id:commandId,state:'completed',result:{phase:'task_queued',project_alias:'fixture-project',...fence}},
    task_observation:{...fence,backend_ready_last_observed:true,uncertain_observed:false,enrollment_submitted_observed:true}}));return false;
  }
  if(message.method==='managed.stopTaskV1'){queueMicrotask(()=>f.sessions.close(f.session));return false;}
 }},{...input,stopCommandKey:'original-stop'});
 await assert.rejects(f.driver.runRetaining(),/incomplete/);
 assert.equal(f.sent.filter(v=>v.method==='managed.stopTaskV1').length,1);assert.equal(f.driver.statusData().lifecycle_complete,false);
 assert.equal(f.driver.statusData().commands[2].dispatch_unknown,true);
});
