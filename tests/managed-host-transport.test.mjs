// UNRUN. Real production HMAC/session producer with explicitly synthetic PG
// rows/Root key; actual loopback WS lifecycle. No Root/kernel/PG enablement.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import http from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
import {createHmac,randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {RootHostTransports,transportMac} from '../cloud/dist/managed-home-provisioning/transport.js';
import {TRANSPORT_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/transport-schema.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {PacketQueue,sameGeneration,canonicalData} from '../cloud/worker/host-control-core.mjs';
const require=createRequire(new URL('../cloud/package.json',import.meta.url)),{WebSocket,WebSocketServer}=require('ws');
const f=JSON.parse(await fs.readFile(new URL('../native/tests/fixtures/engine_registration/hmac-vectors.json',import.meta.url),'utf8'));
const registration=f.vectors.find(v=>v.phase==='ack').body,rootKey=Buffer.alloc(32,11);
const credential=Buffer.from('rdre_'+createHmac('sha256',rootKey).update('rudder_engine_registration_credential_v1\0').update(canonical(Object.fromEntries(['challenge_id','engine_instance_id','credential_id','root_nonce'].map(k=>[k,registration[k]])))).digest('hex'));
const home={intent:{accountId:registration.binding.account_id,homeId:registration.binding.home_id,nonce:registration.binding.provision_nonce}};
function fixture(){let rows=new Map(),live=true;const pool={run:async(_account,work)=>{const before=structuredClone(rows);try{return {status:'committed',value:await work({query:async(sql,a)=>{
 if(sql.includes('SELECT state,migration_sha256'))return {rows:[{state:'ready',migration_sha256:TRANSPORT_MIGRATION_SHA256}]};
 if(sql.includes('SELECT r.ack_json,r.challenge_json'))return {rows:live?[{ack_json:{body:registration},challenge_json:{body:registration},token_sha256:hash(credential)}]:[]};
 if(sql.includes('SELECT r.ack_json,c.token_sha256')){const row=rows.get(a[3]);return {rows:live&&row?.state==='committed'&&row.generation===a[4]?[{ack_json:{body:registration},token_sha256:hash(credential)}]:[]};}
 if(sql.includes('SELECT count(*)'))return {rows:[{n:String([...rows.values()].filter(r=>r.state==='pending').length)}]};
 if(sql.startsWith('SELECT serial::text')){const row=[...rows.values()].sort((a,b)=>BigInt(a.serial)>BigInt(b.serial)?-1:1)[0];return {rows:row?[{...row,live:performance.now()<row.end}]:[]};}
 if(sql.startsWith('INSERT INTO')){assert(!rows.has(a[5]));rows.set(a[5],{serial:a[5],generation:a[6],request:JSON.parse(a[9]),challenge:JSON.parse(a[10]),state:'pending',end:performance.now()+a[11]});}
 if(sql.includes("SET state='superseded'")){for(const row of rows.values())if(row.state==='committed')row.state='superseded';}
 if(sql.includes("SET state='committed'")){const row=rows.get(a[3]);row.state='committed';row.ack=JSON.parse(a[5]);}
 return {rows:[],rowCount:1};}})};}catch(e){rows=before;throw e;}}};
 return {api:new RootHostTransports(pool,randomUUID()),rows:()=>rows,revoke:()=>{live=false;}};}
function request(serial=1,ms=1000){const body={version:1,kind:'host_transport_begin',registration,serial:String(serial),generation:randomUUID(),attempt_nonce:randomUUID(),constraint:{format_version:1,maximum_milliseconds:ms,boottime_deadline_nanoseconds:'9999999999999'},remaining_milliseconds:ms};return {body,mac:transportMac(credential,'begin',body)};}
function commit(challenge){const body={...challenge.body,kind:'host_transport_commit',native_acceptance_sha256:'a'.repeat(64)};return {body,mac:transportMac(credential,'commit',body)};}
async function activate(api,socket,serial){const begin=request(serial),challenge=await api.begin(home,rootKey,begin,socket,originalTime(1000)),done=await api.commit(home,rootKey,commit(challenge),socket);return {begin,challenge,...done};}

test('known EAGAIN retains exact original bytes; unknown write is retained and never replayed',()=>{const q=new PacketQueue(2,128);q.push({nonce:'original'});const original=q.snapshot()[0];assert.equal(q.flush(()=>{throw Object.assign(Error(),{code:'EAGAIN'});}),false);assert.deepEqual(q.snapshot()[0],original);assert.equal(q.unknown,0);assert.throws(()=>q.flush(()=>1));assert.equal(q.unknown,1);assert.equal(q.length,0);assert.equal(q.flush(()=>{throw Error('must not replay');}),false);assert.deepEqual(q.snapshot()[0],original);});
test('foreign generation cannot match a retained nonce and original root-authenticated socket',async()=>{const {api,rows}=fixture(),one={},foreign={},begin=request();const challenge=await api.begin(home,rootKey,begin,one,originalTime(1000));await assert.rejects(api.commit(home,rootKey,commit(challenge),foreign));assert.equal(rows().get('1').state,'pending');assert(!sameGeneration({serial:'1',generation:'a',nonce:'b'},{serial:'1',generation:'foreign',attempt_nonce:'b'}));});
test('stale exact MAC/ACK cannot activate an earlier serial after a newer pending challenge',async()=>{const {api,rows}=fixture(),one={},two={};const old=await api.begin(home,rootKey,request(1),one,originalTime(1000));await api.begin(home,rootKey,request(2),two,originalTime(1000));await assert.rejects(api.commit(home,rootKey,commit(old),one));assert.equal(rows().get('1').state,'pending');assert.equal(api.currentSocket(home.intent.homeId,one),false);});
test('original auth deadline cannot be renewed by a late commit or new readback clock',async()=>{const {api}=fixture(),socket={},begin=request(1,100);const challenge=await api.begin(home,rootKey,begin,socket,originalTime(100));await new Promise(r=>setTimeout(r,180));await assert.rejects(api.commit(home,rootKey,commit(challenge),socket));await assert.rejects(api.begin(home,rootKey,begin,socket,originalTime(5000)));assert.equal(api.currentSocket(home.intent.homeId,socket),false);});
test('sidecar no-forward hash uses actual Cloud canonical request with wide string counters',()=>{const body=f.vectors.find(v=>v.phase==='operation_request').body;assert.deepEqual(Buffer.from(JSON.stringify(canonicalData(body))),canonical(body));});
test('wrong MAC and same-MAC foreign generation cannot replace the original socket',async()=>{const {api}=fixture(),socket={},bad=request();bad.mac='f'.repeat(64);await assert.rejects(api.begin(home,rootKey,bad,socket,originalTime(1000)));const challenge=await api.begin(home,rootKey,request(),socket,originalTime(1000)),changed=commit(challenge);changed.body.generation=randomUUID();await assert.rejects(api.commit(home,rootKey,changed,socket));assert.equal(api.currentSocket(home.intent.homeId,socket),false);});
test('known confirmed closed generations recycle active memory rather than imposing a lifetime reconnect cap',async()=>{const {api}=fixture();for(let serial=1;serial<=40;serial++){const socket={},active=await activate(api,socket,serial);const a=active.packet.body,body={version:1,kind:'host_transport_confirm',registration,serial:a.serial,generation:a.generation,attempt_nonce:a.attempt_nonce,commit_sha256:a.commit_sha256};await api.confirm(home,rootKey,{body,mac:transportMac(credential,'confirm',body)});api.closed(socket);}assert.equal(api.currentSocket(home.intent.homeId,{}),false);});
test('actual original WS disconnect refuses old traffic and preserves a separately authenticated replacement',async()=>{const {api}=fixture(),server=http.createServer(),ws=new WebSocketServer({server});const sockets=[];ws.on('connection',socket=>sockets.push(socket));await new Promise(r=>server.listen(0,'127.0.0.1',r));const url='ws://127.0.0.1:'+server.address().port;
 let first,second;try{first=new WebSocket(url);await once(first,'open');while(sockets.length<1)await new Promise(r=>setTimeout(r,1));const old=sockets[0];await activate(api,old,1);old.once('close',()=>api.closed(old));const closed=once(old,'close');first.close(1000,'actual fixture disconnect');await closed;assert.equal(api.currentSocket(home.intent.homeId,old),false);
  second=new WebSocket(url);await once(second,'open');while(sockets.length<2)await new Promise(r=>setTimeout(r,1));const next=sockets[1],active=await activate(api,next,2);await active.owner.assertCurrent();api.closed(old);assert(api.currentSocket(home.intent.homeId,next));await active.owner.assertCurrent();assert.throws(()=>JSON.stringify(active.owner));
 }finally{first?.terminate();second?.terminate();for(const socket of sockets)socket.terminate();await new Promise(r=>ws.close(r));await new Promise(r=>server.close(r));}});
