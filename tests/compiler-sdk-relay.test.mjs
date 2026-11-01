// Real opaque relay mechanism; no successful Root ACK/key or SDK owner.
import test from 'node:test';import assert from 'node:assert/strict';
import {RuntimeQualificationRelay,READY,ACK} from '../cloud/worker/runtime-qualification.mjs';
const id='11111111-1111-4111-8111-111111111111',nonce='22222222-2222-4222-8222-222222222222';
function fixture(){const hash='a'.repeat(64),r={kind:'blob',algorithm:'sha256',digest:hash,size_bytes:1};
 const scope={format_version:1,scope:'disposable-runtime-floor-and-original-client-only',qualification_id:id,qualification_nonce:nonce,image:'registry.fly.io/rudder-workers-exla@sha256:'+hash,image_receipt_sha256:hash,source_manifest_sha256:hash,inventory:r,manifest:r,installer_input:{...r,kind:'directory'},native_sha256:hash,buddy_sha256:hash,service_sha256:hash,launcher_sha256:hash};
 const sdk={...scope,scope:'disposable-fixed-compiler-sdk-only',sdk:{fixture_data_only:true}};const sent=[],out=[];let now=1n;
 const generation={authorized:true,closed:false,lost:false};const relay=new RuntimeQualificationRelay({scope,compilerScope:sdk,config:{configSha:hash,home_id:'home',incarnation:'instance'},now:()=>now,send:(g,m)=>sent.push([g,m]),emit:m=>out.push(m)});
 const message={kind:READY,rpc_id:nonce,packet:{mac:hash,body:{version:1,kind:'compiler_sdk_qualification_ready',qualification_id:id,qualification_nonce:nonce,registration_binding:{config_sha256:hash,home_id:'home',incarnation:'instance'},root_nonce:id,registration_proof_sha256:hash,scope:sdk,observation:{request_nonce:nonce,original_enable_constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'30000000000'}},selected_executables:[{},{},{},{}],remaining_milliseconds:30000,sdk_proof:{opaque:true}}}};
 return{relay,message,generation,sent,out,expire:()=>now=30000000000n};}
test('SDK opaque traffic uses the same current socket but cannot replay or accept a late ACK',()=>{
 const f=fixture();f.relay.forward(f.message,f.generation,true);assert.equal(f.sent.length,1);assert.throws(()=>f.relay.forward(f.message,f.generation,true));
 const late=fixture();late.relay.forward(late.message,late.generation,true);late.expire();assert.throws(()=>late.relay.receive({type:ACK,rpc_id:nonce,packet:{body:{},mac:'0'.repeat(64)}},late.generation));assert.equal(late.out.length,0);
});
test('SDK subtype cannot silently downgrade to runtime proof or an unrelated original purpose',()=>{
 const f=fixture();const bad=structuredClone(f.message);bad.packet.body.kind='runtime_image_qualification_ready';assert.throws(()=>f.relay.forward(bad,f.generation,true));
 const g=fixture();assert.throws(()=>g.relay.receive({type:ACK,rpc_id:nonce,packet:{body:{},mac:'0'.repeat(64)}},g.generation));assert.equal(g.out.length,0);
});
