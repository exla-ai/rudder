import fs from 'node:fs/promises';
import path from 'node:path';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import assert from 'node:assert/strict';
import { FabricRemoteClient } from '../alpha13-030ebef/dist/fabric-remote.js';

const exec=promisify(execFile),fly='/workspace/home/.fly/bin/flyctl';
const nonce=randomUUID(),account='qualification:alpha13:'+nonce,workspace='qualification-alpha13-'+nonce;
const token='rdrw_'+randomBytes(32).toString('base64url');
const hash=b=>createHash('sha256').update(b).digest('hex'),tokenHash=hash(token).slice(0,32);
const app='rudder-workers-exla',control='8911d61a434978',name='rudder-alpha13-cas-'+nonce.slice(0,8);
const image='registry.fly.io/rudder-workers-exla:4.0.0-alpha.13';
const digest=process.argv[2];
assert.match(digest,/^sha256:[0-9a-f]{64}$/);
const directory='/workspace/home/.cache/rudder-release-build/alpha13-cloud-smoke-'+nonce;
await fs.mkdir(directory,{mode:0o700});
const report={format_version:1,provider:'Fly.io / AWS S3',version:'4.0.0-alpha.13',
  revision:'030ebeff621089649c0cfb50e55f8fc9c7642d61',name,nonce,image,imageDigest:digest,
  rootId:'alpha13-runnable-'+nonce,steps:[],limits:[
    'Small synthetic immutable fixture, not a cold-host performance workload or agent launch.',
    'Producer bytes are a bounded in-memory fixture; worker restore uses the shipped native CAS and durable publication.',
    'Only a generated synthetic account/workspace credential is used. No user account, vault or AWS key enters the canary.',
    'Published synthetic immutable objects remain retained by the fabric no-GC policy.'
  ]};
const emit=v=>console.log(JSON.stringify({at:new Date().toISOString(),...v}));
const delay=ms=>new Promise(r=>setTimeout(r,ms));
const quote=s=>"'"+s.replaceAll("'","'\\''")+"'";
async function command(args,label,timeout=60000){
 const start=Date.now();const r=await exec(fly,args,{timeout,maxBuffer:4*1024*1024});
 report.steps.push({step:label,ms:Date.now()-start});return r.stdout;
}
async function machines(){return JSON.parse(await command(['machine','list','-a',app,'--json'],'inventory'));}
async function authRow(remove){
 // Only hashes/nonce identities cross exec argv; the bearer value stays private.
 const code=`const D=require('/app/node_modules/better-sqlite3');const db=new D('/tmp/rudder-cloud/rudder-cloud.sqlite');const id=${JSON.stringify(workspace)},account=${JSON.stringify(account)},hash=${JSON.stringify(tokenHash)},key=${JSON.stringify('qualification:'+nonce)};const row=db.prepare('select account_id,workspace_key,worker_token_hash from rudder_workspaces where id=?').get(id);if(row&&(row.account_id!==account||row.workspace_key!==key||row.worker_token_hash!==hash))throw Error('qualification identity conflict');${remove?"if(row)db.prepare('delete from rudder_workspaces where id=? and account_id=? and workspace_key=? and worker_token_hash=?').run(id,account,key,hash);":"if(!row){const now=new Date().toISOString();db.prepare(\"insert into rudder_workspaces(id,account_id,workspace_key,status,worker_token_hash,created_at,updated_at)values(?,?,?,'stopped',?,?,?)\").run(id,account,key,hash,now,now);}"}const exists=Boolean(db.prepare('select id from rudder_workspaces where id=?').get(id));db.close();console.log(JSON.stringify({exists}));`;
 const value=JSON.parse((await command(['machine','exec',control,'-a','rudder-cloud-control',
  'node -e '+quote(code),'--timeout','15'],remove?'remove-synthetic-worker-auth':'create-synthetic-worker-auth',25000)).trim());
 assert.equal(value.exists,!remove);return value;
}
const canonical=v=>typeof v==='bigint'?v.toString():Array.isArray(v)?'['+v.map(canonical).join(',')+']':v&&typeof v==='object'?'{'+Object.keys(v).sort().map(k=>JSON.stringify(k)+':'+canonical(v[k])).join(',')+'}':JSON.stringify(v);
const objects=new Map();
function object(kind,bytes){const ref={kind,algorithm:'sha256',digest:hash(bytes),size_bytes:bytes.length};objects.set(ref.digest,{ref,bytes});return ref;}
const meta=(kind,v)=>object(kind,Buffer.from(canonical(v))),blob=b=>object('blob',Buffer.from(b));
const chunks=[blob(Buffer.alloc(4*1024*1024,41)),blob(Buffer.alloc(4*1024*1024,42)),blob(Buffer.alloc(1024*1024,43))];
const large=meta('file',{format_version:1,layout:'fixed_4mib_fanout256_v1',level:0,logical_size:9*1024*1024,children:chunks});
const small=blob('small immutable fixture\n');
const entries=[{name_hex:Buffer.from('large.bin').toString('hex'),node:{type:'file',object:large,logical_size:9*1024*1024,mode:0o750}},
 {name_hex:Buffer.from('link').toString('hex'),node:{type:'symlink',target_hex:Buffer.from('large.bin').toString('hex')}},
 {name_hex:'ff61',node:{type:'file',object:small,logical_size:small.size_bytes,mode:0o640}}];
function tree(entries){const summary={entry_count:entries.length,subdirectory_count:0,entries_bytes:entries.reduce((n,e)=>n+Buffer.byteLength(canonical(e)),0)};
 const shard=meta('directory_shard',{format_version:1,prefix_hex:'',summary,node:{type:'leaf',entries}});
 return meta('directory',{format_version:2,mode:0o755,entry_count:entries.length,subdirectory_count:0,directory_shard:shard});}
const view=tree(entries),empty=tree([]),policy=blob('{"excluded_paths":[]}'),profile=blob('{"format_version":1,"partitions":[]}'),hardlinks=blob('{"format_version":1,"groups":[]}');
const source=meta('source_root',{format_version:2,consistency:'quiesced',directory:view,projection_policy:policy});
const environment=meta('environment_root',{format_version:2,directory:empty,platform:{os:'linux',arch:'x86_64'},installer_inputs:[],provenance:{fixture:nonce}});
const root=meta('runnable_root',{format_version:1,consistency:'quiesced',source,environment,view_root:view,profile,hardlinks,knowledge_cursor:18446744073709551615n});
report.fixture={root,logicalFileBytes:9*1024*1024,objects:objects.size,encodedBytes:[...objects.values()].reduce((n,o)=>n+o.bytes.length,0),largeFileSha256:hash(Buffer.concat(chunks.map(r=>objects.get(r.digest).bytes))),rawNameHex:'ff61'};
await fs.writeFile(path.join(directory,'cleanup.json'),JSON.stringify({nonce,account,workspace,tokenHash,name,imageDigest:digest}),{mode:0o600});
if(process.argv.includes('--local-only')){
 const {FabricLocalClient}=await import('../alpha13-030ebef/dist/fabric-local.js');
 const home=path.join(directory,'local-cas');await fs.mkdir(home,{mode:0o700});
 const native='/workspace/code/exla-ai-rudder/target/release/rudder-native';
 const local=await FabricLocalClient.open({home,nativeBinary:native});
 let operation;
 try{
  emit({step:'local-native-capabilities',capabilities:await local.request({operation:'capabilities'})});
  const staging=await local.beginStaging(300000);
  for(const item of objects.values()){
   operation={operation:'receive',object:item.ref,bytes_hex:item.bytes.toString('hex')};
   await local.receive(staging.lease_id,item.ref,item.bytes);
   emit({step:'local-receive',kind:item.ref.kind,bytes:item.ref.size_bytes});
  }
  operation={operation:'publish',lease_id:staging.lease_id,pin:'alpha13-local-fixture',root};
  emit({step:'local-publication',receipt:await local.publish(staging.lease_id,'alpha13-local-fixture',root)});
  await assert.rejects(local.read(root));
  await local.pin(root);assert.equal((await local.closure(root)).length,objects.size);
  assert.equal(hash(await local.read(root)),root.digest);
  emit({step:'local-scoped-source-read',ok:true});
 }catch(error){
  emit({step:'local-failure',code:error.code,operation:operation?.operation,kind:operation?.object?.kind});
  if(operation){try{await exec(native,['fabric'],{env:{...process.env,RUDDER_HOME:home},timeout:10000,maxBuffer:1024*1024,input:JSON.stringify({format_version:1,request:operation})});}
   catch(e){emit({step:'direct-diagnostic',message:String(e.stderr||e.message).slice(-2000)});}}
  process.exitCode=1;
 }finally{await local.close();}
 process.exit(process.exitCode||0);
}
let machine;
try{
 emit({step:'owned-canary-intent',name,nonce});
 assert.equal(JSON.parse(await command(['secrets','list','-a',app,'--json'],'no-app-secrets')).length,0);
 await authRow(false);
 const remote=new FabricRemoteClient({baseUrl:'https://rudder-cloud-control.fly.dev/api/rudder/fabric',authHeaders:()=>({Authorization:'Bearer '+token,'X-Rudder-Workspace-Id':workspace}),concurrency:2});
 const pin='fixture:'+nonce;let pinned=false;
 const transferSource={pin:async r=>{assert.deepEqual(r,root);assert(!pinned);pinned=true;return pin;},unpin:async p=>{assert.equal(p,pin);assert(pinned);pinned=false;},closure:async()=>[...objects.values()].map(o=>o.ref),read:async r=>objects.get(r.digest).bytes};
 emit({step:'publish-synthetic-runnable',objects:objects.size});
 report.publication=await remote.uploadRoot(report.rootId,root,transferSource);assert(!pinned);assert.deepEqual(report.publication.receipt.root,root);
 const repeated=await remote.getRoot(report.rootId);assert.deepEqual(repeated,report.publication.receipt);
 emit({step:'replicated-fixture-published',objects:report.publication.receipt.object_count});
 const guest=`import fs from 'node:fs/promises';import assert from 'node:assert/strict';import{createHash}from'node:crypto';
import{FabricLocalClient}from'file:///opt/rudder/dist/fabric-local.js';import{FabricRemoteClient}from'file:///opt/rudder/dist/fabric-remote.js';
const expected=${JSON.stringify(report.fixture)},rootId=${JSON.stringify(report.rootId)};let client;let proof;let phase='native-store';
try{assert.equal(process.getuid(),1500);assert.equal(process.env.RUDDER_WORKER_FUSE_READY,'1');
const home=await fs.mkdtemp('/workspace/native-cas-');await fs.chmod(home,0o700);client=await FabricLocalClient.open({home,nativeBinary:'/opt/rudder/dist/native/rudder-native'});
assert.deepEqual((await client.request({operation:'retention'})).pins,{});
const remote=new FabricRemoteClient({baseUrl:'https://rudder-cloud-control.fly.dev/api/rudder/fabric',authHeaders:()=>({Authorization:'Bearer '+process.env.RUDDER_WORKER_TOKEN,'X-Rudder-Workspace-Id':process.env.RUDDER_WORKSPACE_ID}),concurrency:2});
phase='native-restore';const start=Date.now();const downloaded=await remote.downloadRoot(rootId,client,'alpha13-restored');assert.deepEqual(downloaded.receipt.root,expected.root);assert.equal(downloaded.downloaded_objects,expected.objects);
phase='native-read-pin';await client.pin(expected.root);assert.equal((await client.closure(expected.root)).length,expected.objects);
const raw=await client.read(expected.root);assert(raw.includes(Buffer.from('"knowledge_cursor":18446744073709551615')));const rootMetadata=JSON.parse(raw);
phase='native-exact-content';const dir=JSON.parse(await client.read(rootMetadata.view_root));assert.equal(dir.format_version,2);const page=JSON.parse(await client.read(dir.directory_shard));
const largeEntry=page.node.entries.find(e=>e.name_hex===Buffer.from('large.bin').toString('hex'));assert.equal(largeEntry.node.mode,0o750);
const file=JSON.parse(await client.read(largeEntry.node.object));assert.equal(file.layout,'fixed_4mib_fanout256_v1');
const blocks=[];for(const ref of file.children)blocks.push(await client.read(ref));const big=Buffer.concat(blocks);assert.equal(big.length,expected.logicalFileBytes);assert.equal(createHash('sha256').update(big).digest('hex'),expected.largeFileSha256);
assert.equal(page.node.entries.find(e=>e.name_hex===Buffer.from('link').toString('hex')).node.target_hex,Buffer.from('large.bin').toString('hex'));
const rawEntry=page.node.entries.find(e=>e.name_hex===expected.rawNameHex);assert.equal((await client.read(rawEntry.node.object)).toString(),'small immutable fixture\\n');
phase='native-restart';
await client.close();client=await FabricLocalClient.open({home,nativeBinary:'/opt/rudder/dist/native/rudder-native'});
assert.deepEqual((await client.request({operation:'retention'})).pins['alpha13-restored'],expected.root);
await client.pin(expected.root);assert.equal((await client.closure(expected.root)).length,expected.objects);assert((await client.read(expected.root)).equals(raw));
phase='repeat-restore';const cached=await remote.downloadRoot(rootId,client,'alpha13-restored-again');assert.equal(cached.downloaded_objects,expected.objects);
proof={ok:true,uid:1500,node:process.version,root:expected.root,downloadedObjects:downloaded.downloaded_objects,downloadedBytes:downloaded.downloaded_bytes,smallFixtureElapsedMs:Date.now()-start,
 nativeRestore:true,retainedAcrossNativeRestart:true,secondRestoreDownloadedObjects:cached.downloaded_objects,exactU64Bytes:true,chunkedFileSha256:expected.largeFileSha256,rawNameMetadata:true,fileModeMetadata:true,symlinkMetadata:true,filesystemMaterialization:false};
}catch(error){proof={ok:false,phase,code:String(error.code||error.name),message:String(error.message).replace(/rdrw_[A-Za-z0-9_-]+/g,'[redacted]').slice(0,1500)};}
finally{if(client)await client.close().catch(()=>{});}
await fs.writeFile('/workspace/alpha13-cloud-proof.json',JSON.stringify(proof),{mode:0o600});console.log(JSON.stringify(proof));setTimeout(()=>process.exit(0),480000);await new Promise(()=>{});`;
 const config=path.join(directory,'machine.json');await fs.writeFile(config,JSON.stringify({image,guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},restart:{policy:'no'},services:[],auto_destroy:false,
  env:{RUDDER_HOME:'1',RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',RUDDER_WORKER_TOKEN:token,RUDDER_WORKSPACE_ID:workspace,NODE_OPTIONS:'--import=data:text/javascript;base64,'+Buffer.from(guest).toString('base64')},metadata:{rudder_role:'alpha13-cloud-smoke',qualification:nonce}}),{mode:0o600});
 await fs.writeFile(path.join(directory,'guest.mjs'),guest,{mode:0o600});
 try{await command(['machine','run',image,'-a',app,'--machine-config',config,'--name',name,'--region','sjc','--detach','--skip-dns-registration'],'create-owned-worker',120000);}catch(error){report.createError=String(error.stderr||error.message).replace(/rdrw_[A-Za-z0-9_-]+/g,'[redacted]').slice(-1500);}
 const deadline=Date.now()+180000;let started=false;
 while(Date.now()<deadline){const owned=(await machines()).filter(m=>m.name===name&&m.config?.metadata?.qualification===nonce);assert(owned.length<=1);machine=owned[0];
  if(machine?.state==='started')break;
  if(machine?.state==='stopped'){report.earlyExit=machine.events?.find(e=>e.type==='exit')?.request?.exit_event;throw new Error('canary exited before returning its proof');}
  if(machine?.state==='created'&&!started){started=true;await command(['machine','start',machine.id,'-a',app],'start-reconciled-worker',90000).catch(()=>{});}await delay(3000);}
 assert.equal(machine?.state,'started');assert.equal(machine.image_ref.digest,digest);assert(!machine.config.mounts?.length&&!machine.config.services?.length);
 report.machine=machine.id;report.instance=machine.instance_id;emit({step:'fresh-native-worker',machine:machine.id});
 let proof;for(let n=0;n<30;n++){try{proof=JSON.parse((await command(['machine','exec',machine.id,'-a',app,'/bin/cat /workspace/alpha13-cloud-proof.json','--timeout','10'],'read-owned-proof',20000)).trim());break;}catch{}await delay(3000);}
 assert.equal(proof?.ok,true,JSON.stringify(proof||'worker proof unavailable'));report.worker=proof;report.ok=true;
}catch(error){report.ok=false;report.error=String(error.message).replace(/rdrw_[A-Za-z0-9_-]+/g,'[redacted]').slice(0,2000);process.exitCode=1;}
finally{
 try{for(const m of await machines())if(m.name===name&&m.config?.metadata?.qualification===nonce)await command(['machine','destroy',m.id,'-a',app,'--force'],'destroy-owned-worker');report.cleanupVerified=!(await machines()).some(m=>m.name===name&&m.config?.metadata?.qualification===nonce);}catch{report.cleanupVerified=false;process.exitCode=1;}
 try{await authRow(true);report.syntheticCredentialRemoved=true;}catch{report.syntheticCredentialRemoved=false;process.exitCode=1;}
 report.verifiedAt=new Date().toISOString();await fs.writeFile(path.join(directory,'report.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});
 emit({step:'cloud-smoke-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,syntheticCredentialRemoved:report.syntheticCredentialRemoved,report:path.join(directory,'report.json')});
}
