// Real driver timer/state mechanics with explicit synthetic registry and WS
// boundaries. No PG commit, physical qualification or activation permit issuer.
import test from 'node:test';
import assert from 'node:assert/strict';
import {Server} from 'node:http';
import {EventEmitter} from 'node:events';
import {setImmediate as turn} from 'node:timers/promises';
import {createRequire} from 'node:module';
import {CompilerSdkQualificationDriver} from '../cloud/dist/managed-home-provisioning/compiler-qualification-driver.js';
import {RuntimeImageQualificationDriver} from '../cloud/dist/managed-home-provisioning/runtime-qualification-driver.js';
import {A2_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/index.js';
import {policy,candidate} from './fixtures/managed-runtime-qualification/data.mjs';
const require=createRequire(new URL('../cloud/package.json',import.meta.url));
const {WebSocketServer}=require('ws');

async function fixture(t,Type,role){
 const p=policy();p.controlPlane={profile:'qualification-v1',qualificationReceiptSha256:'a'.repeat(64)};
 p.compilerSdk=structuredClone(A2_COMPILER_SDK);
 const authority={mode:'postgres',authorityId:'00000000-0000-4000-8000-000000000001',
  connection:{host:'localhost',port:5432,database:'fixture',user:'fixture_runtime',password:'synthetic-only'}};
 const config={mode:'postgres',authority,registrationAuthority:{...authority,connection:{...authority.connection,user:'fixture_registration'}},
  policy:p,flyToken:'synthetic-provider-token-no-access',tokenEncryptionKey:Buffer.alloc(32,1)};
 const c=candidate(p);if(role==='sdk')c.sdk=structuredClone(A2_COMPILER_SDK);
 const driver=new Type(config,c,'fixture','original');
 t.after(()=>driver.registry.close());
 let now=Date.now(),server;const timers=[];
 t.mock.method(Date,'now',()=>now);
 t.mock.method(globalThis,'setTimeout',(callback,delay)=>{const timer={callback,delay,cleared:false};timers.push(timer);return timer;});
 t.mock.method(globalThis,'clearTimeout',timer=>{if(timer)timer.cleared=true;});
 t.mock.method(Server.prototype,'listen',function(){server=this;queueMicrotask(()=>this.emit('listening'));return this;});
 const ws=new EventEmitter();ws.readyState=1;const sends=[];
 ws.send=(_data,callback)=>{sends.push(callback);};
 t.mock.method(WebSocketServer.prototype,'handleUpgrade',(_request,_socket,_head,accept)=>accept(ws));
 driver.registry.authenticateManagedHomeWorker=async()=>({});
 driver.registry.bindAuthenticatedManagedHomeSocket=()=>{};
 driver.registry.closeManagedHostSocket=()=>{};
 driver.registry.provisionRuntimeImageQualification=async()=>({scope:{qualification_id:'synthetic'},compilerScope:{scope:'disposable-fixed-compiler-sdk-only'}});
  driver.registry.runtimeImageQualificationCustodyData=()=>{throw Error('no synthetic provider custody');};
 let permit,permitTakes=0,lateReject;
 driver.registry.takeOriginalRoleActivationPermit=()=>{permitTakes++;return permit;};
 const ack={body:{kind:role==='sdk'?'compiler_sdk_qualification_ack':'runtime_image_qualification_ack',result:{state:'published'}},mac:'a'.repeat(64)};
 driver.wire.receive=async(_worker,_message,reply)=>{
  await reply({type:'runtime_image_qualification_ack_v1',packet:ack});
  await new Promise((_resolve,reject)=>{lateReject=reject;});
 };
 await driver.startRetaining();
 assert.equal(driver.statusData().error,null,'synthetic provision must reach the live stage');
 assert.equal(timers.length,1);assert.equal(timers[0].delay,300_000);
 const socket={destroy(){throw Error('synthetic valid upgrade was rejected');}};
 server.emit('upgrade',{method:'GET',url:'/api/rudder/home/mh-00000000-0000-4000-8000-000000000001/control',
  headers:{host:'rudder-v4-qualification.fly.dev','x-forwarded-proto':'https',authorization:'Bearer rdrmh_'+'a'.repeat(64)}},socket,Buffer.alloc(0));
 await turn();
 assert.equal(ws.listenerCount('message'),1,'exercise the actual driver message handler');
 return {driver,server,ws,timers,fuse:timers[0],sends,
  advance(ms){now+=ms;},publish(){ws.emit('message',Buffer.from('{}'));},
  permit(value){permit=value;},takes:()=>permitTakes,rejectLate(){lateReject?.(Error('late synthetic wire failure'));}};
}

for(const [role,Type,state]of [['sdk',CompilerSdkQualificationDriver,'sdk_role_commit_observed'],['runtime',RuntimeImageQualificationDriver,'role_commit_observed']]){
 test(role+' live stage expires on the original timer before any ACK',async t=>{
  const f=await fixture(t,Type,role);f.fuse.callback();
  assert.equal(f.driver.statusData().expired,true);assert.equal(f.driver.statusData().error,'expired');
  assert.throws(()=>f.driver.takeOriginalRoleActivationPermit());assert.equal(f.takes(),0);
 });
 for(const sendCallback of [false,true])test(role+' '+(sendCallback?'ACK send callback':'ACK queued')+' cannot settle the fuse without an original permit',async t=>{
  const f=await fixture(t,Type,role);f.publish();assert.equal(f.sends.length,1);
  if(sendCallback){f.sends[0]();await turn();}
  assert.equal(f.driver.takeOriginalRoleActivationPermit(),undefined);
  assert.equal(f.takes(),sendCallback?1:0);assert.equal(f.fuse.cleared,false);assert.equal(f.timers.length,1,'polling must not allocate a new deadline');
  f.fuse.callback();assert.equal(f.driver.statusData().expired,true);assert.equal(f.driver.statusData().error,'expired');
  assert.throws(()=>f.driver.takeOriginalRoleActivationPermit());
 });
 test(role+' original permit handoff clears its fuse and rejects a previously queued expiry callback',async t=>{
  const f=await fixture(t,Type,role);f.publish();f.sends[0]();await turn();
  const syntheticPermit=Object.freeze({syntheticRegistryBoundaryOnly:true});f.permit(syntheticPermit);
  assert.equal(f.driver.takeOriginalRoleActivationPermit(),syntheticPermit);assert.equal(f.takes(),1);
  assert.equal(f.fuse.cleared,true);assert.equal(f.timers.length,1);
  f.advance(300_001);f.fuse.callback(); // Explicitly invoke even a previously queued, now-cleared callback.
  const completed=f.driver.statusData();assert.equal(completed.state,state);assert.equal(completed.expired,false);assert.equal(completed.error,null);
  assert.equal(completed.ack_send_unknown,false);assert.equal(completed.messages_observed,1);assert.equal(completed.owners_retained,true);
  assert.throws(()=>f.driver.takeOriginalRoleActivationPermit(),'consumed stage cannot yield another permit');assert.equal(f.takes(),1);
 });
 test(role+' completed fuse does not hide a genuine retained owner failure',async t=>{
  const f=await fixture(t,Type,role);f.publish();f.sends[0]();await turn();f.permit({syntheticRegistryBoundaryOnly:true});
  assert(f.driver.takeOriginalRoleActivationPermit());assert.equal(f.fuse.cleared,true);
  f.ws.emit('error',Error('late retained socket failure'));f.rejectLate();await turn();
  assert.equal(f.driver.statusData().ack_send_unknown,true);assert.equal(f.driver.statusData().error,'effect_unknown');
  assert.equal(f.driver.statusData().expired,false);assert.equal(f.driver.statusData().owners_retained,true);
 });
 test(role+' late permit is rejected against the original clock even if its timer callback was delayed',async t=>{
  const f=await fixture(t,Type,role);f.publish();f.sends[0]();await turn();f.permit({syntheticRegistryBoundaryOnly:true});
  f.advance(300_001);assert.equal(f.fuse.cleared,false);
  assert.throws(()=>f.driver.takeOriginalRoleActivationPermit(),{code:'expired'});assert.equal(f.takes(),0);
  assert.equal(f.driver.statusData().expired,true);assert.equal(f.driver.statusData().error,'expired');assert.equal(f.timers.length,1);
 });
}
