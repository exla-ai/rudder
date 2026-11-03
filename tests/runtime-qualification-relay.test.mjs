// Opaque relay mechanisms only. No Root registration key, successful signed
// ACK, provider candidate or qualification owner is constructed by this suite.
import test from 'node:test';
import assert from 'node:assert/strict';
import {once} from 'node:events';
import WebSocket,{WebSocketServer} from 'ws';
import {RuntimeQualificationRelay,READY,ACK} from '../cloud/worker/runtime-qualification.mjs';
const id='11111111-1111-4111-8111-111111111111',nonce='22222222-2222-4222-8222-222222222222';
const hash='a'.repeat(64),ref={kind:'blob',digest:hash,size_bytes:1};
function fixture(overrides={}){
  const scope={format_version:1,scope:'disposable-runtime-floor-and-original-client-only',qualification_id:id,qualification_nonce:nonce,
    image:`registry.fly.io/rudder-workers-exla@sha256:${hash}`,image_receipt_sha256:hash,source_manifest_sha256:hash,
    inventory:ref,manifest:ref,installer_input:{...ref,kind:'directory'},native_sha256:hash,buddy_sha256:hash,service_sha256:hash,launcher_sha256:hash};
  let current=100000000000n;const sent=[],emitted=[];
  const generation={authorized:true,closed:false,lost:false};
  const relay=new RuntimeQualificationRelay({scope,config:{configSha:hash,home_id:'fixture',incarnation:'fixture'},now:()=>current,
    send:(g,m)=>sent.push([g,m]),emit:m=>emitted.push(m),...overrides});
  const message={kind:READY,rpc_id:nonce,packet:{mac:'0'.repeat(64),body:{version:1,kind:'runtime_image_qualification_ready',qualification_id:id,
    qualification_nonce:nonce,registration_binding:{config_sha256:hash,home_id:'fixture',incarnation:'fixture'},root_nonce:id,registration_proof_sha256:hash,
    scope,observation:{request_nonce:nonce,original_enable_constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'130000000000'}},
    selected_executables:[{},{},{},{}],remaining_milliseconds:30000}}};
  return{relay,generation,message,sent,emitted,setTime:v=>current=v};
}
test('one original scoped packet is forwarded unchanged and never replayed',()=>{
  const f=fixture();f.relay.forward(f.message,f.generation,true);
  assert.equal(f.sent.length,1);assert.deepEqual(f.sent[0][1],{type:READY,rpc_id:nonce,packet:f.message.packet});
  assert.throws(()=>f.relay.forward(f.message,f.generation,true));assert.equal(f.sent.length,1);
  assert.deepEqual(Object.keys(f.relay.retainedData()).sort(),['attempted','receivedBytes','requestBytes','sent','unknown']);
});
test('absent scope, changed scope and non-current transport cannot forward',()=>{
  for(const variant of ['absent','scope','offline','unregistered']){
    const f=fixture(variant==='absent'?{scope:null}:{});
    if(variant==='scope')f.message.packet.body.scope={...f.message.packet.body.scope,qualification_nonce:id};
    if(variant==='offline')f.generation.lost=true;
    assert.throws(()=>f.relay.forward(f.message,f.generation,variant!=='unregistered'));
    assert.equal(f.sent.length,0);assert.equal(f.relay.retainedData().unknown,true);
  }
});
test('original cutoff expires without replacement window or send',()=>{
  const f=fixture();f.setTime(130000000000n);assert.throws(()=>f.relay.forward(f.message,f.generation,true));
  assert.equal(f.sent.length,0);f.setTime(100000000000n);assert.throws(()=>f.relay.forward(f.message,f.generation,true));
});
test('send exception retains original bytes and cannot retry on a replacement generation',()=>{
  const f=fixture({send:()=>{throw Error('actual transport mechanism failure');}});
  assert.throws(()=>f.relay.forward(f.message,f.generation,true));assert.equal(f.relay.retainedData().unknown,true);
  assert.throws(()=>f.relay.forward(f.message,{authorized:true},true));
});
test('unsolicited or malformed ACK never emits a Root receipt',()=>{
  const f=fixture();assert.throws(()=>f.relay.receive({type:ACK},f.generation));
  f.relay.forward(f.message,f.generation,true);
  assert.throws(()=>f.relay.receive({type:ACK,rpc_id:nonce,packet:{ready:true}},f.generation));
  assert.equal(f.emitted.length,0);assert.ok(f.relay.retainedData().receivedBytes>0);
});
test('real loopback WebSocket carries opaque original bytes and retains malformed reply',{timeout:4000},async t=>{
  const server=new WebSocketServer({host:'127.0.0.1',port:0});await once(server,'listening');
  let client;t.after(async()=>{client?.terminate();for(const socket of server.clients)socket.terminate();await new Promise(resolve=>server.close(resolve));});
  const accepted=once(server,'connection');client=new WebSocket(`ws://127.0.0.1:${server.address().port}`);
  await once(client,'open');const [peer]=await accepted;
  const f=fixture({send:(_g,m)=>client.send(JSON.stringify(m))});
  const incoming=once(peer,'message');f.relay.forward(f.message,f.generation,true);
  const [raw]=await incoming;assert.deepEqual(JSON.parse(raw.toString()),{type:READY,rpc_id:nonce,packet:f.message.packet});
  const reply=once(client,'message');peer.send(JSON.stringify({type:ACK,rpc_id:nonce,packet:{malformed:'no authority'}}));
  const [bad]=await reply;assert.throws(()=>f.relay.receive(JSON.parse(bad.toString()),f.generation));
  assert.equal(f.emitted.length,0);assert.ok(f.relay.retainedData().receivedBytes>0);
});

test('ordinary runtime relay rejects the compiler qualification domain and longer deadline',()=>{
 for(const constraint of [{format_version:1,domain:'compiler_sdk_qualification_v1',maximum_milliseconds:240000,boottime_deadline_nanoseconds:'340000000000'},
  {format_version:1,maximum_milliseconds:240000,boottime_deadline_nanoseconds:'340000000000'}]){
  const f=fixture();f.message.packet.body.observation.original_enable_constraint=constraint;
  assert.throws(()=>f.relay.forward(f.message,f.generation,true));assert.equal(f.sent.length,0);
 }
});
