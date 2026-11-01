// Actual fixed-script evaluation with synthetic FDs/HTTP/WS only, UNRUN.
// These fixtures are not a protected FD issuer, Root/native or TLS/PG proof.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {EventEmitter,once} from 'node:events';
import {PacketQueue,sameGeneration,canonicalData} from '../cloud/worker/host-control-core.mjs';
import {RuntimeQualificationRelay,READY as QUAL_READY,ACK as QUAL_ACK} from '../cloud/worker/runtime-qualification.mjs';
const production='https://rudder-cloud-control.fly.dev',qualification='https://rudder-v4-qualification.fly.dev';
const privateToken='FIXTURE_PRIVATE_TOKEN_'.repeat(3),nonce='11111111-1111-4111-8111-111111111111';
const scope={version:1,home_id:'home',workspace_id:'project',transfer_id:'22222222-2222-4222-8222-222222222222',request_nonce:'33333333-3333-4333-8333-333333333333',mode:'fresh',manifest:null,maximum_manifest_bytes:null,maximum_objects:128,maximum_bytes:4194304,maximum_object_bytes:1048576,constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:'130000000000'}};
const config=origin=>({version:1,machine_id:'machine',store_id:'store',incarnation:'incarnation',account_id:'account',home_id:'home',cloud_origin:origin,projects:[{alias:'project'}]});
const uploadSource=readFileSync(new URL('../cloud/worker/root-upload.mjs',import.meta.url),'utf8')
  .replace(/^import .*;\n/gm,'').replace(/^export /gm,'').replace(/^if\(process.argv\[1\].*\n?/m,'');
async function uploader({origin=qualification,patch={},filePatch={},env={}}={}){
  const reads=[],closed=[],requests=[],out=[],err=[],files=new Map([[4,Buffer.from(JSON.stringify(scope))],[5,Buffer.from(privateToken)],[7,Buffer.from(JSON.stringify({...config(origin),...patch}))]]);
  let constructed=0,seenScope;
  const fs={fstatSync:fd=>fd===3?{isSocket:()=>true}:{isFile:()=>true,uid:0,gid:0,nlink:fd===4?0:1,mode:0o100600,size:files.get(fd)?.length,...(fd===7?filePatch:{})},
    fstatfsSync:()=>({type:0x9fa0}),readSync:(fd,b)=>{reads.push(fd);const bytes=fd===6?Buffer.from('100.00 9.00\n'):files.get(fd);if(!bytes)throw Error();bytes.copy(b);return bytes.length;},closeSync:fd=>closed.push(fd)};
  const process={argv:['node','root-upload.mjs'],env,getuid:()=>1500,getgid:()=>1500,exitCode:0,stdout:{write:(s,cb)=>{out.push(s);cb();}},stderr:{write:(s,cb)=>{err.push(s);cb();}}};
  const https={request:(url,options,callback)=>{requests.push({url,options});const req=new EventEmitter(),res=new EventEmitter(),socket=new EventEmitter();res.statusCode=200;
    req.write=(_b,cb)=>cb();req.destroy=()=>{};res.destroy=()=>{};socket.destroy=()=>{};
    req.end=()=>queueMicrotask(()=>{callback(res);res.emit('data',Buffer.from('{"ok":true}'));res.emit('end');res.emit('close');req.emit('close');socket.emit('close');});
    queueMicrotask(()=>{req.emit('socket',socket);options.lookup(url.hostname,{},()=>{});});return req;}};
  const transfer=async(_s,s,token,check,exchange)=>{seenScope=s;assert.equal(token,privateToken);check();await exchange({path:`/api/rudder/home/${s.home_id}/fresh-roots/${s.transfer_id}/missing`,method:'POST',length:2,contentType:'application/json',token,check,writer:r=>r.write(Buffer.from('{}'),()=>{}),register:()=>{}});
    return {format_version:1,transfer_id:s.transfer_id,request_nonce:s.request_nonce,http_settled:true,source_eof:true};};
  const invoke=new Function('fs','net','https','dns','createHash','once','process','transfer',uploadSource+'\nrunTransfer=transfer;return main;');
  await invoke(fs,{Socket:class extends EventEmitter{constructor(){super();constructed++;}}},https,{lookup:(_h,_o,cb)=>cb(null,'192.0.2.1',4)},createHash,once,process,transfer)();
  return {reads,closed,requests,out,err,process,constructed,seenScope};
}
test('actual uploader main uses SAME protected config FD7 before token and selects finite HTTPS',async()=>{
  for(const origin of [production,qualification]){const f=await uploader({origin});assert.equal(f.process.exitCode,0);assert.equal(f.requests.length,1);
    assert.equal(f.requests[0].url.origin,origin);assert.equal(f.requests[0].options.rejectUnauthorized,true);assert.equal(f.requests[0].options.agent,false);
    assert.equal(f.requests[0].options.headers.authorization,'Bearer '+privateToken);assert(f.reads.indexOf(7)<f.reads.indexOf(5));assert(f.closed.includes(7));assert(f.closed.includes(5));
    assert.deepEqual(f.seenScope,scope);assert(!Object.hasOwn(f.seenScope,'cloud_origin'));assert(!f.out.join('').includes(privateToken));assert.equal(f.err.length,0);
  }
});
test('wrong config identity/origin/descriptor refuses before reading private token or network',async()=>{
  const cases=[{origin:qualification+'/'},{origin:'https://rudder-v4-qualification.fly.dev:443'},{origin:'https://rudder-v4-qualification.fly.dev?x=1'},
    {origin:'https://user:pass@rudder-v4-qualification.fly.dev'},{origin:'http://rudder-v4-qualification.fly.dev'},
    {patch:{home_id:'foreign'}},{patch:{projects:[{alias:'foreign'}]}},{filePatch:{uid:1500}},{filePatch:{gid:1500}},
    {filePatch:{nlink:2}},{filePatch:{mode:0o100644}},{filePatch:{isFile:()=>false}},{env:{RUDDER_CONTROL_ORIGIN:qualification}}];
  for(const c of cases){const f=await uploader(c);assert.equal(f.process.exitCode,78);assert.equal(f.requests.length,0);assert.equal(f.constructed,0);assert(!f.reads.includes(5));assert.equal(f.out.length,0);assert.deepEqual(f.err,['ROOT_UPLOAD_FAILED\n']);}
});

const sidecarSource=readFileSync(new URL('../cloud/worker/host-control.mjs',import.meta.url),'utf8').replace(/^import .*;\n/gm,'');
async function sidecar(origin,env={}){
  const reads=[],ipc=[],sockets=[];const files=new Map([[4,Buffer.from(JSON.stringify(config(origin)))],[5,Buffer.from(privateToken)],[7,Buffer.from(JSON.stringify({version:1,nonce,constraint:scope.constraint}))]]);
  const fs={fstatSync:fd=>({isFile:()=>true,uid:0,gid:0,nlink:fd===7?0:1,mode:0o100600,size:files.get(fd).length}),
    readSync:(fd,b)=>{reads.push(fd);if(fd===3)throw Object.assign(Error(),{code:'EAGAIN'});const v=fd===6?Buffer.from('100.00 9.00\n'):files.get(fd);v.copy(b);return v.length;},
    writeSync:(_fd,b)=>{ipc.push(JSON.parse(b));return b.length;},closeSync:()=>{}};
  const process={argv:['node','host-control.mjs'],env,getuid:()=>1500,getgid:()=>1500,exitCode:0};
  class Socket{static OPEN=1;readyState=1;bufferedAmount=0;handlers={};constructor(url,options){this.url=url;this.options=options;sockets.push(this);}on(e,f){this.handlers[e]=f;}terminate(){}send(_b,_o,cb){cb();}}
  new Function('fs','createHash','randomUUID','WebSocket','process','setInterval','clearInterval','PacketQueue','sameGeneration','canonicalData','RuntimeQualificationRelay','QUAL_READY','QUAL_ACK',sidecarSource)(fs,createHash,()=>nonce,Socket,process,()=>1,()=>{},PacketQueue,sameGeneration,canonicalData,RuntimeQualificationRelay,QUAL_READY,QUAL_ACK);
  await Promise.resolve();if(sockets.length)sockets[0].handlers.open();return {reads,ipc,sockets,process};
}
test('actual sidecar selects only exact WSS peer and keeps bearer outside native messages',async()=>{
  for(const origin of [production,qualification]){const f=await sidecar(origin);assert.equal(f.process.exitCode,0);assert.equal(f.sockets.length,1);const s=f.sockets[0];
    assert.equal(s.url,origin.replace('https:','wss:')+'/api/rudder/home/home/control');assert.equal(s.options.followRedirects,false);assert.equal(s.options.maxRedirects,0);assert.equal(s.options.rejectUnauthorized,true);
    assert.equal(s.options.headers.authorization,'Bearer '+privateToken);assert.equal(f.ipc[0].config_sha256,createHash('sha256').update(JSON.stringify(config(origin))).digest('hex'));assert(!JSON.stringify(f.ipc).includes(privateToken));
  }
});
test('sidecar destination and environment rejection precede private token read',async()=>{
  for(const origin of [qualification+'/',qualification+':443',qualification+'/path',qualification+'.evil.invalid','wss://rudder-v4-qualification.fly.dev','https://rudder.exla.ai']){const f=await sidecar(origin);assert.equal(f.process.exitCode,78);assert.equal(f.sockets.length,0);assert(!f.reads.includes(5));assert.equal(f.ipc.length,0);}
  const f=await sidecar(qualification,{RUDDER_CONTROL_ORIGIN:qualification});assert.equal(f.process.exitCode,78);assert.equal(f.sockets.length,0);assert(!f.reads.includes(5));
});
