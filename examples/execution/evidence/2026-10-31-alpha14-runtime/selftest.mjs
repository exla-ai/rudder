// LOCAL ONLY. No provider calls, DNS, SDK/model auth, Cargo, HOME/profile reads
// or namespace changes. Injected transport is always marked testOnlyMock.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import vm from 'node:vm';
import crypto from 'node:crypto';
import {createRequire} from 'node:module';
import {EventEmitter} from 'node:events';
import {fileURLToPath} from 'node:url';
import {main,options,fixtureBundle,plan,ownershipFor,validateGuest,hostPublication,nativeRestore,guestSource,
  protectedRead,controlProbeSource,authSource,runNode,taggedMetadata,verifySourceMap,ROLE} from './qualify-partial-cloud.mjs';
import {parseProcStatus,heldNamespaceWitness,gateSource,parseOwnedExecJson,preserveOwnedExecOutput} from './helpers.mjs';
const HERE=path.dirname(fileURLToPath(import.meta.url)),hash=b=>crypto.createHash('sha256').update(b).digest('hex');
const normalize=x=>JSON.parse(JSON.stringify(x)),clone=x=>structuredClone(x);
const fixtureBytes=await fsp.readFile(path.join(HERE,'fixture-v1.json')),bundle=fixtureBundle(fixtureBytes);
const rev='1'.repeat(40),digest='sha256:'+'2'.repeat(64),controlId='1234567890abcd';
const argv=['--run','--module-root','/workspace/home/.cache/rudder-release-build/alpha14-tagged-'+rev.slice(0,7),
  '--version','4.0.0-alpha.14','--revision',rev,'--image','registry.fly.io/rudder-workers-exla:4.0.0-alpha.14','--digest',digest,
  '--control-id',controlId,'--control-revision',rev,'--nonce','00000000-0000-0000-0000-000000000001'];
const hashes=Object.fromEntries(['dist/fabric-remote.js','dist/fabric-local.js','dist/native-binary.js','cloud/src/server.ts','cloud/dist/server.js',
  'cloud/src/fabric-partial-checkpoint.ts','cloud/dist/fabric-partial-checkpoint.js'].map(p=>[p,'3'.repeat(64)]));
const inputs={...bundle,hashes,sourceEnvelopeSha256:hash(Buffer.from(JSON.stringify(hashes)))};
const value=options(argv),prepared=plan(value,inputs,'rdrw_'+'A'.repeat(43),'2026-10-31T00:00:00.000Z');
const cases=[];const check=(name,fn)=>{fn();cases.push(name);};
assert.deepEqual(options([]),{run:false});assert.deepEqual(options(['--describe']),{run:false});
assert.equal((await main([])).providerCalls,0);assert.equal((await main(['--describe'])).providerWrites,0);cases.push('inert defaults/describe');
for(const key of ['module-root','version','revision','image','digest','control-id','control-revision']){
  const a=[...argv],i=a.indexOf('--'+key);a.splice(i,2);assert.throws(()=>options(a));}
for(const [key,bad]of [['module-root','/workspace/code/exla-ai-rudder'],['image','registry.fly.io/rudder-workers-exla:latest'],['digest','sha256:BAD'],
  ['control-id','8edd225a7d0378'],['control-revision','f'.repeat(40)],['nonce','00000000-0000-0000-0000-000000000000']]){
  const a=[...argv];a[a.indexOf('--'+key)+1]=bad;assert.throws(()=>options(a));}
cases.push('required exact pins fail closed');
for(const root of ['/workspace/home/.cache/rudder-release-build/alpha14-candidate-96c7b25','/workspace/home/.cache/rudder-release-build/alpha14-1111111']){
  const a=[...argv];a[a.indexOf('--module-root')+1]=root;assert.throws(()=>options(a));}
const sourceMap={'tests/fixtures/partial-checkpoint-v1.json':hash(fixtureBytes)},metadata={formatVersion:1,sourceKind:'tagged',version:value.version,packageVersion:value.version,
  revision:value.revision,frozenRoot:value.root,archiveSha256:'6'.repeat(64),sourceEnvelopeEncoding:'sha256 of sorted compact JSON path-to-sha256 map',matches:0,
  source:sourceMap,sourceFiles:1,sourceBytes:fixtureBytes.length,sourceEnvelopeSha256:hash(Buffer.from(JSON.stringify(sourceMap)))};
assert.equal(taggedMetadata(value,Buffer.from(JSON.stringify(metadata))).meta.sourceKind,'tagged');
for(const change of [m=>m.sourceKind='candidate',m=>m.revision='9'.repeat(40),m=>m.packageVersion='4.0.0-alpha.13',m=>m.sourceEnvelopeSha256='0'.repeat(64),m=>m.sourceFiles=2,
  m=>{m.source['../private']='f'.repeat(64);},m=>m.sourceBytes=256*1024*1024]){const m=clone(metadata);change(m);assert.throws(()=>taggedMetadata(value,Buffer.from(JSON.stringify(m))));}
const tree=await fsp.mkdtemp(path.join(HERE,'source-map-'));await fsp.chmod(tree,0o700);await fsp.mkdir(path.join(tree,'tests/fixtures'),{recursive:true,mode:0o700});
await fsp.writeFile(path.join(tree,'tests/fixtures/partial-checkpoint-v1.json'),fixtureBytes,{mode:0o600});assert.equal((await verifySourceMap(tree,metadata)).verified,true);
await fsp.appendFile(path.join(tree,'tests/fixtures/partial-checkpoint-v1.json'),' ');await assert.rejects(verifySourceMap(tree,metadata));await fsp.rm(tree,{recursive:true});
cases.push('tagged whole-map/envelope/source-byte checks reject candidates and mutations');
check('golden exact13/12953/high-u64',()=>{assert.equal(bundle.expected.objectCount,13);assert.equal(bundle.expected.totalBytes,12953);assert.match(bundle.fixture.receipt_json,/9007199254740995/);});
check('preload parses and stays below environment bound',()=>{new vm.SourceTextModule(guestSource(prepared.input));assert(Buffer.byteLength(prepared.source)<48000);assert(Buffer.byteLength(prepared.config.env.NODE_OPTIONS)<65536);});
// Execute the actual function-toString parser against real own status, then
// explicit tabs/CRLF; catches serialization escape regressions from templates.
const parser=new vm.Script('('+JSON.parse(JSON.stringify(parseProcStatus.toString()))+')').runInNewContext();
const procStatus=fs.readFileSync('/proc/self/status','utf8'),expected={};
for(const line of procStatus.split('\n')){const at=line.indexOf(':');if(at>=0&&['Uid','Gid','Groups','NoNewPrivs','CapEff','CapPrm','CapInh','CapAmb','CapBnd'].includes(line.slice(0,at)))expected[line.slice(0,at)]=line.slice(at+1).trim();}
assert.deepEqual(normalize(parser(procStatus)),expected);assert.equal(parser('Uid:\t1500\t1500\t1500\t1500\r\nGroups:\t\nNoNewPrivs:\t1\n').Uid,'1500\t1500\t1500\t1500');cases.push('serialized real proc parser');
for(const kind of ['user','net','mnt']){const w=heldNamespaceWitness(fs,assert,kind);try{assert.equal(w.inode,fs.statSync('/proc/self/ns/'+kind,{bigint:true}).ino.toString());assert.equal(fs.statfsSync('/proc/self/fd/'+w.fd).type,0x6e736673);}finally{fs.closeSync(w.fd);}}
cases.push('read-only own held NSFS witnesses');

const control={id:controlId,name:'mock-control',instance_id:'mock-control-instance',state:'started',image_ref:{digest:'sha256:'+'4'.repeat(64),labels:{'org.opencontainers.image.revision':rev}},config:{image:'registry.fly.io/rudder-cloud-control:mock@sha256:'+'4'.repeat(64)}};
const machineFor=p=>({id:'abcdef12345678',name:p.name,instance_id:'mock-worker-instance',state:'started',region:'iad',image_ref:{digest,labels:{'org.opencontainers.image.revision':rev}},config:{...clone(p.config),image:p.config.image+'@'+digest}});
check('exact observed ownership, wrong expected digest remains cleanable',()=>{
  const owned=ownershipFor(prepared,value),m=machineFor(prepared);assert(owned.matches(m));owned.remember(m);assert(owned.matches(m));
  for(const mutation of [x=>x.id='8edd225a7d0378',x=>x.instance_id='01M670MG4A8E0DWE9GA1C0PQ92',x=>x.config.image='other',
    x=>x.config.metadata.qualification_revision='bad',x=>x.config.env.OPENAI_API_KEY='foreign',x=>x.config.services=[{}],x=>x.config.mounts=[{}],x=>x.config.files=[{}],x=>x.image_ref.digest='sha256:'+'9'.repeat(64)]){
    const changed=clone(m);mutation(changed);assert(!owned.matches(changed));}
  const wrong=machineFor(prepared);wrong.image_ref.digest='sha256:'+'9'.repeat(64);wrong.config.image=prepared.config.image+'@'+wrong.image_ref.digest;
  const other=ownershipFor(prepared,value);assert(other.matches(wrong));assert.equal(other.remember(wrong).observedDigest,wrong.image_ref.digest);assert(other.matches(wrong));
});
check('owned exec guest exit and parse failures stay failures',()=>{assert.throws(()=>parseOwnedExecJson('Exit code: 5\n{}',{}),{code:'OWNED_GUEST_EXIT_NONZERO'});assert.throws(()=>parseOwnedExecJson('invalid',{}),{code:'OWNED_EXEC_INVALID_JSON'});});
const writes=[];await preserveOwnedExecOutput(Buffer.from('bad-json'),Buffer.from('stderr'),'stdout','stderr',async(...v)=>writes.push(v));
assert.deepEqual(writes.map(x=>x[0]),['stdout','stderr']);assert.deepEqual(writes[0][2],{mode:0o600,flag:'wx'});cases.push('private pipes before parse');

// Real SQLite on a disposable private path exercises the generated exact-row
// mutation code, including rollback/refusal. Never opens the control database.
const require=createRequire(import.meta.url),D=require('/workspace/code/exla-ai-rudder/cloud/node_modules/better-sqlite3');
const sqlDir=await fsp.mkdtemp(path.join(HERE,'local-sql-'));await fsp.chmod(sqlDir,0o700);
const databasePath=path.join(sqlDir,'fixture.sqlite'),db=new D(databasePath);
db.exec('create table rudder_workspaces(id text primary key,account_id text,workspace_key text,status text,worker_token_hash text,created_at text,updated_at text,machine_id text,volume_id text,home_id text)');db.close();
const identity={...prepared.auth,databasePath};
const sql=(mode,rowid=null)=>{let answer;vm.runInNewContext(authSource(identity,mode,rowid),{require:name=>name==='/app/node_modules/better-sqlite3'?D:require(name),console:{log:line=>{answer=JSON.parse(line);}}});return answer;};
assert.equal(sql('inspect').status,'absent');const created=sql('create');assert.match(created.rowid,/^[1-9][0-9]*$/);assert.throws(()=>sql('create'));assert.throws(()=>sql('remove','999'));
const foreign=new D(databasePath);foreign.prepare('update rudder_workspaces set account_id=? where id=?').run('foreign',identity.workspace);foreign.close();assert.throws(()=>sql('remove',created.rowid));
const restored=new D(databasePath);assert.equal(restored.prepare('select account_id from rudder_workspaces').get().account_id,'foreign');restored.prepare('update rudder_workspaces set account_id=? where id=?').run(identity.account,identity.workspace);restored.close();
assert.equal(sql('remove',created.rowid).status,'absent');assert.equal(sql('absent').status,'absent');assert.equal(sql('remove',created.rowid).status,'absent');
cases.push('real disposable SQLite exact row/rowid/rollback/idempotent removal');
await fsp.rm(sqlDir,{recursive:true});

function passedProof(p=prepared) {
  const fields={Uid:'1500\t1500\t1500\t1500',Gid:'1500\t1500\t1500\t1500',Groups:'',NoNewPrivs:'1',CapEff:'0000000000000000',CapPrm:'0000000000000000',CapInh:'0000000000000000',CapAmb:'0000000000000000',CapBnd:'0000000000000000'};
  const parent={user:{device:'4',inode:'4026531837'},net:{device:'4',inode:'4026531999'},mnt:{device:'4',inode:'4026532000'}};
  return {formatVersion:1,kind:ROLE,nonce:p.nonce,version:value.version,revision:value.revision,completed:true,qualified:true,syntheticDataOnly:true,
    admittedAfterResolvedDigest:true,parentNamespacesAndStatusUnchanged:true,parentBefore:parent,parentAfter:clone(parent),boot:{uid:1500,gid:1500,fields,syntheticCredentialFields:1,otherCredentialFields:0},
    runtime:{workerVersion:value.version,moduleHashes:p.input.workerHashes,nativeSha256:'5'.repeat(64),nativeBytes:123456},
    child:{settled:true,exit:0,signal:null,failure:null},supervisorModuleEvaluated:false,activeProducerReceiptQualified:false,currentGrantQualified:false,
    sourceAcceptanceQualified:false,runnableExecutionQualified:false,modelCalls:false,providerAccountCalls:false,
    summary:{status:'passed',kind:'fresh-native-checkpoint-data-retention',publicationId:bundle.expected.publicationId,publication:bundle.expected.publication,
      closureObjects:13,closureBytes:12953,nativeHelpersSettled:2,nativeCredentialEnvironmentFields:0,exactAllObjectBytes:true,rawQBytes:true,rawRunnableBytes:true,
      highU64Preserved:true,retainedAcrossNativeRestart:true,syntheticDataOnly:true,activeProducerReceiptQualified:false,currentGrantQualified:false,
      sourceAcceptanceQualified:false,runnableExecutionQualified:false}};
}
validateGuest(passedProof(),value,prepared);
for(const mutate of [x=>x.boot.fields.CapBnd='0000000000000001',x=>x.parentAfter.net.inode='555',x=>x.summary.closureObjects=12,x=>x.summary.nativeHelpersSettled=1,x=>x.currentGrantQualified=true,x=>x.boot.otherCredentialFields=1]){
  const p=passedProof();mutate(p);assert.throws(()=>validateGuest(p,value,prepared));}
cases.push('strict DATA proof capability/status/closure/no-live-authority checks');

// Full controller mock transport follows actual machine/list/config and row
// lifecycle. No mocked success is counted as real qualification.
const scenarios=['pass','wrong-digest','wrong-image','app-secret','preexisting-name','uncertain-create-found','uncertain-create-unobserved',
  'bad-host-json','bad-guest-json','guest-failed','guest-capability','destroy-lost-reply','auth-delete-lost-reply','auth-create-lost-reply','control-replaced',
  'control-wrong-revision','worker-wrong-revision','second-control','secret-before-admission'];
const runs=[];
for(const scenario of scenarios){
  const nonce=crypto.randomUUID(),args=[...argv];args[args.indexOf('--nonce')+1]=nonce;
  const v=options(args),p=plan(v,inputs,'rdrw_'+'B'.repeat(43));let machine,row=false,gate=false,calls=0,destroys=0,controlReads=0,secretsReads=0;
  const oldHome={id:'8edd225a7d0378',name:'home',instance_id:'01M670MG4A8E0DWE9GA1C0PQ92',state:'started',config:{metadata:{}}};
  const reply=(data,ok=true)=>({ok,stdout:typeof data==='string'?data:JSON.stringify(data),diagnostics:{settled:true,status:ok?0:1}});
  const runCommand=async(args,o)=>{
    calls++;const label=path.basename(o.diagnosticPath).replace(/^\d+-/,'').replace(/-stderr.private$/,'');
    if(args[0]==='secrets'){secretsReads++;return reply(scenario==='app-secret'||scenario==='secret-before-admission'&&secretsReads>=3?[{Name:'FOREIGN'}]:[]);}
    if(args[0]==='machine'&&args[1]==='list'){
      if(args[args.indexOf('-a')+1]==='rudder-cloud-control'){controlReads++;const c=clone(control);if(scenario==='control-replaced'&&row)c.instance_id='other-instance';
        if(scenario==='control-wrong-revision')c.image_ref.labels['org.opencontainers.image.revision']='9'.repeat(40);
        return reply(scenario==='second-control'?[c,{id:'11111111111111',state:'stopped'}]:[c]);}
      const all=[oldHome];if(scenario==='preexisting-name')all.push({id:'11111111111111',name:p.name,config:{metadata:{qualification:nonce}}});if(machine)all.push(machine);return reply(all);
    }
    if(args[1]==='run'){
      const config=JSON.parse(await fsp.readFile(args[args.indexOf('--machine-config')+1]));machine={...machineFor({...p,config}),config:{...config,image:config.image+'@'+digest}};
      if(scenario==='wrong-digest'){machine.image_ref.digest='sha256:'+'9'.repeat(64);machine.config.image=config.image+'@'+machine.image_ref.digest;}
      if(scenario==='wrong-image')machine.config.image='registry.fly.io/rudder-workers-exla:foreign';
      if(scenario==='worker-wrong-revision')machine.image_ref.labels['org.opencontainers.image.revision']='9'.repeat(40);
      if(scenario==='uncertain-create-unobserved')machine=undefined;
      return reply('',!scenario.startsWith('uncertain-create'));
    }
    if(args[1]==='destroy'){assert(machine);assert.equal(args[2],machine.id);assert.notEqual(args[2],oldHome.id);destroys++;machine=undefined;return reply('',scenario!=='destroy-lost-reply');}
    if(args[1]!=='exec')throw Error('unexpected mock command');
    if(args[args.indexOf('-a')+1]==='rudder-cloud-control'){
      if(label==='verify-control-runtime-source')return reply({status:'passed',nonce,hashes:Object.fromEntries(Object.entries(hashes).filter(([k])=>k.startsWith('cloud/'))),durableStoreEnabled:true,databasePath:'/data/rudder-cloud.sqlite'});
      if(label==='assert-synthetic-auth-absent'){assert(!row);return reply({status:'absent',nonce});}
      if(label==='create-exact-synthetic-auth'){assert(!row);row=true;return reply({status:'created',nonce,rowid:'1'},scenario!=='auth-create-lost-reply');}
      if(label==='remove-exact-synthetic-auth'){row=false;return reply({status:'absent',nonce},scenario!=='auth-delete-lost-reply');}
      if(label==='verify-synthetic-auth-absence'||label==='independent-synthetic-auth-absence'){assert(!row);return reply({status:'absent',nonce});}
      throw Error('unexpected mock control exec');
    }
    assert(machine);assert.equal(args[2],machine.id);
    if(label==='verify-owned-stock-runtime')return reply({workerVersion:value.version,moduleHashes:p.input.workerHashes,nativeSha256:'5'.repeat(64),nativeBytes:123456});
    if(label==='publish-atomic-data-admission-gate'){assert.equal(machine.image_ref.digest,digest);gate=true;return reply({nonce,published:true});}
    if(label==='read-owned-data-proof'){
      if(scenario==='bad-guest-json')return reply('not-json');const proof=passedProof({...p,config:machine.config});
      if(scenario==='guest-failed'){proof.completed=false;proof.qualified=false;}
      if(scenario==='guest-capability')proof.boot.fields.CapBnd='0000000000000001';
      const raw=Buffer.from(JSON.stringify(proof,null,2)+'\n');return reply({status:'ready',sha256:hash(raw),proof});
    }
    throw Error('unexpected mock worker exec');
  };
  const runPublication=async(source,env,o)=>{assert.match(env.PCD_BEARER,/^rdrw_/);assert(!Object.hasOwn(env,'AWS_ACCESS_KEY_ID'));assert(source.includes('uploadPartialCheckpoint'));
    if(scenario==='bad-host-json')return reply('not-json');
    return reply({status:'passed',kind:'synthetic-checkpoint-data-publication',bundle:{root_id:bundle.expected.publicationId,root:bundle.expected.publication,object_count:13,total_bytes:12953},uploadedObjects:9,exactObjectReads:13,
      rawReceiptSha256:bundle.expected.receiptSha256,opaqueHighU64Receipt:true,sameIdGetReconciled:true,syntheticDataOnly:true,activeProducerReceiptQualified:false,currentGrantQualified:false});};
  const result=await main(args,{runCommand,readInputs:async()=>inputs,runPublication,pause:async()=>{},emit:()=>{}});
  assert.equal(result.testOnlyMock,true);assert.equal(result.qualified,false);assert.equal(result.providerCalls,0);
  const report=JSON.parse(await fsp.readFile(result.reportPath));assert.equal(report.testOnlyMock,true);
  const positive=['pass','uncertain-create-found','destroy-lost-reply','auth-delete-lost-reply'].includes(scenario);
  assert.equal(result.mockPassed,positive,scenario);assert.equal(report.completed,positive||scenario==='control-replaced',scenario);
  if(positive){assert(result.cleanupVerified&&result.independentAbsence&&result.syntheticCredentialRemoved&&result.independentCredentialAbsence);assert.equal(destroys,1);assert(gate);}
  if(scenario==='wrong-digest'){assert.equal(destroys,1);assert(result.cleanupVerified);assert(!gate);assert.equal(report.createdDescriptor.observedDigest,'sha256:'+'9'.repeat(64));}
  if(scenario==='worker-wrong-revision'||scenario==='secret-before-admission'){assert.equal(destroys,1);assert(result.cleanupVerified);assert(!gate);assert.equal(result.syntheticCredentialRemoved,true);}
  if(scenario==='wrong-image'){assert.equal(destroys,0);assert.equal(result.cleanupVerified,false);assert(!gate);}
  if(scenario==='app-secret'||scenario==='preexisting-name'){assert.equal(destroys,0);assert.equal(report.creationAttempted,false);assert.equal(report.authCreationAttempted,false);assert(!row);}
  if(scenario==='control-wrong-revision'||scenario==='second-control'){assert.equal(destroys,0);assert.equal(report.creationAttempted,false);assert.equal(report.authCreationAttempted,false);assert(!row);}
  if(scenario==='uncertain-create-unobserved'){assert.equal(result.cleanupVerified,false);assert.equal(destroys,0);assert(!gate);}
  if(scenario==='auth-create-lost-reply'){assert.equal(report.creationAttempted,false);assert.equal(result.syntheticCredentialRemoved,true);assert(!row);}
  if(scenario==='control-replaced'){assert.equal(result.syntheticCredentialRemoved,false);assert(row);}
  if(scenario==='bad-host-json')assert.equal(await fsp.readFile(path.join(path.dirname(result.reportPath),'host-publication.stdout.private'),'utf8'),'not-json');
  if(scenario==='bad-guest-json'){const file=report.steps.find(s=>s.step==='read-owned-data-proof').privateStdoutFile;assert.equal(await fsp.readFile(file,'utf8'),'not-json');}
  runs.push({scenario,calls,destroys,gate,result});
}
cases.push('19 complete lifecycle mock scenarios');

// Execute the actual serialized preload with mocked Node modules and a
// pre-published gate. This checks its literal escapes/import names, root-owned
// image verification, child credential exclusion and persistence before parse.
async function preloadTest(badChild=false) {
  const p=prepared.input,files=new Map(),fdMap=new Map();let nextFd=10,proofResolve;
  const proofSaved=new Promise(r=>{proofResolve=r;}),status=Object.entries(passedProof().boot.fields).map(([k,v])=>k+':\t'+v+'\n').join('');
  const st=(file,extra={})=>({uid:0,gid:0,mode:0o100644,nlink:1,dev:4,ino:100,size:files.get(file)?.length||0,isDirectory:()=>false,isFile:()=>true,isSymbolicLink:()=>false,...extra});
  for(const [relative,expected]of Object.entries(p.workerHashes))files.set('/opt/rudder/'+relative,Buffer.from('mock:'+relative));
  files.set('/opt/rudder/package.json',Buffer.from(JSON.stringify({version:value.version})));files.set('/opt/rudder/dist/native/rudder-native',Buffer.from('native'));
  files.set(p.gatePath,Buffer.from(JSON.stringify(p.gate)));
  const expectedHashes=Object.fromEntries(Object.keys(p.workerHashes).map(k=>[k,hash(files.get('/opt/rudder/'+k))]));
  const input={...p,workerHashes:expectedHashes};
  const statFor=file=>file===input.gatePath?st(file,{uid:1500,gid:1500,mode:0o100600}):file==='/workspace'?st(file,{uid:1500,gid:1500,isDirectory:()=>true,isFile:()=>false}):
    file.startsWith('/proc/self/ns/')?st(file,{dev:4n,ino:BigInt(file.endsWith('/user')?1001:file.endsWith('/net')?1002:1003)}):
    files.has(file)?st(file,{mode:file.endsWith('/rudder-native')?0o100755:0o100644}):st(file,{isDirectory:()=>true,isFile:()=>false});
  const fakeFs={constants:fs.constants,readFileSync:f=>{assert.equal(f,'/proc/self/status');return status;},lstatSync:statFor,statfsSync:()=>({type:0x6e736673}),
    fstatSync:fd=>statFor(fdMap.get(fd).file),openSync:(file,flags,mode)=>{if(flags&fs.constants.O_CREAT){assert(!files.has(file));files.set(file,Buffer.alloc(0));}
      const fd=nextFd++;fdMap.set(fd,{file,position:0});return fd;},closeSync:fd=>{fdMap.delete(fd);},readSync:(fd,b,o,n)=>{const h=fdMap.get(fd),raw=files.get(h.file);assert(raw);const q=Math.min(n,raw.length-h.position);raw.copy(b,o,h.position,h.position+q);h.position+=q;return q;},
    writeFileSync:(fd,body)=>{const file=fdMap.get(fd).file;files.set(file,Buffer.from(body));if(file===input.proofPath)proofResolve();},fsyncSync:()=>{},
    existsSync:file=>files.has(file),mkdirSync:file=>{files.set(file,Buffer.alloc(0));},realpathSync:f=>f};
  let spawnCalled=false;const summary=passedProof().summary;
  const fakeCp={spawn:(bin,args,opts)=>{spawnCalled=true;assert.equal(bin,'/usr/local/bin/node');assert(!Object.hasOwn(opts.env,'NODE_OPTIONS'));assert.deepEqual(Object.keys(opts.env).filter(k=>/TOKEN|KEY|CREDENTIAL|AUTH/.test(k)),[]);
    assert.equal(opts.env.PCD_BEARER,prepared.config.env.RUDDER_WORKER_TOKEN);assert.equal(opts.detached,true);assert(args[2].includes('nativeRestore'));
    const ch=new EventEmitter();ch.stdout=new EventEmitter();ch.stderr=new EventEmitter();ch.pid=1234;
    setImmediate(()=>{ch.stdout.emit('data',Buffer.from(badChild?'not-json':JSON.stringify(summary)));ch.emit('exit',0,null);ch.emit('close');});return ch;}};
  const proc={env:{...prepared.config.env,RUDDER_WORKER_FUSE_READY:'1'},getuid:()=>1500,getgid:()=>1500,version:process.version,kill:()=>{},exit:()=>{throw Error('unexpected mock fuse');}};
  const modules={'node:fs':fakeFs,'node:child_process':fakeCp,'node:crypto':crypto,'node:os':{release:()=>'mock-kernel'},'node:assert/strict':{default:assert}};
  const context=vm.createContext({process:proc,Buffer,console,setTimeout:()=>({}),clearTimeout:()=>{}});
  const cache=new Map(),moduleFor=async name=>{assert(Object.hasOwn(modules,name));if(cache.has(name))return cache.get(name);const object=modules[name],mod=new vm.SyntheticModule(Object.keys(object),function(){for(const k of Object.keys(object))this.setExport(k,object[k]);},{context});cache.set(name,mod);await mod.link(()=>{});await mod.evaluate();return mod;};
  const mod=new vm.SourceTextModule(guestSource(input),{context,importModuleDynamically:moduleFor});await mod.link(()=>{});let evaluationSettled=false;void mod.evaluate().then(()=>{evaluationSettled=true;});
  await Promise.race([proofSaved,new Promise((_,reject)=>setTimeout(()=>reject(Error('mock preload did not finish')),2000))]);
  assert.equal(evaluationSettled,false,'preload must hold Node import; ordinary supervisor must not evaluate');
  const proof=JSON.parse(files.get(input.proofPath));assert(spawnCalled);assert(files.has(input.home+'/native.stdout.private'));assert(files.has(input.home+'/native.stderr.private'));
  assert.equal(proof.completed,!badChild);assert.equal(proof.qualified,!badChild);if(!badChild)validateGuest(proof,value,{...prepared,input});else assert.equal(files.get(input.home+'/native.stdout.private').toString(),'not-json');
}
await preloadTest();await preloadTest(true);cases.push('actual serialized preload success/malformed child preservation');

// Execute both actual emitted driver functions against public-API adapters.
// These are code-generation/env/byte-ownership checks, not durability proofs.
async function driverTest(native=false) {
  const across=value=>Object.prototype.toString.call(value)==='[object Set]'?new Set(Array.from(value,across)):
    Buffer.isBuffer(value)?Buffer.from(value):Array.isArray(value)?Array.from(value,across):value&&typeof value==='object'?Object.fromEntries(Object.entries(value).map(([k,v])=>[k,across(v)])):value;
  // Adapters live outside vm; normalize only cross-realm prototypes, retaining
  // every value, byte and Set member. Production uses unmodified strict assert.
  const vmAssert=Object.assign((v,m)=>assert(v,m),assert,{deepEqual:(a,b)=>assert.deepEqual(across(a),across(b))});
  const all=[...bundle.fixture.objects,...bundle.fixture.base_objects],byRef=new Map(all.map(o=>[`${o.reference.kind}:${o.reference.algorithm}:${o.reference.digest}:${o.reference.size_bytes}`,Buffer.from(o.hex,'hex')]));
  const key=r=>`${r.kind}:${r.algorithm}:${r.digest}:${r.size_bytes}`,p=JSON.parse(bundle.fixture.envelope_json).payload;
  let result,reads=0,uploads=0,opens=0,closes=0,pinned=false;const retained={},issued=new Set();
  const receipt={format_version:1,root_id:bundle.expected.publicationId,root:bundle.expected.publication,durability:'replicated',published_at_ms:1,object_count:13,total_bytes:12953};
  class Remote {
    constructor(o){assert.equal(o.concurrency,1);assert.deepEqual(normalize(o.scope),bundle.expected.scope);assert.equal(o.authHeaders().Authorization,'Bearer '+'rdrw_'+'A'.repeat(43));}
    async beginStaging(){return {lease_id:'mock-stage'};}
    async putObject(id,ref,bytes){assert.equal(id,'mock-stage');assert.deepEqual(Buffer.from(bytes),byRef.get(key(ref)));}
    async publishRoot(id,stage,root){assert.equal(id,bundle.expected.base.root_id);assert.equal(stage,'mock-stage');return {...receipt,root,root_id:id};}
    async getRoot(id){assert.equal(id,bundle.expected.publicationId);return receipt;}
    async getObject(ref){return Buffer.from(byRef.get(key(ref)));}
    async uploadPartialCheckpoint(id,raw,source){uploads++;assert.equal(id,bundle.expected.publicationId);assert.equal(raw,bundle.fixture.receipt_json);
      const inventory=await source.pinPartialCheckpoint(raw);assert.equal(Object.keys(inventory).length,12);assert.equal(inventory.publication_id,id);
      assert(!issued.has(inventory.owned_pin));issued.add(inventory.owned_pin);
      assert.deepEqual(normalize(inventory.q),p.q);assert.equal(inventory.objects.length,9);
      if(uploads===1)for(const ref of inventory.objects){assert.deepEqual(await source.read(ref),byRef.get(key(ref)));reads++;}
      await source.unpin(inventory.owned_pin);return {kind:'partial_checkpoint_data',source_role:'quiesced_source_complement_data',bundle:receipt,uploaded_objects:uploads===1?9:0,uploaded_bytes:0};}
    async downloadRoot(id,local,pin){assert.equal(id,bundle.expected.publicationId);retained[pin]=bundle.expected.publication;return {receipt,downloaded_objects:13,downloaded_bytes:12953};}
  }
  const Local={async open(o){opens++;pinned=false;assert.equal(o.nativeBinary,'/opt/rudder/dist/native/rudder-native');assert(!Object.hasOwn(o.env,'PCD_BEARER'));assert(!Object.hasOwn(o.env,'NODE_OPTIONS'));
    assert(!Object.keys(o.env).some(k=>/TOKEN|SECRET|PASSWORD|KEY|CREDENTIAL|AUTH/i.test(k)));let closed=false;return {
      async request(request){assert(!closed);assert.equal(request.operation,'retention');return {pins:{...retained}};},
      async pin(ref){assert(!closed);assert.deepEqual(normalize(ref),bundle.expected.publication);pinned=true;},
      async closure(ref){assert(!closed&&pinned);return bundle.expected.refs;},async read(ref){assert(!closed);if(!pinned)throw Error('requires owned read pin');return Buffer.from(byRef.get(key(ref)));},
      async close(){assert(!closed);closed=true;closes++;}};}};
  const modules={'node:fs/promises':fsp,'node:crypto':crypto,'node:assert/strict':{default:vmAssert},'mock:fabric-remote':{FabricRemoteClient:Remote},
    '/opt/rudder/dist/fabric-remote.js':{FabricRemoteClient:Remote},'/opt/rudder/dist/fabric-local.js':{FabricLocalClient:Local}};
  const context=vm.createContext({process:{env:{PCD_BEARER:'rdrw_'+'A'.repeat(43)}},Buffer,console:{log:line=>{result=JSON.parse(line);}}});
  const cache=new Map(),get=async name=>{assert(Object.hasOwn(modules,name),name);if(cache.has(name))return cache.get(name);const object=modules[name];
    const mod=new vm.SyntheticModule(Object.keys(object),function(){for(const k of Object.keys(object))this.setExport(k,object[k]);},{context});cache.set(name,mod);await mod.link(()=>{});await mod.evaluate();return mod;};
  const input=native?{...prepared.input}:{moduleUrl:'mock:fabric-remote',fixturePath:path.join(HERE,'fixture-v1.json'),fixtureSha256:hash(fixtureBytes),baseUrl:'https://never-resolved.invalid/api/rudder/fabric',workspace:prepared.auth.workspace};
  const mod=new vm.SourceTextModule('await ('+(native?nativeRestore:hostPublication).toString()+')('+JSON.stringify(input)+');',{context,importModuleDynamically:get});await mod.link(()=>{});await mod.evaluate();
  assert.equal(result.status,'passed');assert.equal(result.syntheticDataOnly,true);assert.equal(result.currentGrantQualified,false);
  if(native){assert.equal(opens,2);assert.equal(closes,2);assert.equal(result.highU64Preserved,true);}else{assert.equal(reads,9);assert.equal(uploads,2);assert.equal(result.opaqueHighU64Receipt,true);}
}
await driverTest();await driverTest(true);cases.push('actual emitted host/native drivers, raw receipt, 12-field inventory, clean native env and restart');

// Credential-free local Node children verify close/timeout/output settlement.
// No Fly executable, native binary, network call or model is launched here.
const nodeDir=await fsp.mkdtemp(path.join(HERE,'local-node-'));await fsp.chmod(nodeDir,0o700);
for(const [name,source,timeout,pass]of [['success','console.log(JSON.stringify({status:"passed"}));',3000,true],
  ['deadline','setInterval(()=>{},1000);',100,false],['output-bound','process.stdout.write("x".repeat(70000));setInterval(()=>{},1000);',3000,false]]){
  const stdoutPath=path.join(nodeDir,name+'.stdout'),stderrPath=path.join(nodeDir,name+'.stderr');
  const answer=await runNode(source,{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:nodeDir,TMPDIR:nodeDir}, {timeout,stdoutPath,stderrPath});
  assert.equal(answer.ok,pass);assert.equal(answer.diagnostics.settled,true);assert.equal((await fsp.stat(stdoutPath)).mode&0o777,0o600);assert.equal((await fsp.stat(stderrPath)).mode&0o777,0o600);
  if(!pass)assert(['HOST_DATA_DEADLINE','HOST_STDOUT_BOUND'].includes(answer.diagnostics.failure));assert((await fsp.stat(stdoutPath)).size<=65536);
}
cases.push('real local Node close/deadline/pipe bounds with persisted private output');

const report={formatVersion:1,status:'passed',kind:'partial-cloud-controller-preparation-selftests',providerCalls:0,dnsCalls:0,models:0,
  providerQualified:false,testOnlyMock:true,cases,casesCount:cases.length,lifecycleScenarios:runs,fixtureSha256:hash(fixtureBytes),
  sourceHashes:Object.fromEntries(await Promise.all(['qualify-partial-cloud.mjs','helpers.mjs','selftest.mjs','fixture-v1.json'].map(async file=>[file,hash(await fsp.readFile(path.join(HERE,file)))]))),
  limits:['Local SQLite and mocked Fly/Cloud are not S3/Native/Fly runtime qualification.','Own /proc reads do not change namespaces or qualify the stock image.'],verifiedAt:new Date().toISOString()};
const output=path.join(HERE,'selftest-'+crypto.randomUUID()+'.json');await fsp.writeFile(output,JSON.stringify(report,null,2)+'\n',{mode:0o600,flag:'wx'});
console.log(JSON.stringify({status:'passed',cases:cases.length,lifecycleScenarios:runs.length,providerCalls:0,dnsCalls:0,providerQualified:false,report:output}));
