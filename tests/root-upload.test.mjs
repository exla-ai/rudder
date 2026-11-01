// Pure fake transport tests, UNRUN by author. No profile/provider/model access.
import test from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {Duplex} from 'node:stream';
import {createHash} from 'node:crypto';
import {validateScope,reference,missingAck,httpExchange,runTransfer} from '../cloud/worker/root-upload.mjs';
const scope={version:1,home_id:'test-home',workspace_id:'test-workspace',transfer_id:'11111111-1111-4111-8111-111111111111',request_nonce:'22222222-2222-4222-8222-222222222222',
  mode:'fresh',manifest:null,maximum_manifest_bytes:null,maximum_objects:128,maximum_bytes:4194304,maximum_object_bytes:1048576,constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'9007199254740993000'}};
const ref=bytes=>({kind:'blob',algorithm:'sha256',digest:createHash('sha256').update(bytes).digest('hex'),size_bytes:bytes.length});
const flush=()=>new Promise(resolve=>queueMicrotask(resolve));
test('scope separates original image300 from fresh30 and rejects authority extras',()=>{
  assert.deepEqual(validateScope(scope),scope);
  const image={...scope,mode:'image',manifest:ref(Buffer.from('manifest')),maximum_manifest_bytes:33554432,maximum_objects:100000,maximum_bytes:2147483648,maximum_object_bytes:4194304,constraint:{...scope.constraint,maximum_milliseconds:300000}};
  assert.equal(validateScope(image).mode,'image');
  assert.throws(()=>validateScope({...scope,constraint:image.constraint}));assert.throws(()=>validateScope({...scope,token:'NEVER_REAL'}));
  assert.throws(()=>validateScope({...scope,home_id:'../foreign'}));assert.throws(()=>validateScope({...scope,constraint:{...scope.constraint,boottime_deadline_nanoseconds:'18446744073709551616'}}));
});
test('reference framing preserves kind/raw SHA/size without wide integer rounding',()=>{
  const r=ref(Buffer.from('hello'));assert.equal(reference(r),`blob/${r.digest}/5`);
  for(const bad of [{...r,size_bytes:9007199254740992},{...r,digest:r.digest.toUpperCase()},{...r,kind:'checkpoint'},{...r,retained:true}])assert.throws(()=>reference(bad));
});
test('missing and verified preseed are disjoint exact supplied references',()=>{
  const a=ref(Buffer.from('a')),b=ref(Buffer.from('b'));const ack={format_version:1,transfer_id:scope.transfer_id,missing:[a],preseeded:[b]};
  assert.deepEqual(missingAck(ack,scope,[a,b]),ack);
  assert.throws(()=>missingAck({...ack,preseeded:[a]},scope,[a,b]));assert.throws(()=>missingAck({...ack,missing:[ref(Buffer.from('foreign'))]},scope,[a,b]));
});
function transport(status=200){
  let onResponse,finishDns,options,url,aborts=0;const req=new EventEmitter(),res=new EventEmitter(),sock=new EventEmitter();res.statusCode=status;
  req.write=(_b,callback)=>{queueMicrotask(()=>callback());return true;};req.end=()=>{};
  req.destroy=()=>{aborts++;};res.destroy=()=>{};sock.destroy=()=>{};
  const request=(u,o,cb)=>{url=u;options=o;onResponse=cb;queueMicrotask(()=>{req.emit('socket',sock);o.lookup('rudder-cloud-control.fly.dev',{},()=>{});});return req;};
  const lookup=(_h,_o,cb)=>{finishDns=()=>cb(null,'192.0.2.1',4);};
  const close=(body={ok:true})=>{onResponse(res);res.emit('data',Buffer.from(JSON.stringify(body)));res.emit('end');res.emit('close');req.emit('close');sock.emit('close');};
  return {req,res,sock,request,lookup,close,dns:()=>finishDns(),options:()=>options,url:()=>url,aborts:()=>aborts};
}
test('HTTP completion retains original DNS callback after all three close events',{timeout:1000},async()=>{
  const t=transport();let complete=false,abort;
  const p=httpExchange({path:'/api/rudder/home/test-home/fresh-roots/x/missing',method:'POST',length:2,contentType:'application/json',token:'SYNTHETIC_TEST_ONLY',check:()=>{},writer:async()=>{},register:a=>{abort=a;},request:t.request,lookup:t.lookup}).then(v=>{complete=true;return v;});
  await flush();t.close();await flush();assert.equal(complete,false);assert.equal(typeof abort,'function');t.dns();assert.deepEqual(await p,{ok:true});
  assert.equal(t.url().origin,'https://rudder-cloud-control.fly.dev');assert.equal(t.options().agent,false);assert.equal(t.options().rejectUnauthorized,true);
  assert.equal(t.options().headers.authorization,'Bearer SYNTHETIC_TEST_ONLY');assert.equal(t.options().headers['content-length'],'2');
});
test('redirect and timeout cancellation never retry or count destroy as settled',{timeout:1000},async()=>{
  const t=transport(302);let calls=0,abort,settled=false;
  const p=httpExchange({path:'/fixed',method:'PUT',length:0,contentType:'application/octet-stream',token:'SYNTHETIC',check:()=>{},writer:async()=>{},register:a=>{abort=a;},
    request:(...args)=>{calls++;return t.request(...args);},lookup:t.lookup}).finally(()=>{settled=true;});
  await flush();abort();await flush();assert.equal(settled,false);assert.equal(t.aborts(),1);t.close();t.dns();await assert.rejects(p);assert.equal(calls,1);
});
test('response bytes are bounded before parsing and error bodies never escape',{timeout:1000},async()=>{
  const t=transport();const p=httpExchange({path:'/fixed',method:'POST',length:0,contentType:'application/json',token:'SYNTHETIC',check:()=>{},writer:async()=>{},register:()=>{},request:t.request,lookup:t.lookup});
  await flush();t.close('x'.repeat(50000));t.dns();await assert.rejects(p,e=>!String(e).includes('xxxxx'));assert(t.aborts()>0);
});
class Paired extends Duplex {
  constructor(){super({allowHalfOpen:true,highWaterMark:16384});}
  _read(){}
  _write(b,_encoding,done){this.peer.push(Buffer.from(b));done();}
  _final(done){this.peer.push(null);done();}
}
function pair(){const a=new Paired(),b=new Paired();a.peer=b;b.peer=a;return [a,b];}
class Frames {
  constructor(stream){this.s=stream;this.it=stream[Symbol.asyncIterator]();this.b=Buffer.alloc(0);}
  async take(n){while(this.b.length<n){const x=await this.it.next();assert(!x.done);this.b=Buffer.concat([this.b,x.value]);}const b=this.b.subarray(0,n);this.b=this.b.subarray(n);return b;}
  async read(){return JSON.parse((await this.take((await this.take(4)).readUInt32BE())).toString());}
  send(v){const b=Buffer.from(JSON.stringify(v)),h=Buffer.alloc(4);h.writeUInt32BE(b.length);this.s.write(Buffer.concat([h,b]));}
}
test('actual stream protocol carries verified bytes and final EOF separately',{timeout:1000},async()=>{
  const [server,client]=pair(),f=new Frames(client),bytes=Buffer.from('actual private fixture bytes'),r=ref(bytes),calls=[];
  const exchange=async args=>{calls.push(args.path);const chunks=[];await args.writer({write:(b,cb)=>{chunks.push(Buffer.from(b));cb();}});if(args.method==='POST')return {format_version:1,transfer_id:scope.transfer_id,missing:[r],preseeded:[]};
    assert.deepEqual(Buffer.concat(chunks),bytes);return {format_version:1,transfer_id:scope.transfer_id,object:r,stored:true};};
  const run=runTransfer(server,scope,'SYNTHETIC',()=>{},exchange);assert.equal((await f.read()).type,'ready');
  f.send({type:'missing',objects:[r]});assert.equal((await f.read()).type,'missing_ack');f.send({type:'object',object:r});
  client.write(bytes.subarray(0,2));client.write(bytes.subarray(2));assert.equal((await f.read()).type,'object_ack');
  f.send({type:'finish'});client.end();assert.equal((await f.read()).type,'finished');
  // Duplex autoDestroy waits both halves. Consume the actual response EOF.
  assert.equal((await f.it.next()).done,true);client.destroy();server.destroy();const done=await run;
  assert.equal(done.source_eof,true);assert.equal(done.http_settled,true);assert.equal(calls.length,2);
});
test('hash mismatch fails before object end and cannot be promoted by a fake stored ACK',{timeout:1000},async()=>{
  const [server,client]=pair(),f=new Frames(client),r=ref(Buffer.from('good'));let put=0;
  const exchange=async args=>{if(args.method==='POST')return {format_version:1,transfer_id:scope.transfer_id,missing:[r],preseeded:[]};put++;
    await args.writer({write:(_b,cb)=>cb()});return {format_version:1,transfer_id:scope.transfer_id,object:r,stored:true};};
  const run=runTransfer(server,scope,'SYNTHETIC',()=>{},exchange);run.catch(()=>{});await f.read();f.send({type:'missing',objects:[r]});await f.read();f.send({type:'object',object:r});client.write(Buffer.from('evil'));
  await assert.rejects(run);assert.equal(put,1);client.destroy();server.destroy();
});
test('32MiB exception belongs only to the exact original image manifest',{timeout:1000},async()=>{
  const large={...ref(Buffer.from('metadata only')),size_bytes:5*1024*1024};
  const image={...scope,mode:'image',manifest:{...large,digest:'a'.repeat(64)},maximum_manifest_bytes:33554432,maximum_objects:100000,maximum_bytes:2147483648,maximum_object_bytes:4194304,
    constraint:{...scope.constraint,maximum_milliseconds:300000}};
  validateScope(image);const [server,client]=pair(),f=new Frames(client);let calls=0;
  const run=runTransfer(server,image,'SYNTHETIC',()=>{},async()=>{calls++;throw Error();});run.catch(()=>{});
  await f.read();f.send({type:'missing',objects:[large]});await assert.rejects(run);assert.equal(calls,0);client.destroy();server.destroy();
});
