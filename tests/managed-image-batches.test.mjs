// UNRUN source definitions. Actual local streams/FabricCas, synthetic SDK only;
// no PostgreSQL, provider, physical Root, credentials or production qualification.
import test from 'node:test';import assert from 'node:assert/strict';
import {Readable,PassThrough} from 'node:stream';import {EventEmitter} from 'node:events';import {randomUUID,createHash} from 'node:crypto';
import {FabricCas,MemoryFabricObjectStorage,S3FabricObjectStorage} from '../cloud/dist/fabric-cas.js';
import {RetainedFabricIO} from '../cloud/dist/fabric-cas-retention.js';
import {BatchReader,ImageBatchWork,batchHeader,batchKey,storedBitmap} from '../cloud/dist/managed-home-provisioning/storage-batches.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {validateStorageOperation,isImageOperation} from '../cloud/dist/managed-home-provisioning/storage-protocol.js';
const digest=b=>createHash('sha256').update(b).digest('hex'),ref=b=>({kind:'blob',algorithm:'sha256',digest:digest(b),size_bytes:b.length});
const deferred=()=>{let resolve;const promise=new Promise(r=>resolve=r);return {promise,resolve};};
const immediate=()=>new Promise(resolve=>setImmediate(resolve));
async function until(f){for(let n=0;n<10000;n++){if(f())return;await immediate();}throw Error('local bounded observation failed');}
function header(objects){return {format_version:1,transfer_id:randomUUID(),request_nonce:randomUUID(),batch_id:randomUUID(),ordinal:0,objects:[...objects].sort((a,b)=>Buffer.compare(Buffer.from(batchKey(a)),Buffer.from(batchKey(b)))),total_bytes:objects.reduce((n,r)=>n+r.size_bytes,0)};}
function wire(h,bodies){const raw=canonical(h,262144),length=Buffer.alloc(4);length.writeUInt32BE(raw.length);return Buffer.concat([length,raw,...h.objects.map(r=>bodies.get(r.digest))]);}
test('V2 is explicit image-only metadata; V1/fresh and original quotas stay distinct',()=>{const b=ref(Buffer.from('manifest')),d={...b,kind:'directory'};
 for(const op of ['BeginImagePreseedV1','BeginImagePreseedV2']){const v={op,workspace_id:'project',manifest:b,installer_input:d,image_receipt_sha256:'a'.repeat(64)};assert.equal(validateStorageOperation(v).op,op);assert.equal(isImageOperation(v),true);assert.throws(()=>validateStorageOperation({...v,retained:true}));}
 assert.equal(isImageOperation({op:'BeginFreshRootsV1'}),false);
});
test('batch header refuses duplicates, unsorted refs, unknown fields and widened manifest exceptions',()=>{const a=ref(Buffer.from('a')),b=ref(Buffer.from('b')),h=header([a,b]);assert.deepEqual(batchHeader(h,a),h);
 for(const mutate of [v=>v.objects.reverse(),v=>v.objects[1]=v.objects[0],v=>v.total_bytes++,v=>v.objects=Array(1025).fill(a),v=>v.context={},v=>v.batch_id='later',v=>v.ordinal=100000]){const bad=structuredClone(h);mutate(bad);assert.throws(()=>batchHeader(bad,a));}
 const large={...a,size_bytes:33554432};assert.deepEqual(batchHeader(header([large]),large).objects,[large]);assert.throws(()=>batchHeader(header([large,b]),large));assert.throws(()=>batchHeader(header([{...large,digest:'f'.repeat(64)}]),large));
});
test('actual split byte framing binds canonical header, exact sizes, EOF and trailing data',async()=>{const bytes=Buffer.from('payload'),r=ref(bytes),h=header([r]),body=wire(h,new Map([[r.digest,bytes]]));
 const reader=new BatchReader(Readable.from([...body].map(b=>Buffer.from([b]))),()=>{});const parsed=await reader.header(r);assert.equal(parsed.sha256,hash(canonical(h,262144)));assert.deepEqual(await reader.take(bytes.length),bytes);await reader.eof();assert.equal(reader.actualEof,true);
 for(const suffix of [Buffer.from('trailing')]){const bad=new BatchReader(Readable.from([body,suffix]),()=>{});await bad.header(r);await bad.take(bytes.length);await assert.rejects(bad.eof());}
 const partial=new BatchReader(Readable.from([body.subarray(0,body.length-1)]),()=>{});await partial.header(r);await assert.rejects(partial.take(bytes.length));
});
test('actual FabricCas batch never exceeds four pending writes and ACK waits for their actual promises',async()=>{const storage=new MemoryFabricObjectStorage(),original=storage.putIfAbsent.bind(storage),gate=deferred();let active=0,peak=0,entered=0;
 storage.putIfAbsent=async(key,bytes)=>{if(!key.includes('/objects/'))return original(key,bytes);entered++;active++;peak=Math.max(peak,active);try{await gate.promise;return await original(key,bytes);}finally{active--;}};
 const cas=new FabricCas({storage,accountId:'batch-fixture'}),stage=await cas.beginStaging(),items=Array.from({length:7},(_,i)=>Buffer.from('blob-'+i)),h=header(items.map(ref)),bodies=new Map(items.map(b=>[digest(b),b])),reader=new BatchReader(Readable.from([wire(h,bodies)]),()=>{});await reader.header(h.objects[0]);const work=new ImageBatchWork();work.acquire();let done=false;
 const pending=work.run(reader,h,cas,stage.lease_id,()=>{}).then(result=>{done=true;return result;});try{await until(()=>entered===4);assert.equal(done,false);assert.equal(peak,4);assert.equal(work.slot,true);}finally{gate.resolve();}
 const flags=await pending;assert.equal(flags.length,7);assert.equal(done,true);assert.equal(work.slot,true);work.finished=true;work.releaseIfSettled();assert.equal(work.slot,false);assert.equal(peak,4);assert(work.objects.every(o=>o.settled));assert.equal(storedBitmap(flags),'7f');
});
test('cancelled batch retains actual pending SDK work and slots until the original work settles',async()=>{const storage=new MemoryFabricObjectStorage(),original=storage.putIfAbsent.bind(storage),gate=deferred();let entered=false,cancelled=false;
 storage.putIfAbsent=async(key,bytes)=>{if(key.includes('/objects/')){entered=true;await gate.promise;}return original(key,bytes);};const cas=new FabricCas({storage,accountId:'retained-fixture'}),stage=await cas.beginStaging(),bytes=Buffer.from('retained'),r=ref(bytes),h=header([r]),reader=new BatchReader(Readable.from([wire(h,new Map([[r.digest,bytes]]))]),()=>{if(cancelled)throw Error('fixture original cancelled');});await reader.header(r);const work=new ImageBatchWork();work.acquire();
 const pending=work.run(reader,h,cas,stage.lease_id,()=>{if(cancelled)throw Error('fixture original cancelled');});await until(()=>entered);cancelled=true;await assert.rejects(pending);work.finished=true;work.releaseIfSettled();assert.equal(work.slot,true);assert.equal(work.objects[0].settled,false);gate.resolve();await until(()=>work.objects[0].settled);assert.equal(work.slot,false);assert.equal(work.failed,true);
});
test('batch-only SDK mode aborts at timeout but cannot return/release before original send settlement',async()=>{const gate=deferred(),aborted=deferred();let entered=false;const storage=new S3FabricObjectStorage({bucket:'synthetic-fixture',timeoutMs:1,client:{send:async(command,{abortSignal})=>{entered=true;abortSignal.addEventListener('abort',()=>aborted.resolve(),{once:true});await gate.promise;return {};}}});
 const io=new RetainedFabricIO();let done=false;const actual=io.run(()=>storage.putIfAbsent('fixture/object',Buffer.from('x'))).then(()=>{done=true;},e=>{done=true;throw e;});await until(()=>entered);await aborted.promise;assert.equal(done,false);assert.equal(io.settled,false);gate.resolve();await assert.rejects(actual,{code:'storage_timeout'});assert.equal(io.settled,true);
});
test('original stream close remains separately mandatory after request promise success',async()=>{const io=new RetainedFabricIO(),body=new EventEmitter();io.stream(body);await io.run(async()=>{const p=Promise.resolve('known');io.request(p);await p;});assert.equal(io.settled,false);body.emit('close');assert.equal(io.settled,true);
});
