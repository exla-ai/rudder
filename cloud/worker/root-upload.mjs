// Image-owned one-shot transfer. FD3 anonymous STREAM; FD4 sealed scope DATA;
// FD5 Root-opened per-home bearer; FD6 original procfs uptime; FD7 SAME held
// protected config. No env/argv key or caller-selected destination.
import fs from 'node:fs';
import net from 'node:net';
import https from 'node:https';
import dns from 'node:dns';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { pathToFileURL } from 'node:url';

const ORIGIN='https://rudder-cloud-control.fly.dev', QUALIFICATION_ORIGIN='https://rudder-v4-qualification.fly.dev', FRAME=32768, CHUNK=16384, RESPONSE=49152;
const key=x=>typeof x==='string'&&/^[A-Za-z0-9_.:-]{1,128}$/.test(x);
const uuid=x=>typeof x==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(x);
const exact=(x,n)=>x&&typeof x==='object'&&!Array.isArray(x)&&Object.keys(x).sort().join(',')===[...n].sort().join(',');
// DATA validation only. Production gets config exclusively from actual Root
// FD7; scope/framing never carries an origin, token, config path or override.
export function uploadControlOrigin(config,scope){
  if(!config||config.version!==1||![config.machine_id,config.store_id,config.incarnation,config.account_id,config.home_id].every(key)
    ||config.home_id!==scope.home_id||!Array.isArray(config.projects)||config.projects.length<1||config.projects.length>8
    ||!config.projects.every(p=>p&&key(p.alias))||!config.projects.some(p=>p.alias===scope.workspace_id)
    ||![ORIGIN,QUALIFICATION_ORIGIN].includes(config.cloud_origin))throw Error();
  return config.cloud_origin;
}
export function reference(x) {
  if(!exact(x,['kind','algorithm','digest','size_bytes'])||!['blob','file','directory','directory_shard','source_root','environment_root'].includes(x.kind)
    ||x.algorithm!=='sha256'||typeof x.digest!=='string'||!/^[a-f0-9]{64}$/.test(x.digest)||!Number.isSafeInteger(x.size_bytes)||x.size_bytes<0||x.size_bytes>33554432)throw Error();
  return `${x.kind}/${x.digest}/${x.size_bytes}`;
}
export function validateScope(s) {
  if(!exact(s,['version','home_id','workspace_id','transfer_id','request_nonce','mode','maximum_objects','maximum_bytes','maximum_object_bytes','constraint','manifest','maximum_manifest_bytes'])
    ||s.version!==(s.mode==='image_batch_v1'?2:1)||!key(s.home_id)||!key(s.workspace_id)||!uuid(s.transfer_id)||!uuid(s.request_nonce))throw Error();
  const image=s.mode==='image'||s.mode==='image_batch_v1';
  if(!image&&s.mode!=='fresh'||s.maximum_objects!==(image?100000:128)||s.maximum_bytes!==(image?2147483648:4194304)||s.maximum_object_bytes!==(image?4194304:1048576))throw Error();
  if(image){if(!s.manifest||reference(s.manifest).split('/')[0]!=='blob'||s.maximum_manifest_bytes!==33554432)throw Error();}
  else if(s.manifest!==null||s.maximum_manifest_bytes!==null)throw Error();
  const c=s.constraint;
  if(!exact(c,['format_version','maximum_milliseconds','boottime_deadline_nanoseconds'])||c.format_version!==1||!Number.isInteger(c.maximum_milliseconds)
    ||c.maximum_milliseconds<1||c.maximum_milliseconds>(image?300000:30000)||typeof c.boottime_deadline_nanoseconds!=='string'
    ||!/^[1-9][0-9]{0,19}$/.test(c.boottime_deadline_nanoseconds)||BigInt(c.boottime_deadline_nanoseconds)>18446744073709551615n)throw Error();
  return s;
}
export function batchHeader(value,scope) {
  if(scope.version!==2||scope.mode!=='image_batch_v1')throw Error();
  if(!exact(value,['format_version','transfer_id','request_nonce','batch_id','ordinal','objects','total_bytes'])||value.format_version!==1||value.transfer_id!==scope.transfer_id||value.request_nonce!==scope.request_nonce||!uuid(value.batch_id)||!Number.isInteger(value.ordinal)||value.ordinal<0||value.ordinal>=100000||!Array.isArray(value.objects)||!value.objects.length||value.objects.length>1024)throw Error();
  let total=0,last=Buffer.alloc(0);for(const r of value.objects){reference(r);const k=Buffer.from(batchReference(r));if(Buffer.compare(last,k)>=0||r.size_bytes>4194304&&!(reference(r)===reference(scope.manifest)&&r.size_bytes<=33554432&&value.objects.length===1))throw Error();last=k;total+=r.size_bytes;}
  if(total!==value.total_bytes||total>33554432||Buffer.byteLength(canonicalBatch(value))>262144)throw Error();return value;
}
export const batchReference=r=>`${r.kind}/${r.algorithm}/${r.digest}/${r.size_bytes}`;
export const canonicalBatch=v=>JSON.stringify(Array.isArray(v)?v.map(x=>JSON.parse(canonicalBatch(x))):v&&typeof v==='object'?Object.fromEntries(Object.keys(v).sort().map(k=>[k,JSON.parse(canonicalBatch(v[k]))])):v);
export function batchAck(a,h,digest){if(!exact(a,['format_version','transfer_id','request_nonce','batch_id','ordinal','header_sha256','object_count','total_bytes','stored_bitmap'])||a.format_version!==1||a.transfer_id!==h.transfer_id||a.request_nonce!==h.request_nonce||a.batch_id!==h.batch_id||a.ordinal!==h.ordinal||a.header_sha256!==digest||a.object_count!==h.objects.length||a.total_bytes!==h.total_bytes||typeof a.stored_bitmap!=='string')throw Error();const b=Buffer.from(a.stored_bitmap,'hex');if(b.toString('hex')!==a.stored_bitmap||b.length!==Math.ceil(a.object_count/8)||a.object_count%8&&b.at(-1)>>(a.object_count%8))throw Error();return a;}

export function batchReconciliation(value,h,digest) {
  if(exact(value,['format_version','scope','ack'])&&value.format_version===1&&value.scope==='stored-original-image-batch-ack-data-only'){batchAck(value.ack,h,digest);return value;}
  if(!exact(value,['format_version','scope','transfer_id','batch_id','header_sha256','actual_known_objects','actual_unsettled_objects'])||value.format_version!==1||value.scope!=='image-batch-unknown-data-only'||value.transfer_id!==h.transfer_id||value.batch_id!==h.batch_id||value.header_sha256!==digest)throw Error();
  for(const n of [value.actual_known_objects,value.actual_unsettled_objects])if(n!==null&&(!Number.isSafeInteger(n)||n<0||n>h.objects.length))throw Error();return value;
}

export function missingAck(value,scope,objects) {
  if(!exact(value,['format_version','transfer_id','missing','preseeded'])||value.format_version!==1||value.transfer_id!==scope.transfer_id
    ||!Array.isArray(value.missing)||!Array.isArray(value.preseeded))throw Error();
  const supplied=new Set(objects.map(reference)),seen=new Set();
  for(const r of [...value.missing,...value.preseeded]){const id=reference(r);if(!supplied.has(id)||seen.has(id))throw Error();seen.add(id);}
  return value;
}
function file(fd,max,mode,links) {
  const s=fs.fstatSync(fd);if(!s.isFile()||s.uid!==0||s.gid!==0||s.nlink!==links||(mode!==null&&(s.mode&0o7777)!==mode)||s.size<1||s.size>max)throw Error();
  const b=Buffer.alloc(s.size);if(fs.readSync(fd,b,0,b.length,0)!==b.length)throw Error();return b;
}
function clock() {
  let last=0n;
  return ()=>{const b=Buffer.alloc(128),n=fs.readSync(6,b,0,b.length,0),m=/^(0|[1-9][0-9]*)\.([0-9]{2}) /.exec(b.subarray(0,n).toString('ascii'));
    if(!m)throw Error();const latest=BigInt(m[1])*1000000000n+BigInt(m[2])*10000000n+10000000n;
    if(latest<last)throw Error();last=latest;return latest;}; // conservative upper endpoint, never a precise clock claim
}
class Reader {
  // Receiving source EOF must leave the reply half open for the final receipt.
  constructor(socket){this.socket=socket;this.iterator=socket.iterator({destroyOnReturn:false});this.pending=Buffer.alloc(0);}
  async take(n,allowEof=false){if(n>FRAME)throw Error();const out=Buffer.alloc(n);let at=0;
    while(at<n){if(!this.pending.length){const x=await this.iterator.next();if(x.done){if(allowEof&&at===0)return null;throw Error();}this.pending=x.value;if(this.pending.length>65536)throw Error();}
      const k=Math.min(n-at,this.pending.length);this.pending.copy(out,at,0,k);at+=k;this.pending=this.pending.subarray(k);}return out;}
  async frame(){const h=await this.take(4),n=h.readUInt32BE();if(n<1||n>FRAME)throw Error();return JSON.parse((await this.take(n)).toString('utf8'));}
}
async function write(socket,bytes){await new Promise((resolve,reject)=>socket.write(bytes,e=>e?reject(Error()):resolve()));}
async function frame(socket,value){const b=Buffer.from(JSON.stringify(value));if(b.length>FRAME)throw Error();const h=Buffer.alloc(4);h.writeUInt32BE(b.length);await write(socket,Buffer.concat([h,b]));}

// Resolves only after actual request/response/socket closes AND the original
// custom DNS callback returns. destroy/AbortError/timeout alone is not settlement.
// Public component-test seam remains production-bound. The executable selects
// a different finite destination only from its original protected Root FD7.
export function httpExchange(args){return httpExchangeAtOrigin(ORIGIN,args);}
function httpExchangeAtOrigin(origin,{path,method,length,contentType,token,check,writer,register,request=https.request,lookup=dns.lookup}) {
  return new Promise((resolve,reject)=>{
    let req,res,sock,requestClosed=false,responseClosed=true,socketClosed=true,dnsPending=0,started=false,error=false,status,body=[],bytes=0,settled=false,writerSettled=true;
    const complete=()=>{if(!started||settled||!requestClosed||!responseClosed||!socketClosed||dnsPending||!writerSettled)return;settled=true;
      if(error||status!==200)return reject(Error());try{resolve(JSON.parse(Buffer.concat(body).toString('utf8')));}catch{reject(Error());}};
    const abort=()=>{error=true;try{req?.destroy();}catch{}try{res?.destroy();}catch{}try{sock?.destroy();}catch{}complete();};
    register(abort);
    try{
      check();
      if(![ORIGIN,QUALIFICATION_ORIGIN].includes(origin))throw Error();
      const target=new URL(path,origin);if(target.origin!==origin||!path.startsWith('/')||path.startsWith('//')||target.username||target.password||target.hash||target.search)throw Error();
      req=request(target,{method,agent:false,rejectUnauthorized:true,maxHeaderSize:8192,
        headers:{authorization:`Bearer ${token}`,'content-type':contentType,'content-length':String(length)},
        lookup:(hostname,options,callback)=>{dnsPending++;let called=false;const done=(...args)=>{if(called)return;called=true;dnsPending--;callback(...args);complete();};
          try{lookup(hostname,options,done);}catch(e){done(e);}},
      },r=>{res=r;status=r.statusCode;responseClosed=false;r.on('data',b=>{bytes+=b.length;if(bytes>RESPONSE)return abort();body.push(b);});r.on('error',abort);
        r.on('aborted',abort);r.on('end',()=>{try{check();}catch{abort();}});r.on('close',()=>{responseClosed=true;complete();});});
      started=true;
      req.on('socket',s=>{sock=s;socketClosed=false;s.once('close',()=>{socketClosed=true;complete();});});
      req.on('error',()=>{error=true;});req.once('close',()=>{requestClosed=true;complete();});
      writerSettled=false;
      Promise.resolve().then(()=>writer(req)).then(()=>{check();req.end();},()=>{error=true;abort();})
        .catch(()=>{error=true;abort();}).finally(()=>{writerSettled=true;complete();});
    }catch{error=true;started=true;if(!req)requestClosed=true;abort();}
  });
}
export async function runTransfer(socket,scope,token,check,exchange=httpExchange) {
  const reader=new Reader(socket),base=`/api/rudder/home/${scope.home_id}/fresh-roots/${scope.transfer_id}`;
  const queried=new Set(),missing=new Set(),uploaded=new Set();let count=0,total=0,activeAbort,batchOrdinal=0,lastBatchKey=Buffer.alloc(0);const batchMode=scope.mode==='image_batch_v1',batchIds=new Set();
  const call=(method,path,length,contentType,writer)=>exchange({path,method,length,contentType,token,check,writer,register:a=>{activeAbort=a;}}).finally(()=>{activeAbort=undefined;});
  const cancel=()=>{try{activeAbort?.();}catch{}socket.destroy();};
  const timer=setInterval(()=>{try{check();}catch{cancel();}},20);
  try {
    check();await frame(socket,{type:'ready',format_version:1,transfer_id:scope.transfer_id,request_nonce:scope.request_nonce});
    for(;;){check();const m=await reader.frame();
      if(batchMode&&exact(m,['type','header_length','header_sha256'])&&m.type==='batch'){
        if(!Number.isInteger(m.header_length)||m.header_length<1||m.header_length>262144||typeof m.header_sha256!=='string'||!/^[a-f0-9]{64}$/.test(m.header_sha256))throw Error();
        const parts=[];let at=0;while(at<m.header_length){check();const b=await reader.take(Math.min(CHUNK,m.header_length-at));check();parts.push(b);at+=b.length;}const raw=Buffer.concat(parts,m.header_length),h=batchHeader(JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(raw)),scope);
        if(raw.toString()!==canonicalBatch(h)||createHash('sha256').update(raw).digest('hex')!==m.header_sha256||h.ordinal!==batchOrdinal||batchIds.has(h.batch_id)||Buffer.compare(lastBatchKey,Buffer.from(batchReference(h.objects[0])))>=0||count+h.objects.length>scope.maximum_objects||total+h.total_bytes>scope.maximum_bytes)throw Error();
        batchIds.add(h.batch_id);const header=Buffer.alloc(4);header.writeUInt32BE(raw.length);let readBytes=0,knownAck=false;
        try{const a=await call('PUT',base+'/batches/'+h.batch_id,4+raw.length+h.total_bytes,'application/vnd.rudder.image-batch-v1',async req=>{check();await write(req,header);check();for(let at=0;at<raw.length;at+=CHUNK){check();await write(req,raw.subarray(at,Math.min(at+CHUNK,raw.length)));check();}for(const r of h.objects){const hash=createHash('sha256');let remaining=r.size_bytes;while(remaining){check();const b=await reader.take(Math.min(CHUNK,remaining));check();remaining-=b.length;readBytes+=b.length;hash.update(b);await write(req,b);check();}if(hash.digest('hex')!==r.digest)throw Error();}check();});
          batchAck(a,h,m.header_sha256);knownAck=true;check();await frame(socket,{type:'batch_ack',...a});batchOrdinal++;lastBatchKey=Buffer.from(batchReference(h.objects.at(-1)));count+=h.objects.length;total+=h.total_bytes;
        }catch{let reconciliation=null;
          // The failed exchange has ACTUALLY settled before it rejects. This
          // optional same-ID inspection never retries PUT or invents an ACK.
          if(!knownAck&&readBytes===h.total_bytes)try{check();const b=Buffer.from(JSON.stringify({format_version:1,request_nonce:h.request_nonce,header_sha256:m.header_sha256}));const v=await call('POST',base+'/batches/'+h.batch_id+'/reconcile',b.length,'application/json',req=>write(req,b));
            check();reconciliation=batchReconciliation(v,h,m.header_sha256);
          }catch{}await frame(socket,{type:'batch_unknown',format_version:1,transfer_id:h.transfer_id,request_nonce:h.request_nonce,batch_id:h.batch_id,header_sha256:m.header_sha256,reconciliation});throw Error();}
      }else if(!batchMode&&exact(m,['type','objects'])&&m.type==='missing'){
        if(!Array.isArray(m.objects)||m.objects.length<1||m.objects.length>128)throw Error();
        for(const r of m.objects){const id=reference(r);if(r.size_bytes>scope.maximum_object_bytes&&!(scope.mode==='image'&&id===reference(scope.manifest)&&r.size_bytes<=scope.maximum_manifest_bytes))throw Error();if(queried.has(id)||queried.size>=100000)throw Error();queried.add(id);}
        const b=Buffer.from(JSON.stringify({format_version:1,objects:m.objects}));
        const a=missingAck(await call('POST',base+'/missing',b.length,'application/json',req=>write(req,b)),scope,m.objects);
        for(const r of a.missing)missing.add(reference(r));check();await frame(socket,{type:'missing_ack',...a});
      }else if(!batchMode&&exact(m,['type','object'])&&m.type==='object'){
        const r=m.object,id=reference(r);if(!missing.delete(id)||uploaded.has(id)||(r.size_bytes>scope.maximum_object_bytes&&!(scope.mode==='image'&&id===reference(scope.manifest)&&r.size_bytes<=scope.maximum_manifest_bytes))||++count>scope.maximum_objects)throw Error();
        total+=r.size_bytes;if(total>scope.maximum_bytes)throw Error();uploaded.add(id);const hash=createHash('sha256');
        const a=await call('PUT',base+'/objects/'+id,r.size_bytes,'application/octet-stream',async req=>{
          let remaining=r.size_bytes;while(remaining){check();const b=await reader.take(Math.min(CHUNK,remaining));hash.update(b);await write(req,b);remaining-=b.length;}
          if(hash.digest('hex')!==r.digest)throw Error();check();});
        if(!exact(a,['format_version','transfer_id','object','stored'])||a.format_version!==1||a.transfer_id!==scope.transfer_id||reference(a.object)!==id||typeof a.stored!=='boolean')throw Error();
        check();await frame(socket,{type:'object_ack',...a});
      }else if(exact(m,['type'])&&m.type==='finish'){
        if(missing.size||batchMode&&batchOrdinal===0)throw Error();if(await reader.take(1,true)!==null)throw Error();check();
        const receipt={format_version:1,transfer_id:scope.transfer_id,request_nonce:scope.request_nonce,http_settled:true,source_eof:true};
        await frame(socket,{type:'finished',...receipt});await new Promise((resolve,reject)=>socket.end(e=>e?reject(Error()):resolve()));
        if(!socket.closed)await once(socket,'close');return receipt;
      }else throw Error();
    }
  }catch{cancel();throw Error();}finally{clearInterval(timer);}
}
async function main() {
  let tokenBytes;
  try{
    if(process.argv.length!==2||Object.keys(process.env).length||process.getuid()!==1500||process.getgid()!==1500||!fs.fstatSync(3).isSocket()||fs.fstatfsSync(6).type!==0x9fa0)throw Error();
    const raw=file(4,FRAME,null,0),scope=validateScope(JSON.parse(raw.toString('utf8')));raw.fill(0);fs.closeSync(4);
    // Original config is read and bound BEFORE even reading the private token.
    const configRaw=file(7,FRAME,0o600,1);let origin;
    try{origin=uploadControlOrigin(JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(configRaw)),scope);}finally{configRaw.fill(0);fs.closeSync(7);}
    const exchange=args=>httpExchangeAtOrigin(origin,args);
    const now=clock(),end=BigInt(scope.constraint.boottime_deadline_nanoseconds);const check=()=>{if(now()>=end)throw Error();};check();
    tokenBytes=file(5,4096,0o600,1);if(tokenBytes.length<32||!tokenBytes.every(b=>b>=48&&b<=57||b>=65&&b<=90||b>=97&&b<=122||b===45||b===95))throw Error();
    const token=tokenBytes.toString('ascii');tokenBytes.fill(0);fs.closeSync(5);
    const socket=new net.Socket({fd:3,readable:true,writable:true,allowHalfOpen:true,highWaterMark:CHUNK});socket.on('error',()=>{});
    const receipt=await runTransfer(socket,scope,token,check,exchange);check();
    await new Promise((resolve,reject)=>process.stdout.write(JSON.stringify(receipt)+'\n',e=>e?reject(Error()):resolve()));
  }catch{tokenBytes?.fill(0);process.exitCode=78;await new Promise(resolve=>process.stderr.write('ROOT_UPLOAD_FAILED\n',resolve));}
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href)await main();
