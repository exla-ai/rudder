// Component mechanisms only. No Root/SDK/role publication or native grant.
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
import {StagingListenerAdmissionClosure} from '../cloud/dist/managed-home-provisioning/staging-listener-admission.js';
import {OriginalSdkRoleActivationPermit,assertOriginalSdkRoleActivationPermit} from '../cloud/dist/managed-home-provisioning/compiler-qualification.js';
import {OriginalRuntimeRoleActivationPermit,assertOriginalRuntimeRoleActivationPermit} from '../cloud/dist/managed-home-provisioning/runtime-qualification.js';
const require=createRequire(new URL('../cloud/package.json',import.meta.url)),{WebSocket,WebSocketServer}=require('ws');
for(const [name,Type,check]of [['sdk',OriginalSdkRoleActivationPermit,assertOriginalSdkRoleActivationPermit],['runtime',OriginalRuntimeRoleActivationPermit,assertOriginalRuntimeRoleActivationPermit]]){
 test(name+' activation refuses row/JSON/prototype/symbol reconstruction',()=>{
  for(const value of [null,{},JSON.parse('{"format_version":1,"role":"'+name+'"}'),Object.create(Type.prototype)])assert.throws(()=>check(value));
  assert.throws(()=>Type.observe(Symbol('foreign issuer'),{},'00000000-0000-4000-8000-000000000001',{}));
  const forged=Object.create(Type.prototype);assert.throws(()=>forged.claimForAdminActivation());assert.throws(()=>JSON.stringify(forged));
 });
}
test('listener restriction preserves actual old WebSocket while next server binds SAME port',{timeout:5000},async()=>{
 const old=http.createServer(),wss=new WebSocketServer({server:old}),next=http.createServer((_q,r)=>{r.writeHead(200);r.end('next');});
 let client,accepted;
 try{
  const listening=once(old,'listening');old.listen(0,'127.0.0.1');await listening;const port=old.address().port;
  const arriving=once(wss,'connection');client=new WebSocket('ws://127.0.0.1:'+port);await once(client,'open');[accepted]=await arriving;
  const held=new StagingListenerAdmissionClosure(old);await held.closeRetaining();
  assert.equal(old.listening,false);assert.equal(client.readyState,WebSocket.OPEN);assert.equal(accepted.readyState,WebSocket.OPEN);
  assert.equal(held.statusData().existing_socket_drain_claim,false);
  const bound=once(next,'listening');next.listen(port,'127.0.0.1');await bound;
  const message=once(client,'message');accepted.send('original retained channel');assert.equal(String((await message)[0]),'original retained channel');
  assert.equal(await new Promise((resolve,reject)=>{http.get('http://127.0.0.1:'+port,r=>{let b='';r.on('data',v=>b+=v);r.on('end',()=>resolve(b));}).on('error',reject);}), 'next');
  assert.throws(()=>held.closeRetaining());assert.equal(client.readyState,WebSocket.OPEN);
 }finally{
  if(client&&client.readyState===WebSocket.OPEN){const ended=once(client,'close');client.close();await ended;}
  accepted?.close();await new Promise(resolve=>wss.close(resolve));
  if(next.listening)await new Promise(resolve=>next.close(resolve));
  if(old.listening)await new Promise(resolve=>old.close(resolve));
 }
});
