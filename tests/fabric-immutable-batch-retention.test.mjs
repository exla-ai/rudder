// UNRUN. Real FabricCas/streams/spools and original carrier; synthetic S3 SDK.
// No actual AWS policy qualification, PostgreSQL, Root, model or timing claim.
import test,{after} from 'node:test';
import {createRequire} from 'node:module';
import assert from 'node:assert/strict';
import {createHash,randomUUID} from 'node:crypto';
import {Readable} from 'node:stream';
import {FabricCas,S3FabricObjectStorage,FabricBatchStagingOwner,FabricBatchAdmission,canonicalFabricMetadata} from '../cloud/dist/fabric-cas.js';
import {RetainedFabricIO} from '../cloud/dist/fabric-cas-retention.js';
import {BatchReader,ImageBatchWork,batchKey} from '../cloud/dist/managed-home-provisioning/storage-batches.js';
const sha=b=>createHash('sha256').update(b).digest('hex');
const ref=(bytes,kind='blob')=>({kind,algorithm:'sha256',digest:sha(bytes),size_bytes:bytes.length});
const deferred=()=>{let resolve;const promise=new Promise(r=>{resolve=r;});return {promise,resolve};};
async function until(f){for(let n=0;n<10000;n++){if(f())return;await new Promise(r=>setImmediate(r));}throw Error('bounded fixture observation failed');}
const aws=(name,status)=>Object.assign(new Error('synthetic storage error'),{name,$metadata:{httpStatusCode:status}});
// Patch ONLY this isolated test process's SDK method; no credential provider,
// token file, network handler or external request is ever invoked by the mock.
const requireCloud=createRequire(new URL('../cloud/package.json',import.meta.url)),{S3Client}=requireCloud('@aws-sdk/client-s3'),services=new Map(),clients=new Set(),originalSend=S3Client.prototype.send;
S3Client.prototype.send=function(command,options){clients.add(this);const f=services.get(command.input.Bucket);if(!f)throw Error('No external SDK calls in fixture');return f(command,options);};
after(()=>{S3Client.prototype.send=originalSend;for(const c of clients)c.destroy();});
const roleArn='arn:aws:iam::123456789012:role/rudder-fabric-fixture';
function policy(bucket){const root='arn:aws:s3:::'+bucket,resources=['objects/*','roots/*','batch-holds-v1/*'].map(p=>root+'/fabric/v1/accounts/*/'+p);
 return {Version:'2012-10-17',Statement:[
  {Effect:'Deny',Principal:'*',Action:['s3:DeleteObject','s3:DeleteObjectVersion'],Resource:resources,Condition:{ArnEquals:{'aws:PrincipalArn':roleArn}}},
  {Effect:'Deny',Principal:'*',Action:'s3:PutObject',Resource:resources,Condition:{Null:{'s3:if-none-match':'true'},Bool:{'s3:ObjectCreationOperation':'true'}}},
  {Effect:'Deny',Principal:{AWS:roleArn},Action:['s3:PutLifecycleConfiguration','s3:PutBucketVersioning','s3:PutBucketPolicy','s3:DeleteBucketPolicy','s3:DeleteBucket'],Resource:root}
 ]};}
function fixture(){const bucket='fixture-retained-'+randomUUID().replaceAll('-',''),objects=new Map(),calls=[],state={now:1700000000000,policy:policy(bucket),versioning:'Enabled',rules:[],onPut:undefined,onSend:undefined};
 const end=state.now+300000,original={remaining(){const left=end-state.now;if(left<=0)throw Error('original fixture expired');return left;}};
 const client={send:async(command,{abortSignal})=>{const name=command.constructor.name,i=command.input;calls.push({name,key:i.Key});if(state.onSend)await state.onSend(name,abortSignal);
  if(name==='GetBucketVersioningCommand')return {Status:state.versioning};
  if(name==='GetBucketLifecycleConfigurationCommand')return {Rules:state.rules};
  if(name==='GetBucketPolicyCommand')return {Policy:JSON.stringify(state.policy)};
  if(name==='PutObjectCommand'){
   if(state.onPut)await state.onPut(i,abortSignal);
   if(i.IfNoneMatch==='*'&&objects.has(i.Key))throw aws('PreconditionFailed',412);
   if(i.IfMatch&&(!objects.has(i.Key)||sha(objects.get(i.Key))!==i.IfMatch))throw aws('PreconditionFailed',412);
   const chunks=[];if(Buffer.isBuffer(i.Body))chunks.push(i.Body);else for await(const b of i.Body)chunks.push(Buffer.from(b));
   const bytes=Buffer.concat(chunks);assert.equal(bytes.length,i.ContentLength);assert.equal(sha(bytes),Buffer.from(i.ChecksumSHA256,'base64').toString('hex'));objects.set(i.Key,bytes);return {};
  }
  if(name==='GetObjectCommand'){if(!objects.has(i.Key))throw aws('NoSuchKey',404);const b=objects.get(i.Key);return {Body:Readable.from([b]),ETag:sha(b),ContentLength:b.length};}
  if(name==='DeleteObjectCommand'){objects.delete(i.Key);return {};}
  throw Error('unexpected synthetic command');
 }};
 services.set(bucket,client.send);const storage=S3FabricObjectStorage.fromWebIdentityForImmutableBatches({bucket,region:'us-east-1',roleArn,webIdentityTokenFile:'/workspace/THIS_FIXTURE_NEVER_READS_A_TOKEN',timeoutMs:1000,maxConcurrency:16}),cas=new FabricCas({storage,accountId:'batch-retention-fixture',now:()=>state.now});
 return {bucket,objects,calls,state,original,storage,cas,end};
}
const roots=()=>({manifest:ref(Buffer.from('manifest-fixture')),installer_input:ref(canonicalFabricMetadata({format_version:1,mode:493,entries:[]}),'directory')});
async function ready(f){await f.storage.qualifyImmutableBatchRetentionRetaining(f.original);const owner=f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots());await f.cas.beginImmutableBatchStaging(owner);return owner;}
function wire(owner,items){const h={format_version:1,transfer_id:owner.data().transfer_id,request_nonce:randomUUID(),batch_id:randomUUID(),ordinal:0,objects:items.map(i=>i.ref).sort((a,b)=>Buffer.compare(Buffer.from(batchKey(a)),Buffer.from(batchKey(b)))),total_bytes:items.reduce((n,i)=>n+i.bytes.length,0)},raw=canonicalFabricMetadata(h),size=Buffer.alloc(4);size.writeUInt32BE(raw.length);const by=new Map(items.map(i=>[i.ref.digest,i.bytes]));return {h,bytes:Buffer.concat([size,raw,...h.objects.map(r=>by.get(r.digest))])};}
async function batch(f,owner,items){const {h,bytes}=wire(owner,items),reader=new BatchReader(Readable.from([bytes]),()=>owner.assertCurrent());await reader.header(items[0].ref);const work=new ImageBatchWork();work.acquire();
 try{const values=await work.runImmutable(reader,h,f.cas,owner,()=>owner.assertCurrent());work.finishImmutable();return {work,values};}finally{work.finished=true;work.releaseIfSettled();}}

test('new retention is off until SAME actual adapter readback; DATA and forged constructors cannot grant it',async()=>{
 const f=fixture();const unbound=new S3FabricObjectStorage({bucket:f.bucket,client:{send:()=>{throw Error('must not dispatch');}}});await assert.rejects(unbound.qualifyImmutableBatchRetentionRetaining(f.original),{code:'batch_retention_unqualified'});assert.throws(()=>f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots()),{code:'batch_retention_unqualified'});assert.equal(f.calls.length,0);
 assert.throws(()=>new FabricBatchStagingOwner(Symbol(),{}),{code:'invalid_batch_hold'});assert.throws(()=>new FabricBatchAdmission(Symbol(),{}),{code:'invalid_batch_hold'});
 const owner=await ready(f);assert.equal(f.storage.batchRetentionStatus().state,'qualified');assert.throws(()=>JSON.stringify(owner),{code:'invalid_batch_hold'});
 const other=new FabricCas({storage:f.storage,accountId:'another-account',now:()=>f.state.now});assert.throws(()=>other.assertBatchStagingCurrent(owner),{code:'batch_closed'});
 const saved=JSON.parse(JSON.stringify(owner.data()));assert.throws(()=>f.cas.assertBatchStagingCurrent(saved),{code:'invalid_batch_hold'});owner.assertCurrent();
});
test('versioning/lifecycle/workload deletion/conditional-write/config-control exceptions fail actual readback',async()=>{
 for(const change of [f=>f.state.versioning='Suspended',f=>f.state.rules=[{Status:'Enabled'}],f=>f.state.rules=[{Status:'Disabled'}],f=>f.state.policy.Statement[0].Principal={AWS:'role-only'},f=>f.state.policy.Statement[0].Condition={Bool:{'aws:SecureTransport':'false'}},f=>f.state.policy.Statement[0].Action=['s3:DeleteObject'],f=>f.state.policy.Statement[1].Condition={},f=>f.state.policy.Statement[2].Principal={AWS:roleArn+'-foreign'},f=>f.state.policy.Statement[2].Principal='*',f=>f.state.policy.Statement.pop()]){
  const f=fixture();change(f);await assert.rejects(f.storage.qualifyImmutableBatchRetentionRetaining(f.original),{code:'batch_retention_unqualified'});assert.equal(f.storage.batchRetentionStatus().state,'failed');assert.throws(()=>f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots()));assert.equal(f.calls.filter(c=>c.name==='PutObjectCommand').length,0);
 }
});
test('one original hold PUT amortizes ALL per-object lease GETs; blobs and typed metadata stay verified',async()=>{
 const f=fixture(),owner=await ready(f),dir=canonicalFabricMetadata({format_version:1,mode:493,entries:[]}),items=[...Array.from({length:6},(_,n)=>{const bytes=Buffer.from('blob-'+n);return {bytes,ref:ref(bytes)};}),{bytes:dir,ref:ref(dir,'directory')}];
 const before=f.calls.length,{values}=await batch(f,owner,items),after=f.calls.slice(before);assert.equal(values.length,7);assert(values.every(Boolean));assert.equal(after.filter(c=>c.name==='PutObjectCommand').length,7);assert.equal(after.filter(c=>c.name==='GetObjectCommand').length,0);
 assert.equal(f.calls.filter(c=>c.key?.includes('/batch-holds-v1/')&&c.name==='PutObjectCommand').length,1);assert.equal(owner.status().durable_hold_acknowledged,true);
});
test('ordinary V1 abandon/renew namespace cannot delete or extend immutable retention',async()=>{
 const f=fixture(),owner=await ready(f),hold=owner.data(),key=[...f.objects.keys()].find(k=>k.includes('/batch-holds-v1/')),before=Buffer.from(f.objects.get(key));
 await f.cas.abandonStaging(hold.hold_id);assert.deepEqual(f.objects.get(key),before);await assert.rejects(f.cas.renewStaging(hold.hold_id),{code:'lease_missing'});
 await assert.rejects(f.storage.delete(key),{code:'immutable_batch_hold'});await assert.rejects(f.storage.replaceIfMatch(key,sha(before),before),{code:'immutable_batch_hold'});owner.assertCurrent();
 const legacy=await f.cas.beginStaging(),bytes=Buffer.from('legacy');const at=f.calls.length;await f.cas.putBlobStream(legacy.lease_id,ref(bytes),Readable.from([bytes]));assert.equal(f.calls.slice(at).filter(c=>c.name==='GetObjectCommand').length,2);await f.cas.abandonStaging(legacy.lease_id);
});
test('original expiry before consume or after final source chunk dispatches no object PUT and never renews',async()=>{
 for(const atEnd of [false,true]){const f=fixture(),owner=await ready(f),bytes=Buffer.from('expiry'),r=ref(bytes),admission=f.cas.admitImmutableBatch(owner,{transfer_id:owner.data().transfer_id,batch_id:randomUUID(),header_sha256:'a'.repeat(64),objects:[r]}),io=new RetainedFabricIO();
  const body=async function*(){yield bytes;if(atEnd)f.state.now=f.end;};if(!atEnd)f.state.now=f.end;
  const before=f.calls.length;await assert.rejects(io.run(()=>f.cas.putImmutableBatchBlob(admission,r,body())));assert.equal(f.calls.slice(before).filter(c=>c.name==='PutObjectCommand').length,0);assert.equal(owner.data().expires_at_ms,f.end);assert.equal(owner.status().admission_closed,true);assert.equal(f.objects.size,1);
 }
});
test('an expiry in the SDK dispatch boundary cannot rely on an earlier successful spool check',async()=>{
 const f=fixture(),owner=await ready(f),bytes=Buffer.from('queued-dispatch'),r=ref(bytes),admission=f.cas.admitImmutableBatch(owner,{transfer_id:owner.data().transfer_id,batch_id:randomUUID(),header_sha256:'c'.repeat(64),objects:[r]}),io=new RetainedFabricIO();
 const original=f.storage.putFileIfAbsent.bind(f.storage);f.storage.putFileIfAbsent=async(...args)=>{f.state.now=f.end;return original(...args);};const before=f.calls.length;
 await assert.rejects(io.run(()=>f.cas.putImmutableBatchBlob(admission,r,Readable.from([bytes]))));assert.equal(f.calls.slice(before).filter(c=>c.name==='PutObjectCommand').length,0);assert.equal(owner.status().admission_closed,true);
});
test('qualification timeout abort does not dispose its original SDK request or qualify a late policy',async()=>{
 const f=fixture(),entered=deferred(),gate=deferred(),aborted=deferred();f.state.onSend=async(name,signal)=>{if(name==='GetBucketPolicyCommand'){signal.addEventListener('abort',()=>aborted.resolve(),{once:true});entered.resolve();await gate.promise;}};
 let settled=false;const actual=f.storage.qualifyImmutableBatchRetentionRetaining({remaining:()=>20}).finally(()=>{settled=true;});await entered.promise;await aborted.promise;
 assert.equal(settled,false);assert.equal(f.storage.batchRetentionStatus().state,'failed');assert.equal(f.storage.batchRetentionStatus().io_settled,false);gate.resolve();await assert.rejects(actual,{code:'batch_retention_unqualified'});assert.equal(f.storage.batchRetentionStatus().io_settled,true);assert.throws(()=>f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots()));
});
test('whole hash/typed-edge failures and immutable conflicts never turn into a successful batch',async()=>{
 const f=fixture(),owner=await ready(f),expected=Buffer.from('correct'),wrong=Buffer.from('corrupt'),r=ref(expected),admission=f.cas.admitImmutableBatch(owner,{transfer_id:owner.data().transfer_id,batch_id:randomUUID(),header_sha256:'b'.repeat(64),objects:[r]}),io=new RetainedFabricIO();
 await assert.rejects(io.run(()=>f.cas.putImmutableBatchBlob(admission,r,Readable.from([wrong]))),{code:'checksum_mismatch'});assert.equal(f.calls.filter(c=>c.key?.includes('/objects/')&&c.name==='PutObjectCommand').length,0);assert.throws(()=>f.cas.finishImmutableBatch(admission));
 const g=fixture(),own=await ready(g),bytes=Buffer.from('conflict'),object=ref(bytes),key='fabric/v1/accounts/'+sha(Buffer.from('batch-retention-fixture'))+'/objects/sha256/'+object.digest;g.objects.set(key,Buffer.from('bad'));
 await assert.rejects(batch(g,own,[{bytes,ref:object}]));assert(g.calls.some(c=>c.name==='GetObjectCommand'&&c.key===key));assert.equal(own.status().admission_closed,true);
 const t=fixture(),typedOwner=await ready(t),invalid=canonicalFabricMetadata({format_version:1,mode:493,entries:[],unknown:true}),bad=ref(invalid,'directory');await assert.rejects(batch(t,typedOwner,[{bytes:invalid,ref:bad}]));assert.equal(t.calls.filter(c=>c.name==='PutObjectCommand'&&c.key.includes('/objects/')).length,0);
});
test('lost hold write reply retains original uncertain attempt; no retry/readback reconstruction',async()=>{
 const f=fixture();await f.storage.qualifyImmutableBatchRetentionRetaining(f.original);const owner=f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots());let called=0;f.state.onPut=async i=>{if(i.Key.includes('/batch-holds-v1/')){called++;f.objects.set(i.Key,Buffer.from(i.Body));throw Error('lost synthetic ACK');}};
 await assert.rejects(f.cas.beginImmutableBatchStaging(owner));assert.equal(f.objects.size,1);assert.equal(owner.status().admission_closed,true);assert.equal(owner.status().durable_hold_acknowledged,false);await assert.rejects(f.cas.beginImmutableBatchStaging(owner));assert.equal(called,1);assert.equal(f.storage.batchRetentionStatus().retained_transfers,1);
});
test('late direct hold ACK remains a known cleanup fact but grants no expired admission',async()=>{
 const f=fixture();await f.storage.qualifyImmutableBatchRetentionRetaining(f.original);const owner=f.cas.prepareImmutableBatchStaging(randomUUID(),f.original,roots());f.state.onPut=async i=>{if(i.Key.includes('/batch-holds-v1/'))f.state.now=f.end;};await assert.rejects(f.cas.beginImmutableBatchStaging(owner));
 assert.equal(owner.status().durable_hold_acknowledged,true);assert.equal(owner.status().admission_closed,true);assert.equal(f.objects.size,1);assert.throws(()=>owner.assertCurrent());
});
test('cancel/fence retains actual SDK, body, batch slot and hold until actual settlement; no new mutation',async()=>{
 const f=fixture(),owner=await ready(f),gate=deferred(),entered=deferred(),aborted=deferred(),bytes=Buffer.from('pending'),r=ref(bytes);f.state.onPut=async(i,signal)=>{if(i.Key.includes('/objects/')){signal.addEventListener('abort',()=>aborted.resolve(),{once:true});entered.resolve();await gate.promise;}};
 const {h,bytes:body}=wire(owner,[{bytes,ref:r}]),reader=new BatchReader(Readable.from([body]),()=>owner.assertCurrent());await reader.header(r);const work=new ImageBatchWork();work.acquire();const actual=work.runImmutable(reader,h,f.cas,owner,()=>owner.assertCurrent());
 try{await entered.promise;f.storage.fenceImmutableBatchRetention();await aborted.promise;await assert.rejects(actual);work.finished=true;work.releaseIfSettled();assert.equal(work.slot,true);assert.equal(work.objects[0].settled,false);assert.equal(owner.status().io_settled,false);assert.throws(()=>f.cas.finishImmutableBatchTransfer(owner));const n=f.calls.length;await assert.rejects(f.cas.publishRootWithImmutableBatch(owner,'no-new-root',r));assert.equal(f.calls.length,n);}
 finally{gate.resolve();await until(()=>work.objects[0].settled);work.finished=true;work.releaseIfSettled();}
 assert.equal(work.slot,false);assert.equal(owner.status().io_settled,true);assert.equal(f.storage.batchRetentionStatus().retained_transfers,1);assert.equal([...f.objects.keys()].filter(k=>k.includes('/batch-holds-v1/')).length,1);
});
test('two original root publications retain the immutable hold and permit normal settled carrier release',async()=>{
 const f=fixture(),owner=await ready(f),bytes=Buffer.from('manifest-fixture'),directory=canonicalFabricMetadata({format_version:1,mode:493,entries:[]}),m=ref(bytes),d=ref(directory,'directory');await batch(f,owner,[{bytes,ref:m},{bytes:directory,ref:d}]);
 const at=f.calls.length,a=await f.cas.publishRootWithImmutableBatch(owner,'fixture-manifest',m),b=await f.cas.publishRootWithImmutableBatch(owner,'fixture-installer',d);assert.equal(a.durability,'replicated');assert.equal(b.durability,'replicated');assert.equal(f.calls.slice(at).filter(c=>c.key?.includes('/staging/')||c.name==='DeleteObjectCommand').length,0);
 const before=f.objects.size;f.cas.finishImmutableBatchTransfer(owner);assert.equal(f.storage.batchRetentionStatus().retained_transfers,0);assert.equal(owner.status().admission_closed,true);assert.equal(f.objects.size,before);assert.equal([...f.objects.keys()].filter(k=>k.includes('/batch-holds-v1/')).length,1);
});
