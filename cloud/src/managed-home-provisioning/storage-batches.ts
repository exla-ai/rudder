/** Framing/immutable-CAS work only. No Context/registration/PG grant issuer. */
import type {IncomingMessage} from "node:http";
import {FabricCas,type FabricObjectRef,type FabricBatchStagingOwner,type FabricBatchAdmission} from "../fabric-cas.js";
import {RetainedFabricIO} from "../fabric-cas-retention.js";
import {check,exact,uuid,canonical,hash} from "./types.js";
import {storageReference} from "./storage-protocol.js";
export const BATCH_OBJECTS=1024,BATCH_BYTES=32*1024*1024,BATCH_HEADER=256*1024;
export interface ImageBatchHeader {format_version:1;transfer_id:string;request_nonce:string;batch_id:string;ordinal:number;objects:FabricObjectRef[];total_bytes:number;}
export interface ImageBatchAck {format_version:1;transfer_id:string;request_nonce:string;batch_id:string;ordinal:number;header_sha256:string;object_count:number;total_bytes:number;stored_bitmap:string;}
export const batchKey=(r:FabricObjectRef)=>`${r.kind}/${r.algorithm}/${r.digest}/${r.size_bytes}`;
export function batchHeader(value:unknown,manifest:FabricObjectRef):ImageBatchHeader{
 exact(value,["format_version","transfer_id","request_nonce","batch_id","ordinal","objects","total_bytes"]);const h=value as unknown as ImageBatchHeader;
 check(Object.keys(h).length===7&&h.format_version===1);uuid(h.transfer_id);uuid(h.request_nonce);uuid(h.batch_id);check(Number.isInteger(h.ordinal)&&h.ordinal>=0&&h.ordinal<100000&&Array.isArray(h.objects)&&h.objects.length>0&&h.objects.length<=BATCH_OBJECTS);
 let bytes=0,last=Buffer.alloc(0);for(const value of h.objects){const r=storageReference(value),key=Buffer.from(batchKey(r));check(Buffer.compare(last,key)<0,"conflict");last=key;
  const special=batchKey(r)===batchKey(manifest);check(r.size_bytes<=4*1024*1024||special&&r.size_bytes<=BATCH_BYTES&&h.objects.length===1);bytes+=r.size_bytes;check(Number.isSafeInteger(bytes)&&bytes<=BATCH_BYTES);}
 check(h.total_bytes===bytes&&canonical(h,BATCH_HEADER).length<=BATCH_HEADER);return h;
}
export class BatchReader {
 #iterator:AsyncIterator<any>;#pending=Buffer.alloc(0);#eof=false;
 constructor(private readonly request:IncomingMessage,private readonly current:()=>void){this.#iterator=request[Symbol.asyncIterator]();}
 async take(n:number):Promise<Buffer>{check(Number.isInteger(n)&&n>=0&&n<=16384);const out=Buffer.alloc(n);let at=0;while(at<n){this.current();if(!this.#pending.length){const next=await this.#iterator.next();check(!next.done,"invalid_request");this.#pending=Buffer.from(next.value);check(this.#pending.length<=65536);}
  const count=Math.min(n-at,this.#pending.length);this.#pending.copy(out,at,0,count);at+=count;this.#pending=this.#pending.subarray(count);}this.current();return out;}
 async header(manifest:FabricObjectRef){const n=(await this.take(4)).readUInt32BE();check(n>0&&n<=BATCH_HEADER);const chunks:Buffer[]=[];let at=0;while(at<n){const b=await this.take(Math.min(16384,n-at));chunks.push(b);at+=b.length;}const raw=Buffer.concat(chunks,n),h=batchHeader(JSON.parse(new TextDecoder("utf-8",{fatal:true}).decode(raw)),manifest);check(raw.equals(canonical(h,BATCH_HEADER)),"invalid_request");return {header:h,sha256:hash(raw)};}
 async eof(){this.current();check(this.#pending.length===0);const next=await this.#iterator.next();check(next.done,"invalid_request");this.#eof=true;this.current();}
 get actualEof(){return this.#eof;}
 stop(){this.request.destroy();}
}
interface ObjectWork {reference:FabricObjectRef;promise:Promise<void>;settled:boolean;stored?:boolean;failed:boolean;done:boolean;io:RetainedFabricIO;}
// Across all original batches; slots/declared spool released only by actual
// FabricCas promise settlement. Abort/EOF/timeout cannot release these counters.
let jobs=0,spool=0,active=0;
export class ImageBatchWork {
 #immutable?:{cas:FabricCas;admission:FabricBatchAdmission;owner:FabricBatchStagingOwner};
 readonly objects:ObjectWork[]=[];reader?:BatchReader;failed=false;finished=false;sourceEof=false;slot=false;request?:IncomingMessage;
 tryAcquire(){check(!this.slot);if(active>=2)return false;active++;this.slot=true;return true;}
 acquire(){check(this.tryAcquire(),"busy");}
 releaseIfSettled(){if(this.slot&&this.finished&&this.objects.every(o=>o.settled)){active--;this.slot=false;}}
 async run(reader:BatchReader,header:ImageBatchHeader,cas:FabricCas,lease:string,current:()=>void):Promise<boolean[]>{return this.runObjects(reader,header,cas,lease,current);}
 /** Caller has stored this actual work BEFORE the known committed Started ACK.
  * No DATA hold/lease-id overload may select the immutable path. */
 async runImmutable(reader:BatchReader,header:ImageBatchHeader,cas:FabricCas,owner:FabricBatchStagingOwner,current:()=>void):Promise<boolean[]>{
  check(!this.#immutable);const admission=cas.admitImmutableBatch(owner,{transfer_id:header.transfer_id,batch_id:header.batch_id,header_sha256:hash(canonical(header,262144)),objects:header.objects});
  this.#immutable={cas,admission,owner};
  return this.runObjects(reader,header,cas,admission,()=>{admission.assertCurrent();current();});
 }
 finishImmutable(){const original=this.#immutable;check(original&&this.sourceEof&&!this.failed&&this.objects.every(o=>o.settled&&!o.failed),"effect_unknown");original.cas.finishImmutableBatch(original.admission);}
 private async runObjects(reader:BatchReader,header:ImageBatchHeader,cas:FabricCas,lease:string|FabricBatchAdmission,current:()=>void):Promise<boolean[]>{
  this.reader=reader;try{for(const r of header.objects){while(jobs>=4||spool+r.size_bytes>64*1024*1024){current();check(!this.objects.some(o=>o.failed),"effect_unknown");await new Promise(resolve=>setTimeout(resolve,2));}current();check(!this.objects.some(o=>o.failed),"effect_unknown");
   let consumedResolve!:()=>void;const consumed=new Promise<void>(resolve=>{consumedResolve=resolve;});const item:ObjectWork={reference:r,promise:undefined as unknown as Promise<void>,settled:false,failed:false,done:false,io:new RetainedFabricIO()};this.objects.push(item);jobs++;spool+=r.size_bytes;
   const refresh=()=>{if(!item.settled&&item.done&&item.io.settled){item.settled=true;jobs--;spool-=r.size_bytes;this.releaseIfSettled();}};item.io.onProgress(refresh);
   item.promise=Promise.resolve().then(()=>item.io.run(async()=>{if(r.kind==="blob"){
     const body=async function*(){let remaining=r.size_bytes;while(remaining){current();const b=await reader.take(Math.min(16384,remaining));remaining-=b.length;yield b;}consumedResolve();};
     const ack=typeof lease==="string"?await cas.putBlobStream(lease,r,body()):await cas.putImmutableBatchBlob(lease,r,body());item.stored=ack.stored;current();
    }else{const parts:Buffer[]=[];let bytes=0;while(bytes<r.size_bytes){current();const b=await reader.take(Math.min(16384,r.size_bytes-bytes));parts.push(b);bytes+=b.length;}consumedResolve();const ack=typeof lease==="string"?await cas.putObject(lease,r,Buffer.concat(parts,bytes)):await cas.putImmutableBatchObject(lease,r,Buffer.concat(parts,bytes));item.stored=ack.stored;current();}
   })).catch(()=>{item.failed=true;this.failed=true;}).finally(()=>{item.done=true;consumedResolve();refresh();});
   await consumed;current();check(!item.failed,"effect_unknown");
  }
  await reader.eof();this.sourceEof=reader.actualEof;
  while(!this.objects.every(o=>o.settled)){current();await new Promise(resolve=>setTimeout(resolve,2));}
  current();check(!this.failed&&this.sourceEof&&this.objects.length===header.objects.length,"effect_unknown");return this.objects.map(o=>{check(typeof o.stored==="boolean","effect_unknown");return o.stored;});
  }catch{this.failed=true;this.#immutable?.owner.close();for(const o of this.objects)o.io.cancel();reader.stop();throw Error("IMAGE_BATCH_RETAINED");}finally{this.releaseIfSettled();}
 }
}
export function storedBitmap(values:readonly boolean[]):string{const out=Buffer.alloc(Math.ceil(values.length/8));values.forEach((v,i)=>{if(v)out[i>>3]|=1<<(i&7);});return out.toString("hex");}
