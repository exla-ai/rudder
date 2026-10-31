// PRIVATE preparation. Import/default/--describe cause no provider calls, DNS,
// writes or child launches. Root alone opts in after the tagged rollout.
import fs from 'node:fs/promises';
import {constants as FC} from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash, randomBytes, randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {parseArgs} from 'node:util';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {command, parseProcStatus, heldNamespaceWitness, privateErrorDetail, gateSource,
  parseOwnedExecJson, preserveOwnedExecOutput, proofReadSource} from './helpers.mjs';

export const ROLE='alpha14-partial-checkpoint-data-smoke';
const APP='rudder-workers-exla', CONTROL='rudder-cloud-control', REGION='iad';
const BLOCKED_ID='8edd225a7d0378', BLOCKED_INSTANCE='01M670MG4A8E0DWE9GA1C0PQ92';
const BASE='https://rudder-cloud-control.fly.dev/api/rudder/fabric';
const HERE=path.dirname(fileURLToPath(import.meta.url));
const FIXTURE_SHA='5a47a7321952d5064734d9667bf3ddd2ed66f96ada4f871cfb7e0e47530ae76f';
const MAX_PROOF=1048576, MAIN_MS=420000, LIFETIME_MS=600000;
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const quote=value=>"'"+value.replaceAll("'","'\\''")+"'";
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const refKey=ref=>`${ref.kind}:${ref.algorithm}:${ref.digest}:${ref.size_bytes}`;
const equalRef=(a,b)=>refKey(a)===refKey(b);
const HEX=/^[a-f0-9]{64}$/, DIGEST=/^sha256:[a-f0-9]{64}$/;
const SOURCE_FILES=['dist/fabric-remote.js','dist/fabric-local.js','dist/native-binary.js',
  'cloud/src/server.ts','cloud/dist/server.js',...['fabric-cas','fabric-http','fabric-content','fabric-directory',
    'fabric-snapshot','fabric-stream','fabric-partial-checkpoint'].flatMap(name=>['cloud/src/'+name+'.ts','cloud/dist/'+name+'.js'])];

export function options(argv) {
  const {values:v}=parseArgs({args:argv,strict:true,allowPositionals:false,options:{run:{type:'boolean'},describe:{type:'boolean'},
    'module-root':{type:'string'},version:{type:'string'},revision:{type:'string'},image:{type:'string'},digest:{type:'string'},
    'control-id':{type:'string'},'control-revision':{type:'string'},nonce:{type:'string'}}});
  if(!v.run)return {run:false};
  assert(!v.describe);assert.equal(v.version,'4.0.0-alpha.14');assert.match(v.revision||'',/^[a-f0-9]{40}$/);
  assert.equal(v.image,'registry.fly.io/rudder-workers-exla:'+v.version);assert.match(v.digest||'',DIGEST);
  assert.match(v['control-id']||'',/^[a-f0-9]{14}$/);assert.notEqual(v['control-id'],BLOCKED_ID);
  assert.equal(v['control-revision'],v.revision,'this smoke requires control and worker from the same reviewed tag');
  const root=v['module-root'];assert(typeof root==='string'&&path.isAbsolute(root));
  assert.equal(path.dirname(root),'/workspace/home/.cache/rudder-release-build');
  assert.equal(path.basename(root),'alpha14-tagged-'+v.revision.slice(0,7));
  const nonce=v.nonce||randomUUID();assert.match(nonce,/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/);
  assert.notEqual(nonce,'00000000-0000-0000-0000-000000000000');
  return {run:true,root,version:v.version,revision:v.revision,image:v.image,digest:v.digest,controlId:v['control-id'],
    controlRevision:v['control-revision'],nonce};
}

export function fixtureBundle(bytes) {
  assert.equal(sha(bytes),FIXTURE_SHA);const f=JSON.parse(bytes);
  // JSON.parse is used only to select small references/scope. Raw strings are
  // NEVER re-encoded; they contain integers beyond JS's exact Number range.
  const envelope=JSON.parse(f.envelope_json),template=JSON.parse(f.template_json),p=envelope.payload;
  const objects=[...f.objects,...f.base_objects.filter(item=>item.reference.kind==='blob')];
  const seen=new Set();for(const item of [...f.objects,...f.base_objects]){
    const raw=Buffer.from(item.hex,'hex');assert.equal(raw.toString('hex'),item.hex);assert.equal(raw.length,item.reference.size_bytes);
    assert.equal(sha(raw),item.reference.digest);assert(Number.isSafeInteger(item.reference.size_bytes));
  }
  for(const item of objects){assert(!seen.has(refKey(item.reference)));seen.add(refKey(item.reference));}
  assert.equal(objects.length,13);assert.equal(f.objects.length,9);assert.equal(f.base_objects.length,7);
  assert.equal(sha(Buffer.from(f.envelope_json)),f.publication.digest);assert.equal(f.publication.kind,'action_result');
  assert.match(f.receipt_json,/"generation":9007199254740995(?:,|})/);
  assert.match(f.q_json,/"generation":9007199254740997(?:,|})/);
  assert.match(f.runnable_json,/"knowledge_cursor":9007199254740999(?:,|})/);
  assert.match(f.drain_json,/18446744073709551614/);
  const expected={publicationId:f.publication_id,publication:f.publication,scope:template.selection.root.scope,base:template.selection.root,
    refs:objects.map(item=>item.reference),objectCount:13,totalBytes:objects.reduce((n,item)=>n+item.reference.size_bytes,0),
    q:p.q,runnable:p.runnable,qBase64:Buffer.from(f.q_json).toString('base64'),runnableBase64:Buffer.from(f.runnable_json).toString('base64'),
    receiptSha256:sha(Buffer.from(f.receipt_json))};
  assert.equal(expected.totalBytes,12953);return {fixture:f,expected};
}

export function taggedMetadata(value,bytes) {
  assert(bytes.length>0&&bytes.length<=MAX_PROOF);const meta=JSON.parse(bytes);
  assert.equal(meta.formatVersion,1);assert.equal(meta.sourceKind,'tagged');assert.equal(meta.version,value.version);assert.equal(meta.packageVersion,value.version);
  assert.equal(meta.revision,value.revision);assert.equal(meta.frozenRoot,value.root);assert.match(meta.archiveSha256||'',HEX);assert.match(meta.sourceEnvelopeSha256||'',HEX);
  assert.equal(meta.sourceEnvelopeEncoding,'sha256 of sorted compact JSON path-to-sha256 map');assert.equal(meta.matches,0);
  assert(meta.source&&typeof meta.source==='object'&&!Array.isArray(meta.source));const keys=Object.keys(meta.source).sort();
  assert.equal(keys.length,meta.sourceFiles);assert(keys.length>0&&keys.length<=5000);assert(Number.isSafeInteger(meta.sourceBytes)&&meta.sourceBytes>0&&meta.sourceBytes<=128*MAX_PROOF);
  const archivedTrackedLedger={"path":"native/src/.ledger/sessions/5c81b5ca-653d-48a2-b42a-0f8d8a3d1c3a.jsonl","sha256":"b8a4eac658f835a1ba68c82cdac38aaa2e99b8e1f2ffd064f77acb9a68cfe3bb"};
  for(const key of keys){assert(key.length>0&&key.length<=512&&/^[ -~]+$/.test(key)&&!key.includes('\\')&&!key.startsWith('/'));
    const exactArchivedSource=key===archivedTrackedLedger.path&&meta.source[key]===archivedTrackedLedger.sha256;
    assert(key.split('/').every(part=>part&&part!=='.'&&part!=='..'&&!['.git','.rudder'].includes(part)&&(part!=='.ledger'||exactArchivedSource)));assert.match(meta.source[key],HEX);}
  const sorted=Object.fromEntries(keys.map(key=>[key,meta.source[key]]));assert.equal(sha(Buffer.from(JSON.stringify(sorted))),meta.sourceEnvelopeSha256);
  assert.equal(meta.source['tests/fixtures/partial-checkpoint-v1.json'],FIXTURE_SHA);
  return {meta,bytes,sha256:sha(bytes)};
}

export async function verifySourceMap(root,meta) {
  assert.equal(await fs.realpath(root),root);let total=0;const buffer=Buffer.alloc(65536);
  // Explicit tracked-source allowlist only; no recursive traversal, runtime
  // ledger/profile/auth/session discovery, node_modules or build cache reads.
  for(const [relative,expected]of Object.entries(meta.source)){
    const pieces=relative.split('/');let parent=root;
    for(const part of pieces.slice(0,-1)){parent=path.join(parent,part);const m=await fs.lstat(parent);assert(m.isDirectory()&&!m.isSymbolicLink());}
    const file=path.join(root,relative),handle=await fs.open(file,FC.O_RDONLY|FC.O_NOFOLLOW|FC.O_NONBLOCK);
    try{const before=await handle.stat();assert(before.isFile()&&before.nlink===1&&before.size>=0&&before.size<=16*MAX_PROOF);total+=before.size;assert(total<=128*MAX_PROOF);
      const h=createHash('sha256');let n=0;for(;;){const {bytesRead}=await handle.read(buffer,0,buffer.length,null);if(!bytesRead)break;n+=bytesRead;assert(n<=before.size);h.update(buffer.subarray(0,bytesRead));}
      assert.equal(n,before.size);assert.equal(h.digest('hex'),expected);for(const after of [await handle.stat(),await fs.lstat(file)])assert(after.isFile()&&!after.isSymbolicLink()&&after.dev===before.dev&&after.ino===before.ino&&after.nlink===1&&after.size===n&&after.mtimeMs===before.mtimeMs);
    }finally{await handle.close();}
  }
  assert.equal(total,meta.sourceBytes);return {sourceFiles:Object.keys(meta.source).length,sourceBytes:total,verified:true};
}

export async function readBoundedFile(file,max) {
  const handle=await fs.open(file,FC.O_RDONLY|FC.O_NOFOLLOW|FC.O_NONBLOCK);
  try{const before=await handle.stat();assert(before.isFile()&&before.nlink===1&&before.size>=0&&before.size<=max);
    const bytes=Buffer.alloc(before.size+1);let n=0;for(;;){const {bytesRead}=await handle.read(bytes,n,bytes.length-n,null);if(!bytesRead)break;n+=bytesRead;assert(n<=before.size);}
    assert.equal(n,before.size);for(const after of [await handle.stat(),await fs.lstat(file)])assert(after.isFile()&&!after.isSymbolicLink()&&after.dev===before.dev&&after.ino===before.ino&&after.nlink===1&&after.size===n&&after.mtimeMs===before.mtimeMs);
    return bytes.subarray(0,n);
  }finally{await handle.close();}
}

async function loadInputs(value) {
  const release=taggedMetadata(value,await readBoundedFile(path.join(HERE,'../frozen-tagged-inputs.json'),MAX_PROOF));
  const checked=await verifySourceMap(value.root,release.meta);
  assert.equal(await fs.realpath(value.root),value.root);const pkg=JSON.parse(await readBoundedFile(path.join(value.root,'package.json'),65536));
  assert.equal(pkg.version,value.version);const hashes={};
  for(const file of SOURCE_FILES){const body=await readBoundedFile(path.join(value.root,file),4*MAX_PROOF);assert(body.length>0);hashes[file]=sha(body);}
  const bundle=fixtureBundle(await readBoundedFile(path.join(HERE,'fixture-v1.json'),65536));
  assert.equal(sha(await readBoundedFile(path.join(value.root,'tests/fixtures/partial-checkpoint-v1.json'),65536)),FIXTURE_SHA);
  return {...bundle,hashes,sourceEnvelopeSha256:release.meta.sourceEnvelopeSha256,runtimeEnvelopeSha256:sha(Buffer.from(JSON.stringify(hashes))),release,
    taggedSourceProof:{metadataSha256:release.sha256,revision:value.revision,sourceKind:'tagged',archiveSha256:release.meta.archiveSha256,...checked}};
}

// Bounded streaming hash, nofollow/nonblocking open, and held/named identity
// recheck. Root-group writable archives are readable by UID1500, never writable
// by it (GID1500 and zero supplementary groups). No world-writable image input.
export function protectedRead(file,max,fs,crypto,assert,{native=false,bytes=false}={}) {
  const parts=file.split('/').filter(Boolean);let dir='';
  for(const piece of parts.slice(0,-1)){dir+='/'+piece;const m=fs.lstatSync(dir);
    assert(m.isDirectory()&&!m.isSymbolicLink()&&m.uid===0&&m.gid===0&&!(m.mode&0o002));}
  const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
  try{const before=fs.fstatSync(fd);assert(before.isFile()&&before.uid===0&&before.gid===0&&before.nlink===1&&before.size>0&&before.size<=max);
    assert([...(native?[0o755]:[0o644,0o664])].includes(before.mode&0o7777));
    const h=crypto.createHash('sha256'),buffer=Buffer.alloc(65536),pieces=[];let total=0;
    for(;;){const n=fs.readSync(fd,buffer,0,buffer.length,null);if(!n)break;total+=n;assert(total<=max&&total<=before.size);h.update(buffer.subarray(0,n));if(bytes)pieces.push(Buffer.from(buffer.subarray(0,n)));}
    assert.equal(total,before.size);for(const after of [fs.fstatSync(fd),fs.lstatSync(file)])assert(after.isFile()&&!after.isSymbolicLink()&&after.dev===before.dev&&after.ino===before.ino&&after.mode===before.mode&&after.uid===0&&after.gid===0&&after.nlink===1&&after.size===total);
    return {sha256:h.digest('hex'),size:total,...(bytes?{bytes:Buffer.concat(pieces)}:{})};
  }finally{fs.closeSync(fd);}
}

export function runtimeCheck(input,fs,crypto,assert,protectedRead) {
  const hashes={};for(const [relative,expected]of Object.entries(input.workerHashes)){
    const found=protectedRead('/opt/rudder/'+relative,4194304,fs,crypto,assert);assert.equal(found.sha256,expected);hashes[relative]=found.sha256;}
  const pkg=protectedRead('/opt/rudder/package.json',65536,fs,crypto,assert,{bytes:true});assert.equal(JSON.parse(pkg.bytes).version,input.version);
  const binary=protectedRead('/opt/rudder/dist/native/rudder-native',268435456,fs,crypto,assert,{native:true});
  return {workerVersion:input.version,packageSha256:pkg.sha256,moduleHashes:hashes,nativeSha256:binary.sha256,nativeBytes:binary.size};
}

export function controlProbeSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const input=${JSON.stringify(input)},read=${protectedRead.toString()};assert.equal(process.getuid(),0);const hashes={};
for(const [relative,expected]of Object.entries(input.hashes)){const file='/app/'+relative.slice('cloud/'.length);const got=read(file,4194304,fs,crypto,assert);assert.equal(got.sha256,expected);hashes[relative]=got.sha256;}
assert.equal(process.env.RUDDER_FABRIC_DURABLE_STORE,'1');const p=process.env.RUDDER_CLOUD_DB||require('node:path').join(process.env.RUDDER_CLOUD_DATA_DIR||require('node:path').join(require('node:os').homedir(),'.rudder-cloud'),'rudder-cloud.sqlite');
assert(['/data/rudder-cloud.sqlite','/tmp/rudder-cloud/rudder-cloud.sqlite','/root/.rudder-cloud/rudder-cloud.sqlite'].includes(p));const m=fs.lstatSync(p);assert(m.isFile()&&!m.isSymbolicLink());
console.log(JSON.stringify({status:'passed',nonce:input.nonce,hashes,databasePath:p,durableStoreEnabled:true}));`;
}

// Only the nonce-qualified synthetic row is inspected or mutated. No table
// inventory, profile, bearer, provider key or unrelated account row is read.
export function authSource(identity,mode,rowid=null) {
  assert(['inspect','create','remove','absent'].includes(mode));
  return `const D=require('/app/node_modules/better-sqlite3'),assert=require('node:assert/strict'),fs=require('node:fs');const v=${JSON.stringify(identity)},mode=${JSON.stringify(mode)},expectedRowid=${JSON.stringify(rowid)};
assert(v.workspace==='qualification-alpha14-pcd-'+v.nonce&&v.account==='qualification:alpha14:pcd:'+v.nonce&&v.key==='qualification:alpha14:pcd:'+v.nonce);assert(/^[0-9a-f]{32}$/.test(v.tokenHash));
const m=fs.lstatSync(v.databasePath);assert(m.isFile()&&!m.isSymbolicLink());const db=new D(v.databasePath,{fileMustExist:true,timeout:2000});db.pragma('busy_timeout=2000');
const get=()=>db.prepare('select cast(rowid as text) as rowid,id,account_id,workspace_key,status,worker_token_hash,created_at,updated_at,machine_id,volume_id,home_id from rudder_workspaces where id=?').get(v.workspace);
const exact=r=>r&&r.id===v.workspace&&r.account_id===v.account&&r.workspace_key===v.key&&r.worker_token_hash===v.tokenHash&&r.created_at===v.createdAt&&r.updated_at===v.createdAt&&r.status==='stopped'&&r.machine_id===null&&r.volume_id===null&&r.home_id===null&&(!expectedRowid||r.rowid===expectedRowid);
let out;try{db.exec('BEGIN IMMEDIATE');let r=get();
if(mode==='inspect'||mode==='absent'){assert(!r);out={status:'absent',nonce:v.nonce};}
else if(mode==='create'){assert(!r);db.prepare("insert into rudder_workspaces(id,account_id,workspace_key,status,worker_token_hash,created_at,updated_at) values(?,?,?,'stopped',?,?,?)").run(v.workspace,v.account,v.key,v.tokenHash,v.createdAt,v.createdAt);r=get();assert(exact(r));out={status:'created',nonce:v.nonce,rowid:r.rowid};}
else{if(r){assert(exact(r));const result=db.prepare('delete from rudder_workspaces where id=? and rowid=? and account_id=? and workspace_key=? and worker_token_hash=? and created_at=? and updated_at=? and status=? and machine_id is null and volume_id is null and home_id is null').run(v.workspace,r.rowid,v.account,v.key,v.tokenHash,v.createdAt,v.createdAt,'stopped');assert.equal(result.changes,1);}assert(!get());out={status:'absent',nonce:v.nonce};}
db.exec('COMMIT');}catch(e){try{db.exec('ROLLBACK');}catch{}throw e;}finally{db.close();}console.log(JSON.stringify(out));`;
}

// Actual uploader; this adapter owns only synthetic immutable DATA bytes. Its
// transfer-shaped pin is NOT a resurrected native producer/current receipt.
export async function hostPublication(input) {
  const fs=await import('node:fs/promises'),crypto=await import('node:crypto'),{default:assert}=await import('node:assert/strict');
  const {FabricRemoteClient}=await import(input.moduleUrl);const f=JSON.parse(await fs.readFile(input.fixturePath));
  const digest=b=>crypto.createHash('sha256').update(b).digest('hex'),key=r=>`${r.kind}:${r.algorithm}:${r.digest}:${r.size_bytes}`;
  assert.equal(digest(await fs.readFile(input.fixturePath)),input.fixtureSha256);assert.match(process.env.PCD_BEARER||'',/^rdrw_[A-Za-z0-9_-]{43}$/);
  const p=JSON.parse(f.envelope_json).payload,base=JSON.parse(f.template_json).selection.root;
  const remote=new FabricRemoteClient({baseUrl:input.baseUrl,authHeaders:()=>({Authorization:'Bearer '+process.env.PCD_BEARER,'X-Rudder-Workspace-Id':input.workspace}),
    scope:base.scope,concurrency:1,requestTimeoutMs:8000});
  const stage=await remote.beginStaging();for(const item of f.base_objects)await remote.putObject(stage.lease_id,item.reference,Buffer.from(item.hex,'hex'));
  const baseReceipt=await remote.publishRoot(base.root_id,stage.lease_id,base.root);assert.deepEqual(baseReceipt.root,base.root);
  const local=new Map(f.objects.map(o=>[key(o.reference),Buffer.from(o.hex,'hex')])),issued=new Set();let pin,held=false,reads=0,receipts=0,releases=0;
  const source={async pinPartialCheckpoint(raw){assert.equal(raw,f.receipt_json);assert(!held);pin='transfer:partial-checkpoint:rpc:'+crypto.randomUUID();assert(!issued.has(pin));issued.add(pin);held=true;receipts++;
    const out={format_version:1,owned_pin:pin,publication_id:f.publication_id,publication:f.publication,q:p.q,source:p.source,runnable:p.runnable,plan:p.plan,
      freeze_intent:p.freeze_intent,drain_record:p.drain_record,base,objects:f.objects.map(o=>o.reference)};assert.equal(Object.keys(out).length,12);return out;},
    async unpin(value){assert.equal(value,pin);assert(held);held=false;pin=undefined;releases++;},async read(ref){assert(held);const bytes=local.get(key(ref));assert(bytes);reads++;return Buffer.from(bytes);}};
  const uploaded=await remote.uploadPartialCheckpoint(f.publication_id,f.receipt_json,source);assert.equal(uploaded.kind,'partial_checkpoint_data');
  assert.equal(uploaded.source_role,'quiesced_source_complement_data');assert(!Object.hasOwn(uploaded,'receipt')&&!Object.hasOwn(uploaded,'local'));
  assert.equal(held,false);assert.equal(reads,9);assert.equal(uploaded.uploaded_objects,9);assert.deepEqual(uploaded.bundle.root,f.publication);
  const found=await remote.getRoot(f.publication_id);assert.deepEqual(found,uploaded.bundle);assert.equal(found.object_count,13);assert.equal(found.total_bytes,12953);
  const expected=[...f.objects,...f.base_objects.filter(o=>o.reference.kind==='blob')];for(const item of expected)assert.deepEqual(await remote.getObject(item.reference),Buffer.from(item.hex,'hex'));
  // Same immutable ID reconciliation uses the real uploader's GET path. This
  // smoke does not manufacture an acknowledgment loss; the HTTP test does.
  const retry=await remote.uploadPartialCheckpoint(f.publication_id,f.receipt_json,source);assert.deepEqual(retry.bundle,found);assert.equal(retry.uploaded_objects,0);assert.equal(retry.uploaded_bytes,0);assert.equal(reads,9);
  assert.equal(receipts,2);assert.equal(releases,2);assert.equal(held,false);
  console.log(JSON.stringify({status:'passed',kind:'synthetic-checkpoint-data-publication',bundle:found,base:baseReceipt,uploadedObjects:9,
    exactObjectReads:13,rawReceiptSha256:digest(Buffer.from(f.receipt_json)),opaqueHighU64Receipt:true,sameIdGetReconciled:true,
    syntheticDataOnly:true,activeProducerReceiptQualified:false,currentGrantQualified:false}));
}

export async function nativeRestore(input) {
  const {default:assert}=await import('node:assert/strict'),crypto=await import('node:crypto');
  const {FabricLocalClient}=await import('/opt/rudder/dist/fabric-local.js'),{FabricRemoteClient}=await import('/opt/rudder/dist/fabric-remote.js');
  const env={PATH:'/usr/local/bin:/usr/bin:/bin',HOME:input.home,RUDDER_HOME:input.home+'/cas',TMPDIR:input.home+'/tmp',LANG:'C',LC_ALL:'C'};
  assert.equal(Object.keys(env).filter(k=>/TOKEN|SECRET|PASSWORD|KEY|CREDENTIAL|AUTH|NODE_OPTIONS/i.test(k)).length,0);
  const remote=new FabricRemoteClient({baseUrl:input.baseUrl,authHeaders:()=>({Authorization:'Bearer '+process.env.PCD_BEARER,'X-Rudder-Workspace-Id':input.workspace}),
    scope:input.expected.scope,concurrency:1,requestTimeoutMs:8000});
  const e=input.expected,key=r=>`${r.kind}:${r.algorithm}:${r.digest}:${r.size_bytes}`,pin='alpha14-pcd-restored-'+input.nonce.slice(0,8);
  let local,closed=0;const open=()=>FabricLocalClient.open({home:env.RUDDER_HOME,nativeBinary:'/opt/rudder/dist/native/rudder-native',env,requestTimeoutMs:10000});
  const verify=async()=>{const closure=await local.closure(e.publication);assert.deepEqual(new Set(closure.map(key)),new Set(e.refs.map(key)));assert.equal(closure.length,13);
    for(const ref of closure){const bytes=await local.read(ref);assert.equal(bytes.length,ref.size_bytes);assert.equal(crypto.createHash('sha256').update(bytes).digest('hex'),ref.digest);}
    assert.equal((await local.read(e.q)).toString('base64'),e.qBase64);assert.equal((await local.read(e.runnable)).toString('base64'),e.runnableBase64);
    assert.match((await local.read(e.q)).toString(),/"generation":9007199254740997(?:,|})/);assert.match((await local.read(e.runnable)).toString(),/"knowledge_cursor":9007199254740999(?:,|})/);};
  try{local=await open();assert.deepEqual((await local.request({operation:'retention'})).pins,{});
    const download=await remote.downloadRoot(e.publicationId,local,pin);assert.deepEqual(download.receipt.root,e.publication);assert.equal(download.downloaded_objects,13);assert.equal(download.downloaded_bytes,e.totalBytes);
    await assert.rejects(local.read(e.publication));await local.pin(e.publication);await verify();await local.close();closed++;local=undefined;
    local=await open();assert.deepEqual((await local.request({operation:'retention'})).pins[pin],e.publication);await local.pin(e.publication);await verify();
    await local.close();closed++;local=undefined;
    console.log(JSON.stringify({status:'passed',kind:'fresh-native-checkpoint-data-retention',publicationId:e.publicationId,publication:e.publication,
      closureObjects:13,closureBytes:e.totalBytes,exactAllObjectBytes:true,rawQBytes:true,rawRunnableBytes:true,highU64Preserved:true,
      retainedAcrossNativeRestart:true,nativeHelpersSettled:closed,syntheticDataOnly:true,activeProducerReceiptQualified:false,currentGrantQualified:false,
      sourceAcceptanceQualified:false,runnableExecutionQualified:false,nativeCredentialEnvironmentFields:0}));
  }finally{if(local)await local.close();}
}

export function guestSource(input) {
  return 'await ('+guestPreload.toString()+')('+JSON.stringify(input)+','+parseProcStatus.toString()+','+heldNamespaceWitness.toString()+','+
    protectedRead.toString()+','+runtimeCheck.toString()+','+privateErrorDetail.toString()+','+nativeRestore.toString()+');';
}

export async function guestPreload(input,parseProcStatus,heldNamespaceWitness,protectedRead,runtimeCheck,privateErrorDetail,nativeRestore) {
  setTimeout(()=>process.exit(124),360000);
  const fs=await import('node:fs'),cp=await import('node:child_process'),crypto=await import('node:crypto'),os=await import('node:os'),{default:assert}=await import('node:assert/strict');
  const out={formatVersion:1,kind:'alpha14-partial-checkpoint-data-smoke',nonce:input.nonce,version:input.version,revision:input.revision,
    completed:false,qualified:false,syntheticDataOnly:true,activeProducerReceiptQualified:false,currentGrantQualified:false,sourceAcceptanceQualified:false,
    runnableExecutionQualified:false,supervisorModuleEvaluated:false,modelCalls:false,providerAccountCalls:false};
  const write=(file,body)=>{const fd=fs.openSync(file,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK,0o600);try{fs.writeFileSync(fd,body);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}};
  const status=()=>parseProcStatus(fs.readFileSync('/proc/self/status','utf8')),ids={};let phase='admission-gate';
  const witness=kind=>heldNamespaceWitness(fs,assert,kind),identity=w=>({device:w.device,inode:w.inode});
  try{
    let admitted=false;const until=Date.now()+120000;
    while(Date.now()<until){let fd;try{fd=fs.openSync(input.gatePath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);const m=fs.fstatSync(fd);
      if(m.nlink===2){assert(m.isFile()&&m.uid===1500&&m.gid===1500&&(m.mode&0o7777)===0o600);await new Promise(r=>setTimeout(r,20));continue;}
      assert(m.isFile()&&m.uid===1500&&m.gid===1500&&m.nlink===1&&(m.mode&0o7777)===0o600&&m.size<=4096);
      const b=Buffer.alloc(4097);let n=0;for(;;){const q=fs.readSync(fd,b,n,b.length-n,null);if(!q)break;n+=q;assert(n<=4096);}
      for(const x of [fs.fstatSync(fd),fs.lstatSync(input.gatePath)])assert(x.isFile()&&!x.isSymbolicLink()&&x.uid===1500&&x.gid===1500&&x.dev===m.dev&&x.ino===m.ino&&x.nlink===1&&x.mode===m.mode&&x.size===n);
      assert.deepEqual(JSON.parse(b.subarray(0,n)),input.gate);admitted=true;break;
    }catch(e){if(e.code!=='ENOENT')throw e;await new Promise(r=>setTimeout(r,100));}finally{if(fd!==undefined)fs.closeSync(fd);}}
    assert(admitted);out.admittedAfterResolvedDigest=true;phase='boot-identity';
    assert.equal(process.getuid(),1500);assert.equal(process.getgid(),1500);const before=status();
    assert.equal(before.Uid,'1500\t1500\t1500\t1500');assert.equal(before.Gid,'1500\t1500\t1500\t1500');assert.equal(before.Groups,'');assert.equal(before.NoNewPrivs,'1');
    for(const k of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])assert.match(before[k],/^0+$/);
    const credentialKeys=Object.keys(process.env).filter(k=>/TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|AUTH_KEY|SSH_AUTH_SOCK|AWS_ACCESS_KEY/i.test(k));assert.deepEqual(credentialKeys,['RUDDER_WORKER_TOKEN']);
    assert.equal(process.env.RUDDER_WORKSPACE_ID,input.workspace);assert.match(process.env.RUDDER_WORKER_TOKEN||'',/^rdrw_[A-Za-z0-9_-]{43}$/);
    assert.equal(crypto.createHash('sha256').update(process.env.RUDDER_WORKER_TOKEN).digest('hex').slice(0,32),input.tokenHash);
    for(const k of ['RUDDER_HOME','RUDDER_WORKER_MANAGED_EXECUTION','RUDDER_WORKER_SKIP_AGENT_UPDATE','RUDDER_WORKER_FUSE_READY'])assert.equal(process.env[k],'1');
    out.boot={uid:1500,gid:1500,fields:before,kernel:os.release(),node:process.version,syntheticCredentialFields:1,otherCredentialFields:0};
    phase='runtime-source-binding';out.runtime=runtimeCheck(input,fs,crypto,assert,protectedRead);
    for(const kind of ['user','net','mnt'])ids[kind]=witness(kind);out.parentBefore=Object.fromEntries(Object.entries(ids).map(([k,w])=>[k,identity(w)]));
    phase='fresh-private-home';assert.equal(fs.lstatSync('/workspace').uid,1500);assert.equal(input.home,'/workspace/pc-'+input.nonce.slice(0,8));assert(!fs.existsSync(input.home));
    fs.mkdirSync(input.home,{mode:0o700});assert.equal(fs.realpathSync(input.home),input.home);for(const name of ['cas','tmp'])fs.mkdirSync(input.home+'/'+name,{mode:0o700});
    const script='('+nativeRestore.toString()+')('+JSON.stringify(input)+').catch(e=>{console.error(JSON.stringify({status:"failed",step:"NATIVE_DATA_DRIVER",code:typeof e.code==="string"?e.code:"ASSERTION",httpStatus:Number.isInteger(e.status)?e.status:null}));process.exitCode=1;});';
    phase='actual-stock-native-restore';const child=await new Promise(resolve=>{
      const ch=cp.spawn('/usr/local/bin/node',['--input-type=module','-e',script],{cwd:input.home,detached:true,env:{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:input.home,TMPDIR:input.home+'/tmp',
        LANG:'C',LC_ALL:'C',PCD_BEARER:process.env.RUDDER_WORKER_TOKEN},stdio:['ignore','pipe','pipe']});
      let stdout=Buffer.alloc(0),stderr=Buffer.alloc(0),failure,exit,signal,done=false,settle;
      const finish=settled=>{if(done)return;done=true;clearTimeout(timer);clearTimeout(settle);if(!settled){ch.stdout.destroy();ch.stderr.destroy();ch.unref();}
        write(input.home+'/native.stdout.private',stdout);write(input.home+'/native.stderr.private',stderr);resolve({stdout,stderr,settled,exit,signal,failure:failure||null});};
      const stop=code=>{if(done)return;failure||=code;try{process.kill(-ch.pid,'SIGKILL');}catch{}settle||=setTimeout(()=>finish(false),2000);};
      const timer=setTimeout(()=>stop('NATIVE_DATA_DEADLINE'),160000);
      ch.stdout.on('data',b=>{if(done)return;stdout=Buffer.concat([stdout,b.subarray(0,Math.max(0,65536-stdout.length))]);if(stdout.length>=65536)stop('NATIVE_DATA_STDOUT_BOUND');});
      ch.stderr.on('data',b=>{if(done)return;stderr=Buffer.concat([stderr,b.subarray(0,Math.max(0,8192-stderr.length))]);if(stderr.length>=8192)stop('NATIVE_DATA_STDERR_BOUND');});
      ch.on('error',()=>{failure||='NATIVE_DATA_SPAWN';});ch.on('exit',(code,sig)=>{exit=code;signal=sig;});ch.on('close',()=>finish(true));
    });
    out.child={settled:child.settled,exit:child.exit,signal:child.signal,failure:child.failure,stdoutBytes:child.stdout.length,stderrBytes:child.stderr.length,
      stdoutSha256:crypto.createHash('sha256').update(child.stdout).digest('hex'),stderrSha256:crypto.createHash('sha256').update(child.stderr).digest('hex')};
    out.parentAfter={};for(const kind of ['user','net','mnt']){const w=witness(kind);out.parentAfter[kind]=identity(w);fs.closeSync(w.fd);}
    assert.deepEqual(out.parentAfter,out.parentBefore);assert.deepEqual(status(),before);out.parentNamespacesAndStatusUnchanged=true;
    assert(child.settled&&!child.failure);assert.equal(child.exit,0);assert.equal(child.signal,null);const summary=JSON.parse(child.stdout.toString().trim());
    assert.equal(summary.status,'passed');assert.equal(summary.nativeHelpersSettled,2);assert.equal(summary.closureObjects,13);assert.equal(summary.closureBytes,12953);
    out.summary=summary;out.completed=true;out.qualified=true;
  }catch(error){out.failedPhase=phase;out.errorCode=typeof error.code==='string'?error.code:'ASSERTION';try{write(input.failurePath,privateErrorDetail(error).formatted.subarray(0,8192));}catch{}}
  finally{for(const w of Object.values(ids))fs.closeSync(w.fd);write(input.proofPath,Buffer.from(JSON.stringify(out,null,2)+'\n'));}
  await new Promise(()=>{}); // Never import the ordinary supervisor/home entry.
}

export function plan(value,inputs,token,createdAt=new Date().toISOString()) {
  const workspace='qualification-alpha14-pcd-'+value.nonce,account='qualification:alpha14:pcd:'+value.nonce,tokenHash=sha(token).slice(0,32);
  const workerHashes=Object.fromEntries(Object.entries(inputs.hashes).filter(([p])=>p.startsWith('dist/')));
  const input={nonce:value.nonce,version:value.version,revision:value.revision,expected:inputs.expected,baseUrl:BASE,workspace,tokenHash,workerHashes,
    home:'/workspace/pc-'+value.nonce.slice(0,8),gatePath:'/workspace/pcd-gate-'+value.nonce+'.json',proofPath:'/workspace/pcd-proof-'+value.nonce+'.json',failurePath:'/workspace/pcd-failure-'+value.nonce+'.private'};
  input.gate={formatVersion:1,nonce:value.nonce,revision:value.revision,digest:value.digest,sourceEnvelopeSha256:inputs.sourceEnvelopeSha256};
  const source=guestSource(input);assert(Buffer.byteLength(source)<=48000);
  const name='rudder-alpha14-pcd-'+value.nonce.slice(0,8),config={image:value.image,guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},restart:{policy:'no'},auto_destroy:false,
    services:[],mounts:[],env:{RUDDER_HOME:'1',RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',RUDDER_WORKSPACE_ID:workspace,RUDDER_WORKER_TOKEN:token,
      NODE_OPTIONS:'--import=data:text/javascript;base64,'+Buffer.from(source).toString('base64')},metadata:{rudder_role:ROLE,qualification:value.nonce,
      qualification_revision:value.revision,qualification_source_envelope:inputs.sourceEnvelopeSha256}};
  assert(Buffer.byteLength(config.env.NODE_OPTIONS)<65536);
  return {nonce:value.nonce,name,role:ROLE,input,source,config,auth:{nonce:value.nonce,workspace,account,key:account,tokenHash,createdAt}};
}

export function ownershipFor(prepared,value) {
  let id,descriptor;const empty=v=>v===undefined||Array.isArray(v)&&v.length===0;
  const requested=m=>{
    if(!m||!/^[0-9a-f]{14}$/.test(m.id)||m.id===BLOCKED_ID||m.instance_id===BLOCKED_INSTANCE||m.name!==prepared.name)return false;
    const c=m.config;if(!c||!Object.entries(prepared.config.metadata).every(([k,v])=>c.metadata?.[k]===v))return false;
    const d=m.image_ref?.digest;if(d!==undefined&&!DIGEST.test(d))return false;
    if(c.image!==prepared.config.image&&!(typeof c.image==='string'&&c.image.startsWith(prepared.config.image+'@')&&DIGEST.test(c.image.slice(prepared.config.image.length+1))&&(!d||c.image===prepared.config.image+'@'+d)))return false;
    if(!['services','mounts','files'].every(k=>empty(c[k])))return false;
    if(!Object.entries(prepared.config.guest).every(([k,v])=>c.guest?.[k]===v))return false;
    if(!Object.entries(prepared.config.env).every(([k,v])=>c.env?.[k]===v))return false;
    if(!Object.entries(c.env||{}).every(([k,v])=>Object.hasOwn(prepared.config.env,k)||['PRIMARY_REGION','FLY_PRIMARY_REGION'].includes(k)&&v===REGION))return false;
    return true;
  };
  const matches=m=>requested(m)&&(!id||m.id===id)&&(!descriptor?.observedDigest||m.image_ref?.digest===descriptor.observedDigest);
  return {matches,id:()=>id,descriptor:()=>descriptor&&{...descriptor},remember(m){assert(matches(m));id??=m.id;
    descriptor??={id,name:prepared.name,nonce:value.nonce,role:ROLE,revision:value.revision,requestedImage:value.image,observedDigest:m.image_ref?.digest??null,instance:m.instance_id??null};
    if(!descriptor.observedDigest&&m.image_ref?.digest)descriptor.observedDigest=m.image_ref.digest;return {...descriptor};}};
}

export function validateGuest(proof,value,prepared) {
  assert.equal(proof.formatVersion,1);assert.equal(proof.kind,ROLE);assert.equal(proof.nonce,value.nonce);assert.equal(proof.revision,value.revision);assert.equal(proof.version,value.version);
  for(const k of ['completed','qualified','syntheticDataOnly','admittedAfterResolvedDigest','parentNamespacesAndStatusUnchanged'])assert.equal(proof[k],true);
  for(const k of ['supervisorModuleEvaluated','activeProducerReceiptQualified','currentGrantQualified','sourceAcceptanceQualified','runnableExecutionQualified','modelCalls','providerAccountCalls'])assert.equal(proof[k],false);
  assert.deepEqual(proof.parentAfter,proof.parentBefore);for(const k of ['user','net','mnt'])for(const n of ['device','inode'])assert.match(proof.parentBefore[k][n],/^[0-9]{1,20}$/);
  assert.equal(proof.boot.uid,1500);assert.equal(proof.boot.gid,1500);assert.equal(proof.boot.syntheticCredentialFields,1);assert.equal(proof.boot.otherCredentialFields,0);
  assert.equal(proof.boot.fields.Uid,'1500\t1500\t1500\t1500');assert.equal(proof.boot.fields.Gid,'1500\t1500\t1500\t1500');assert.equal(proof.boot.fields.Groups,'');assert.equal(proof.boot.fields.NoNewPrivs,'1');
  for(const k of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])assert.match(proof.boot.fields[k],/^0+$/);
  assert.equal(proof.runtime.workerVersion,value.version);assert.deepEqual(proof.runtime.moduleHashes,prepared.input.workerHashes);assert.match(proof.runtime.nativeSha256,HEX);assert(proof.runtime.nativeBytes>0&&proof.runtime.nativeBytes<=268435456);
  assert.equal(proof.child.settled,true);assert.equal(proof.child.exit,0);assert.equal(proof.child.signal,null);assert.equal(proof.child.failure,null);
  const s=proof.summary,e=prepared.input.expected;assert.equal(s.status,'passed');assert.equal(s.kind,'fresh-native-checkpoint-data-retention');assert.equal(s.publicationId,e.publicationId);assert.deepEqual(s.publication,e.publication);
  assert.equal(s.closureObjects,13);assert.equal(s.closureBytes,e.totalBytes);assert.equal(s.nativeHelpersSettled,2);assert.equal(s.nativeCredentialEnvironmentFields,0);
  for(const k of ['exactAllObjectBytes','rawQBytes','rawRunnableBytes','highU64Preserved','retainedAcrossNativeRestart','syntheticDataOnly'])assert.equal(s[k],true);
  for(const k of ['activeProducerReceiptQualified','currentGrantQualified','sourceAcceptanceQualified','runnableExecutionQualified'])assert.equal(s[k],false);
}

// Host publication runs in its own bounded process group. A timed-out promise
// is not settlement: process close must be observed before calling it success.
export async function runNode(source,env,{timeout,stdoutPath,stderrPath}) {
  return new Promise(resolve=>{
    const ch=spawn(process.execPath,['--input-type=module','-e',source],{detached:true,env,stdio:['ignore','pipe','pipe']});
    let out=Buffer.alloc(0),err=Buffer.alloc(0),status,signal,failure,ended=false,settle;
    const finish=async settled=>{if(ended)return;ended=true;clearTimeout(timer);clearTimeout(settle);if(!settled){ch.stdout.destroy();ch.stderr.destroy();ch.unref();}
      try{await preserveOwnedExecOutput(out,err,stdoutPath,stderrPath);resolve({ok:settled&&!failure&&status===0&&!signal,stdout:out.toString(),diagnostics:{settled,status,signal,failure:failure||null,stdoutSha256:sha(out),stderrSha256:sha(err),stdoutBytes:out.length,stderrBytes:err.length,privateStdoutFile:stdoutPath,privateStderrFile:stderrPath}});}
      catch{resolve({ok:false,stdout:'',diagnostics:{failure:'SAVE_PRIVATE_PIPES',settled}});}};
    const stop=code=>{failure||=code;try{process.kill(-ch.pid,'SIGKILL');}catch{}settle||=setTimeout(()=>finish(false),2000);};
    const timer=setTimeout(()=>stop('HOST_DATA_DEADLINE'),timeout);
    ch.stdout.on('data',b=>{if(ended)return;out=Buffer.concat([out,b.subarray(0,Math.max(0,65536-out.length))]);if(out.length>=65536)stop('HOST_STDOUT_BOUND');});
    ch.stderr.on('data',b=>{if(ended)return;err=Buffer.concat([err,b.subarray(0,Math.max(0,8192-err.length))]);if(err.length>=8192)stop('HOST_STDERR_BOUND');});
    ch.on('error',()=>{failure||='HOST_DRIVER_SPAWN';});ch.on('exit',(code,sig)=>{status=code;signal=sig;});ch.on('close',()=>finish(true));
  });
}

export async function main(argv,{runCommand=command,readInputs=loadInputs,runPublication=runNode,pause=sleep,emit=console.log}={}) {
  const value=options(argv);if(!value.run)return {kind:ROLE,status:'prepared-no-run',providerCalls:0,providerWrites:0,dnsCalls:0,productChanges:false,
    usage:'--run --module-root /workspace/home/.cache/rudder-release-build/alpha14-tagged-REV7 --version 4.0.0-alpha.14 --revision SHA40 --image registry.fly.io/rudder-workers-exla:4.0.0-alpha.14 --digest sha256:HEX --control-id ID14 --control-revision SHA40 [--nonce UUID]'};
  const started=Date.now(),mainDeadline=started+MAIN_MS,lifetimeDeadline=started+LIFETIME_MS,inputs=await readInputs(value);
  const token='rdrw_'+randomBytes(32).toString('base64url'),prepared=plan(value,inputs,token),ownership=ownershipFor(prepared,value);
  const mocked=runCommand!==command||readInputs!==loadInputs||runPublication!==runNode||pause!==sleep;
  const directory=path.join(HERE,(mocked?'mock-':'run-')+value.nonce);await fs.mkdir(directory,{mode:0o700});const reportPath=path.join(directory,'report.json');
  const report={formatVersion:1,kind:ROLE,nonce:value.nonce,name:prepared.name,version:value.version,revision:value.revision,expectedDigest:value.digest,
    requestedImage:value.image,controlId:value.controlId,declaredControlRevision:value.controlRevision,sourceEnvelopeSha256:inputs.sourceEnvelopeSha256,
    taggedSourceProof:inputs.taggedSourceProof??null,runtimeEnvelopeSha256:inputs.runtimeEnvelopeSha256??null,
    sourceHashes:inputs.hashes,fixtureSha256:FIXTURE_SHA,fixtureObjects:13,fixtureBytes:inputs.expected.totalBytes,syntheticAccount:prepared.auth.account,
    syntheticWorkspace:prepared.auth.workspace,testOnlyMock:mocked,qualified:false,completed:false,syntheticDataOnly:true,activeProducerReceiptQualified:false,
    currentGrantQualified:false,sourceAcceptanceQualified:false,runnableExecutionQualified:false,modelCalls:false,providerAccountCalls:false,
    immutableS3ObjectsRetained:true,haRpoQualified:false,steps:[],startedAt:new Date(started).toISOString(),maxControllerLifetimeMs:LIFETIME_MS};
  const save=async(name,body)=>fs.writeFile(path.join(directory,name),body,{mode:0o600,flag:'wx'});
  await save('preload.mjs',prepared.source);await save('machine.json',JSON.stringify(prepared.config,null,2)+'\n');
  if(inputs.release)await save('frozen-tagged-inputs.bound.json',inputs.release.bytes);
  await save('synthetic-auth-identity.private.json',JSON.stringify(prepared.auth,null,2)+'\n');
  report.controllerSha256=sha(await fs.readFile(fileURLToPath(import.meta.url)));report.helperSha256=sha(await fs.readFile(path.join(HERE,'helpers.mjs')));
  report.preloadSha256=sha(Buffer.from(prepared.source));
  let phase='preflight',serial=0,cleanup=false,creationAttempted=false,authAttempted=false,rowid=null,controlDescriptor,providerCalls=0;
  const progress=step=>emit(JSON.stringify({step,kind:ROLE,nonce:value.nonce,name:prepared.name,report:reportPath}));
  const call=async(args,label,timeout=20000,max=4*MAX_PROOF,owned=false)=>{
    phase=label;const remaining=(cleanup?lifetimeDeadline:mainDeadline)-Date.now();assert(remaining>0);const prefix=path.join(directory,String(++serial).padStart(3,'0')+'-'+label);
    if(!mocked)providerCalls++;const response=await runCommand(args,{timeout:Math.min(timeout,remaining),stdoutLimit:max,
      diagnosticPath:prefix+'-stderr.private',ownedStdoutPath:owned?prefix+'-stdout.private':undefined});
    // Injected test commands must satisfy the same preserve-before-parse rule.
    if(owned&&!response.diagnostics?.privateStdoutFile)Object.assign(response.diagnostics??={},await preserveOwnedExecOutput(Buffer.from(response.stdout||''),Buffer.alloc(0),prefix+'-stdout.private',prefix+'-stderr.private'));
    const step={step:label,ok:response.ok,...response.diagnostics};report.steps.push(step);assert(response.ok);return owned?parseOwnedExecJson(response.stdout,step):response.stdout;
  };
  const inventory=async app=>{const data=JSON.parse(await call(['machine','list','-a',app,'--json'],app===APP?'worker-inventory':'control-inventory'));assert(Array.isArray(data)&&data.length<=10000);return data;};
  const noSecrets=async()=>assert.deepEqual(JSON.parse(await call(['secrets','list','-a',APP,'--json'],'assert-empty-worker-secrets')),[]);
  const controlGuard=async()=>{
    const list=await inventory(CONTROL),found=list.filter(m=>m.id===value.controlId);assert.equal(found.length,1);const m=found[0];assert.notEqual(m.id,BLOCKED_ID);assert.notEqual(m.instance_id,BLOCKED_INSTANCE);
    assert.equal(m.state,'started');assert.match(m.image_ref?.digest||'',DIGEST);assert.equal(m.image_ref?.labels?.['org.opencontainers.image.revision'],value.controlRevision);assert(typeof m.config?.image==='string'&&m.config.image.length<512);assert(typeof m.instance_id==='string'&&m.instance_id.length>0);
    // Auth is SQLite-local. A second routable instance could receive the HTTP
    // request without this nonce row; never silently change that topology.
    assert.equal(list.filter(n=>n.state!=='destroyed').length,1);
    const descriptor={id:m.id,instance:m.instance_id,image:m.config.image,digest:m.image_ref.digest,ociRevision:m.image_ref.labels['org.opencontainers.image.revision']};
    if(controlDescriptor)assert.deepEqual(descriptor,controlDescriptor);else controlDescriptor=descriptor;
    return descriptor;
  };
  const controlExec=async(source,label)=>{await controlGuard();return call(['machine','exec',value.controlId,'-a',CONTROL,'/bin/sh -c '+quote('/usr/local/bin/node -e '+quote(source)),'--timeout','15'],label,20000,65536,true);};
  const ownedExec=async(source,label,max=65536)=>{assert(creationAttempted&&ownership.id());const found=(await inventory(APP)).filter(m=>m.id===ownership.id());assert.equal(found.length,1);assert(ownership.matches(found[0]));
    return call(['machine','exec',ownership.id(),'-a',APP,'/bin/sh -c '+quote('/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e '+quote(source)),'--timeout','15'],label,20000,max,true);};
  try{
    progress('preflight-source-and-control');await noSecrets();const before=await inventory(APP);assert(!before.some(m=>m.name===prepared.name||m.config?.metadata?.qualification===value.nonce));
    const controlHashes=Object.fromEntries(Object.entries(inputs.hashes).filter(([p])=>p.startsWith('cloud/')));
    const probe=await controlExec(controlProbeSource({nonce:value.nonce,hashes:controlHashes}),'verify-control-runtime-source');assert.equal(probe.status,'passed');assert.equal(probe.nonce,value.nonce);assert.deepEqual(probe.hashes,controlHashes);assert.equal(probe.durableStoreEnabled,true);
    assert(['/data/rudder-cloud.sqlite','/tmp/rudder-cloud/rudder-cloud.sqlite','/root/.rudder-cloud/rudder-cloud.sqlite'].includes(probe.databasePath));prepared.auth.databasePath=probe.databasePath;report.control={...controlDescriptor,sourceHashes:probe.hashes,durableStoreEnabled:true};
    const absent=await controlExec(authSource(prepared.auth,'inspect'),'assert-synthetic-auth-absent');assert.equal(absent.status,'absent');assert.equal(absent.nonce,value.nonce);
    authAttempted=true;const created=await controlExec(authSource(prepared.auth,'create'),'create-exact-synthetic-auth');assert.equal(created.status,'created');assert.equal(created.nonce,value.nonce);assert.match(created.rowid,/^[1-9][0-9]{0,18}$/);rowid=created.rowid;report.syntheticAuthCreated=true;
    phase='real-cloud-publication';const source='('+hostPublication.toString()+')('+JSON.stringify({moduleUrl:pathToFileURL(path.join(value.root,'dist/fabric-remote.js')).href,
      fixturePath:path.join(HERE,'fixture-v1.json'),fixtureSha256:FIXTURE_SHA,baseUrl:BASE,workspace:prepared.auth.workspace})+').catch(e=>{console.error(JSON.stringify({status:"failed",step:"HOST_DATA_PUBLICATION",code:typeof e.code==="string"?e.code:"ASSERTION",httpStatus:Number.isInteger(e.status)?e.status:null}));process.exitCode=1;});';
    await save('host-publication.mjs',source);if(!mocked)providerCalls++;const pub=await runPublication(source,{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:directory,TMPDIR:directory,LANG:'C',LC_ALL:'C',PCD_BEARER:token},
      {timeout:Math.min(150000,mainDeadline-Date.now()),stdoutPath:path.join(directory,'host-publication.stdout.private'),stderrPath:path.join(directory,'host-publication.stderr.private')});
    if(!pub.diagnostics?.privateStdoutFile)Object.assign(pub.diagnostics??={},await preserveOwnedExecOutput(Buffer.from(pub.stdout||''),Buffer.alloc(0),path.join(directory,'host-publication.stdout.private'),path.join(directory,'host-publication.stderr.private')));
    report.hostPublicationProcess=pub.diagnostics;assert(pub.ok&&pub.diagnostics.settled);const published=parseOwnedExecJson(pub.stdout,report.hostPublicationProcess);
    assert.equal(published.status,'passed');assert.equal(published.kind,'synthetic-checkpoint-data-publication');assert.equal(published.bundle.root_id,inputs.expected.publicationId);assert.deepEqual(published.bundle.root,inputs.expected.publication);assert.equal(published.bundle.object_count,13);assert.equal(published.bundle.total_bytes,12953);
    assert.equal(published.uploadedObjects,9);assert.equal(published.exactObjectReads,13);
    assert.equal(published.rawReceiptSha256,inputs.expected.receiptSha256);for(const k of ['opaqueHighU64Receipt','sameIdGetReconciled','syntheticDataOnly'])assert.equal(published[k],true);
    assert.equal(published.activeProducerReceiptQualified,false);assert.equal(published.currentGrantQualified,false);report.publication=published;progress('synthetic-data-published');
    await noSecrets();const secondBefore=await inventory(APP);assert(!secondBefore.some(m=>m.name===prepared.name||m.config?.metadata?.qualification===value.nonce));
    creationAttempted=true;try{await call(['machine','run',value.image,'-a',APP,'--machine-config',path.join(directory,'machine.json'),'--name',prepared.name,'--region',REGION,'--detach','--skip-dns-registration'],'create-exact-stock-canary',70000);}catch{report.createReplyUncertain=true;}
    let machine,startRequested=false;for(let n=0;n<80&&Date.now()<mainDeadline;n++){
      const matches=(await inventory(APP)).filter(ownership.matches);assert(matches.length<=1);if(matches.length){machine=matches[0];report.createdDescriptor=ownership.remember(machine);
        if(machine.state==='stopped')throw Error('owned canary exited before admission');if(machine.state==='started')break;if(machine.state==='created'&&!startRequested){startRequested=true;try{await call(['machine','start',ownership.id(),'-a',APP],'start-reconciled-exact-canary',25000);}catch{report.startReplyUncertain=true;}}}
      await pause(1000);
    }
    assert.equal(machine?.state,'started');assert.equal(machine.image_ref?.digest,value.digest);assert.equal(machine.image_ref?.labels?.['org.opencontainers.image.revision'],value.revision);report.resolvedDigestVerified=true;await noSecrets();
    const probeSource=`const fs=require('node:fs'),crypto=require('node:crypto'),assert=require('node:assert/strict');assert.equal(process.getuid(),0);console.log(JSON.stringify((${runtimeCheck.toString()})(${JSON.stringify(prepared.input)},fs,crypto,assert,${protectedRead.toString()})));`;
    const runtime=await ownedExec(probeSource,'verify-owned-stock-runtime');assert.equal(runtime.workerVersion,value.version);assert.deepEqual(runtime.moduleHashes,prepared.input.workerHashes);assert.match(runtime.nativeSha256,HEX);report.rootRuntime=runtime;
    const gate=await ownedExec(gateSource(prepared.input),'publish-atomic-data-admission-gate',4096);assert.equal(gate.nonce,value.nonce);assert.equal(gate.published,true);report.admitted=true;
    let fetched;for(let n=0;n<100&&Date.now()<mainDeadline;n++){fetched=await ownedExec(proofReadSource(prepared.input),'read-owned-data-proof',MAX_PROOF+16384);if(fetched.status==='ready')break;assert.equal(fetched.status,'pending');await pause(1500);}
    assert.equal(fetched?.status,'ready');const bytes=Buffer.from(JSON.stringify(fetched.proof,null,2)+'\n');assert.equal(sha(bytes),fetched.sha256);await save('guest-proof.json',bytes);
    validateGuest(fetched.proof,value,prepared);assert.equal(fetched.proof.runtime.nativeSha256,runtime.nativeSha256);report.guestProofSha256=fetched.sha256;
    if(inputs.release){phase='verify-tagged-source-after-smoke';report.taggedSourceUnchanged=await verifySourceMap(value.root,inputs.release.meta);}
    report.completed=true;report.qualified=!mocked;report.mockPassed=mocked;progress(mocked?'mock-data-path-passed':'data-path-passed');
  }catch(error){report.completed=false;report.qualified=false;report.failedPhase=phase;report.errorCode=typeof error.code==='string'?error.code:'ASSERTION';
    const detail=privateErrorDetail(error);await save('controller-failure.private',detail.formatted.subarray(0,8192));progress('failed-private-evidence-retained');
  }finally{
    cleanup=true;report.creationAttempted=creationAttempted;report.authCreationAttempted=authAttempted;
    try{
      const absent=list=>!list.some(m=>m.id===ownership.id()||m.name===prepared.name||m.config?.metadata?.qualification===value.nonce);
      if(creationAttempted){const matches=(await inventory(APP)).filter(ownership.matches);assert(matches.length<=1);for(const m of matches){report.createdDescriptor=ownership.remember(m);
        try{await call(['machine','destroy',ownership.id(),'-a',APP,'--force'],'destroy-exact-owned-canary',15000);}catch{report.destroyReplyUncertain=true;}}}
      else report.noDestroyWithoutCreateAttempt=true;
      let list=await inventory(APP);for(let n=0;creationAttempted&&!absent(list)&&n<20&&Date.now()<lifetimeDeadline-50000;n++){await pause(500);list=await inventory(APP);}
      assert(absent(list));assert(absent(await inventory(APP)));assert(!(report.createReplyUncertain&&!ownership.id()));report.cleanupVerified=true;report.independentAbsence=true;
    }catch{report.cleanupVerified=false;report.independentAbsence=false;}
    try{
      if(authAttempted){try{const removed=await controlExec(authSource(prepared.auth,'remove',rowid),'remove-exact-synthetic-auth');assert.equal(removed.status,'absent');assert.equal(removed.nonce,value.nonce);}catch{report.authDeleteReplyUncertain=true;}
        const first=await controlExec(authSource(prepared.auth,'absent'),'verify-synthetic-auth-absence');const second=await controlExec(authSource(prepared.auth,'absent'),'independent-synthetic-auth-absence');assert.equal(first.status,'absent');assert.equal(second.status,'absent');
        report.syntheticCredentialRemoved=true;report.independentCredentialAbsence=true;
      }else{report.noCredentialMutationWithoutCreateAttempt=true;report.syntheticCredentialRemoved=true;report.independentCredentialAbsence=true;}
    }catch{report.syntheticCredentialRemoved=false;report.independentCredentialAbsence=false;}
    const proof={nonce:value.nonce,name:prepared.name,role:ROLE,ownedId:ownership.id()??null,creationAttempted,authAttempted,canaryAbsent:report.cleanupVerified,
      independentCanaryAbsent:report.independentAbsence,syntheticCredentialRemoved:report.syntheticCredentialRemoved,independentCredentialAbsence:report.independentCredentialAbsence,
      immutableS3ObjectsRetained:true,verifiedAt:new Date().toISOString()};await save('cleanup.json',JSON.stringify(proof,null,2)+'\n');
    report.elapsedMs=Date.now()-started;report.withinLifetimeBound=report.elapsedMs<LIFETIME_MS;report.providerCalls=providerCalls;report.verifiedAt=new Date().toISOString();
    // A completed data path with failed cleanup is never reported qualified.
    report.qualified=report.qualified&&report.cleanupVerified&&report.independentAbsence&&report.syntheticCredentialRemoved&&report.independentCredentialAbsence&&report.withinLifetimeBound;
    if(mocked)report.mockPassed=report.mockPassed===true&&report.cleanupVerified&&report.independentAbsence&&report.syntheticCredentialRemoved&&report.independentCredentialAbsence&&report.withinLifetimeBound;
    await save('report.json',JSON.stringify(report,null,2)+'\n');progress('finished');
  }
  return {reportPath,qualified:report.qualified,mockPassed:report.mockPassed===true,testOnlyMock:mocked,cleanupVerified:report.cleanupVerified,
    independentAbsence:report.independentAbsence,syntheticCredentialRemoved:report.syntheticCredentialRemoved,independentCredentialAbsence:report.independentCredentialAbsence,
    withinLifetimeBound:report.withinLifetimeBound,providerCalls};
}

if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  try{const result=await main(process.argv.slice(2));console.log(JSON.stringify(result));if(result.status!=='prepared-no-run'&&!result.qualified)process.exitCode=1;}
  catch(error){console.log(JSON.stringify({kind:ROLE,status:'rejected-before-execution',errorCode:typeof error.code==='string'?error.code:'ASSERTION'}));process.exitCode=1;}
}
