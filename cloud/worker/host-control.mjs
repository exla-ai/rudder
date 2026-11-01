// Fixed image-owned relay. Root alone authorizes each replacement generation.
// Original Root FD3/config FD4/token FD5/uptime FD6/bootstrap DATA FD7 only.
import fs from 'node:fs';
import {createHash,randomUUID} from 'node:crypto';
import WebSocket from './node_modules/ws/wrapper.mjs';
import {PacketQueue,sameGeneration,canonicalData} from './host-control-core.mjs';
const LIMIT=32768,TIMEOUT=20000;
// This finite mapping is image-owned. Only the actual protected FD4 config
// selects it; no environment, RPC, generation or caller URL can change it.
function controlPlane(origin){
 if(origin==='https://rudder-cloud-control.fly.dev')return 'wss://rudder-cloud-control.fly.dev';
 if(origin==='https://rudder-v4-qualification.fly.dev')return 'wss://rudder-v4-qualification.fly.dev';
 throw Error();
}
const plain=v=>v&&typeof v==='object'&&!Array.isArray(v);
const key=v=>typeof v==='string'&&/^[A-Za-z0-9_.:-]{1,128}$/.test(v);
const text=(v,max)=>typeof v==='string'&&v.length>0&&Buffer.byteLength(v)<=max&&!/[\x00-\x1f\x7f]/.test(v);
const exact=(v,names)=>plain(v)&&Object.keys(v).sort().join(',')===[...names].sort().join(',');
let fatal=false,timer,current,registered=false,registrationId;
const outgoing=new PacketQueue(8,LIMIT),retired=[],unforwarded=new Map();
const emit=value=>outgoing.push(value);
const flush=()=>outgoing.flush(bytes=>fs.writeSync(3,bytes,0,bytes.length));
const fail=()=>{if(fatal)return;fatal=true;process.exitCode=78;clearInterval(timer);try{current?.socket.terminate();}catch{}try{fs.closeSync(3);}catch{}};
function file(fd, max, mode, links) {
  const s = fs.fstatSync(fd);
  if (!s.isFile() || s.uid !== 0 || s.gid !== 0 || s.nlink !== links || (mode !== null && (s.mode & 0o7777) !== mode) || s.size < 1 || s.size > max) throw Error();
  const out = Buffer.alloc(s.size);
  if (fs.readSync(fd, out, 0, out.length, 0) !== out.length) throw Error();
  return out;
}
// Linux /proc/uptime prints CLOCK_BOOTTIME truncated to centiseconds. Use
// ONLY its lower endpoint to anchor the original deadline before parse/work;
// Root and native check the exact original constraint with CLOCK_BOOTTIME.
// This is never presented as a precise clock or a process-lifetime witness.
function lowerNs() {
  const bytes = Buffer.alloc(128); const n = fs.readSync(6, bytes, 0, bytes.length, 0);
  const match = /^(0|[1-9][0-9]*)\.([0-9]{2}) /.exec(bytes.subarray(0, n).toString('ascii'));
  if (!match) throw Error();
  return BigInt(match[1]) * 1000000000n + BigInt(match[2]) * 10000000n;
}
const expired = deadline => lowerNs() >= deadline;
function operation(method, params) {
  if (!plain(params)) throw Error();
  if (method === 'managed.receiptV1') {
    if (!exact(params, ['command_key', 'lookup_command_key']) || !key(params.lookup_command_key)) throw Error();
    return { op: 'ReceiptV1', lookup_command_key: params.lookup_command_key };
  }
  const task = method === 'managed.startTaskV1';
  if (!task && method !== 'managed.openPreparedProjectV1') return undefined;
  if (!exact(params, ['command_key', 'project_alias', 'backend', 'model', 'profile_id', ...(task ? ['text'] : [])])
      || !key(params.project_alias) || !['codex', 'claude'].includes(params.backend)
      || !text(params.model, 256) || !text(params.profile_id, 256)
      || (task && (typeof params.text !== 'string' || !params.text.trim() || Buffer.byteLength(params.text) > 24576 || params.text.includes('\0')))) throw Error();
  return { op: task ? 'StartTaskV1' : 'OpenPreparedProjectV1', project_alias: params.project_alias,
    backend: params.backend, model: params.model, profile_id: params.profile_id, ...(task ? { text: params.text } : {}) };
}
async function start(){
 if(process.argv.length!==2||Object.keys(process.env).length||process.getuid()!==1500||process.getgid()!==1500)throw Error();
 const raw=file(4,LIMIT,0o600,1),config=JSON.parse(raw.toString()),configSha=createHash('sha256').update(raw).digest('hex');raw.fill(0);fs.closeSync(4);
 const bootstrapRaw=file(7,4096,null,0),bootstrap=JSON.parse(bootstrapRaw.toString());bootstrapRaw.fill(0);fs.closeSync(7);
 const control=controlPlane(config.cloud_origin);
 if(config.version!==1||!key(config.home_id)||!key(config.incarnation)||bootstrap.version!==1||!text(bootstrap.nonce,36))throw Error();
 const tokenBytes=file(5,4096,0o600,1);if(tokenBytes.length<32||!tokenBytes.every(b=>b>=48&&b<=57||b>=65&&b<=90||b>=97&&b<=122||b===45||b===95))throw Error();const token=tokenBytes.toString('ascii');tokenBytes.fill(0);fs.closeSync(5);
 const cutoff=constraint=>{if(!exact(constraint,['format_version','maximum_milliseconds','boottime_deadline_nanoseconds'])||constraint.format_version!==1||!Number.isInteger(constraint.maximum_milliseconds)||constraint.maximum_milliseconds<1||constraint.maximum_milliseconds>5000||!/^[1-9][0-9]{0,19}$/.test(constraint.boottime_deadline_nanoseconds))throw Error();const end=BigInt(constraint.boottime_deadline_nanoseconds);if(expired(end))throw Error();return end;};
 const sendWs=(g,value)=>{const bytes=JSON.stringify(value);if(Buffer.byteLength(bytes)>LIMIT||g.socket.readyState!==WebSocket.OPEN||g.socket.bufferedAmount>4*LIMIT)throw Error();
  if(g.sends.size>=8)throw Error();const send={bytes,known:false,unknown:false};g.sends.add(send);
  try{g.socket.send(bytes,{binary:false},error=>{if(error){send.unknown=true;lost(g);}else{send.known=true;g.sends.delete(send);}});}catch{send.unknown=true;lost(g);throw Error();}};
 const lost=g=>{if(g.lost)return;g.lost=true;g.authorized=false;emit({kind:'host_transport_lost_v1',serial:g.serial,generation:g.generation,attempt_nonce:g.nonce});try{g.socket.terminate();}catch{}};
 const closed=g=>{if(g.closed)return;g.closed=true;lost(g);emit({kind:'host_transport_closed_v1',serial:g.serial,generation:g.generation,attempt_nonce:g.nonce});};
 const prune=()=>{for(let i=retired.length-1;i>=0;i--){const g=retired[i];if(g.closed&&!g.pending&&g.sends.size===0)retired.splice(i,1);}if(retired.length>=4)throw Error();};
 const ready=g=>emit({kind:'host_transport_ready_v1',version:1,serial:g.serial,generation:g.generation,attempt_nonce:g.nonce,home_id:config.home_id,incarnation:config.incarnation,config_sha256:configSha});
 const connect=(serial,generation,nonce,end,initial=false)=>{
  if(current){if(!current.closed)throw Error();prune();retired.push(current);}
  const g={serial,generation,nonce,end,initial,authorized:false,lost:false,closed:false,pending:null,sends:new Set(),history:[]};current=g;
  g.socket=new WebSocket(`${control}/api/rudder/home/${encodeURIComponent(config.home_id)}/control`,{headers:{authorization:`Bearer ${token}`},followRedirects:false,maxRedirects:0,perMessageDeflate:false,maxPayload:LIMIT,rejectUnauthorized:true,handshakeTimeout:Math.max(1,Math.min(30000,Number((end-lowerNs())/1000000n)))});
  g.socket.on('error',()=>{try{lost(g);}catch{fail();}});g.socket.on('close',()=>{try{closed(g);}catch{fail();}});
  g.socket.on('open',()=>{try{if(expired(g.end)){lost(g);return;}if(initial)emit({kind:'ready',version:1,session_generation:g.generation,nonce:bootstrap.nonce,home_id:config.home_id,incarnation:config.incarnation,config_sha256:configSha});else ready(g);flush();}catch{fail();}});
  g.socket.on('message',(raw,binary)=>{try{const began=lowerNs();if(fatal||binary||raw.length>LIMIT)throw Error();if(g!==current||g.closed||g.lost){if(g.history.length>=8)throw Error();g.history.push({kind:'late_original_ws_data',bytes:Buffer.from(raw)});return;}const m=JSON.parse(raw.toString());if(m?.type==='repos')return;
   if(['engine_registration_challenge_v1','engine_registration_ack_v1','engine_operation_ack_v1','host_transport_challenge_v1','host_transport_ack_v1'].includes(m?.type)){if(!exact(m,['type','rpc_id','packet'])||!key(m.rpc_id)||!plain(m.packet))throw Error();emit({kind:m.type,rpc_id:m.rpc_id,packet:m.packet});flush();return;}
   if(!exact(m,['type','id','method','params'])||m.type!=='rpc'||!key(m.id))throw Error();
   if(!registered||!g.authorized){sendWs(g,{type:'rpc-result',id:m.id,error:'MANAGED_TRANSPORT_AUTHENTICATING'});return;}
   if(g.pending||outgoing.length){sendWs(g,{type:'rpc-result',id:m.id,error:'MANAGED_BUSY'});return;}
   const op=operation(m.method,m.params);if(!op||!key(m.params.command_key)){sendWs(g,{type:'rpc-result',id:m.id,error:'MANAGED_METHOD_UNSUPPORTED'});return;}
   const pending={id:m.id,key:m.params.command_key,cutoff:began+BigInt(TIMEOUT)*1000000n,expired:false,reply:null};g.pending=pending;
   emit({version:1,session_generation:g.generation,rpc_id:pending.id,command_key:pending.key,operation:op,constraint:{format_version:1,maximum_milliseconds:TIMEOUT,boottime_deadline_nanoseconds:pending.cutoff.toString()}});flush();
  }catch{fail();}});
 };
 const initialEnd=BigInt(bootstrap.constraint.boottime_deadline_nanoseconds);if(expired(initialEnd))throw Error();connect('0',randomUUID(),bootstrap.nonce,initialEnd,true);
 timer=setInterval(()=>{try{if(fatal)return;flush();const bytes=Buffer.alloc(LIMIT+1);let n;try{n=fs.readSync(3,bytes,0,bytes.length,null);}catch(e){if(e?.code==='EAGAIN'||e?.code==='EWOULDBLOCK')return;throw e;}if(!n||n>LIMIT)throw Error();const m=JSON.parse(bytes.subarray(0,n).toString());
  if(m?.kind==='host_transport_connect_v1'){
   if(!exact(m,['kind','serial','generation','attempt_nonce','constraint'])||!key(m.generation)||!key(m.attempt_nonce)||!/^[1-9][0-9]{0,18}$/.test(m.serial))throw Error();const end=cutoff(m.constraint);
   if(current?.initial&&!current.authorized&&current.socket.readyState===WebSocket.OPEN){current.initial=false;current.serial=m.serial;current.generation=m.generation;current.nonce=m.attempt_nonce;current.end=end;ready(current);return;}
   connect(m.serial,m.generation,m.attempt_nonce,end);return;
  }
  if(m?.kind==='host_transport_close_v1'){if(!exact(m,['kind','serial','generation','attempt_nonce'])||!sameGeneration(current,m))throw Error();try{current.socket.terminate();}catch{}if(current.closed)closed(current);return;}
  if(m?.kind==='host_transport_activate_v1'){if(!exact(m,['kind','serial','generation','attempt_nonce','constraint'])||!sameGeneration(current,m)||current.lost||current.closed)throw Error();cutoff(m.constraint);current.authorized=true;registered=true;emit({kind:'host_transport_activated_v1',serial:current.serial,generation:current.generation,attempt_nonce:current.nonce});return;}
  if(['host_transport_begin_v1','host_transport_commit_v1','host_transport_confirm_v1','host_transport_reconcile_v1'].includes(m?.kind)){
   if(!exact(m,['kind','rpc_id','packet'])||!key(m.rpc_id)||!plain(m.packet)||!sameGeneration(current,m.packet.body))throw Error();sendWs(current,{type:m.kind,rpc_id:m.rpc_id,packet:m.packet});return;
  }
  if(['engine_registration_begin_v1','engine_registration_proof_v1'].includes(m?.kind)){if(registered||!exact(m,['kind','rpc_id','packet'])||!key(m.rpc_id)||!plain(m.packet)||expired(initialEnd))throw Error();if(registrationId&&registrationId!==m.rpc_id)throw Error();registrationId=m.rpc_id;sendWs(current,{type:m.kind,rpc_id:m.rpc_id,packet:m.packet});return;}
  if(m?.kind==='engine_registration_complete_v1'){if(!exact(m,['kind','challenge_id'])||!key(m.challenge_id)||!registrationId||registered)throw Error();registered=true;return;}
  if(m?.kind==='engine_operation_v1'){
   if(!registered||!exact(m,['kind','rpc_id','packet','transport'])||!key(m.rpc_id)||!plain(m.packet)||!plain(m.transport))throw Error();
   const body=m.packet.body,rootHash=createHash('sha256').update(JSON.stringify(canonicalData(body))).digest('hex');
   if(!current.authorized||current.closed||current.lost||current.socket.readyState!==WebSocket.OPEN||!sameGeneration(current,m.transport)||expired(BigInt(body.constraint.boottime_deadline_nanoseconds))){
    if(unforwarded.size>=2&&!unforwarded.has(m.rpc_id))throw Error();unforwarded.set(m.rpc_id,m);
    emit({kind:'engine_operation_not_forwarded_v1',rpc_id:m.rpc_id,request_sha256:rootHash,transport:m.transport});return;
   }
   const old=unforwarded.get(m.rpc_id);if(old){if(old.packet.body.payload_sha256!==body.payload_sha256||old.packet.body.command_key!==body.command_key||old.packet.body.request_nonce!==body.request_nonce||JSON.stringify(canonicalData(old.packet.body.constraint))!==JSON.stringify(canonicalData(body.constraint)))throw Error();unforwarded.delete(m.rpc_id);}
   // No code path replays a socket.send attempt. Only packets proven never to
   // reach this call may return to Root for SAME-budget remaining-time signing.
   sendWs(current,{type:m.kind,rpc_id:m.rpc_id,packet:m.packet});return;
  }
  const g=[current,...retired].find(g=>g.generation===m.session_generation&&g.pending?.id===m.rpc_id&&g.pending.key===m.command_key);if(!g||!exact(m,['version','session_generation','rpc_id','command_key','result','error'])||m.version!==1||((m.result!==null)===(m.error!==null)))throw Error();
  const errors={rejected:'MANAGED_REJECTED',busy:'MANAGED_BUSY',expired:'MANAGED_EXPIRED',effect_unknown:'MANAGED_EFFECT_UNKNOWN',internal:'MANAGED_INTERNAL'};if(m.error!==null&&!Object.hasOwn(errors,m.error))throw Error();
  g.pending.reply=m;g.history.push(g.pending);if(g.history.length>8)g.history.shift();
  if(!g.closed&&!g.lost&&!expired(g.pending.cutoff))sendWs(g,{type:'rpc-result',id:g.pending.id,...(m.error===null?{result:m.result}:{error:errors[m.error]})});
  // Original Native reply DATA settles this relay slot, not backend work or
  // Cloud delivery. Root/native receipt owners retain the original key.
  g.pending=null;prune();
 }catch{fail();}},20);
}
start().catch(fail);
