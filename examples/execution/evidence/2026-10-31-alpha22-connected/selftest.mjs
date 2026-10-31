// PRIVATE pure mock tests. No provider, DNS, Cargo, models,
// account/profile/cache reads or namespace transitions. Never pass --run here.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import vm from 'node:vm';
import crypto from 'node:crypto';
import {EventEmitter} from 'node:events';
import {Resolver} from 'node:dns/promises';
import {
  options,provenanceFor,plan,main,canonicalHost,sealDnsAnswers,parseProcStatus,heldNamespaceWitness,privateErrorDetail,
  readProtectedRuntime,validateConnectedProof,preloadSource,ownershipFor,rootProbeSource,gateSource,parseOwnedExecJson,
  preserveOwnedExecOutput,fixtureDiagnosticsSource,saveFixtureDiagnostics,attemptFailureDiagnostics,
} from './qualify-connected-network.mjs';
const hash=bytes=>crypto.createHash('sha256').update(bytes).digest('hex');
const normalize=value=>JSON.parse(JSON.stringify(value)),clone=value=>structuredClone(value);
const parentDigest='sha256:53c763d9e5886cadc262fa03631e5d08338c44486e2f3ffbb8e0afba4c21d89f';
const binaryBytes=Buffer.from('mock connected qualifier, never executed');
const argv=['--run','--image','registry.fly.io/rudder-workers-exla:alpha22-connect-canary-private-mock',
  '--digest','sha256:'+'2'.repeat(64),'--parent-digest',parentDigest,'--qualifier-sha256',hash(binaryBytes),
  '--source-envelope-sha256','3'.repeat(64),'--source-tree-sha256','4'.repeat(64),
  '--compiler-receipt-sha256','5'.repeat(64),'--native-sha256','6'.repeat(64),
  '--artifact-binding','/private/mock-run-binding.json','--artifact-binding-sha256','7'.repeat(64),
  '--revision','1'.repeat(40),'--source-kind','frozen-working-tree',
  '--nonce','00000000-0000-0000-0000-000000000001'];
const value=options(argv),prepared=plan(value),input=prepared.input;
// Inspect the real supported API without making any lookup.
assert.equal(typeof Resolver,'function');assert.equal(typeof Resolver.prototype.resolve4,'function');
assert.deepEqual(options([]),{run:false});assert.deepEqual(options(['--describe']),{run:false});
assert.equal((await main([])).providerCalls,0);assert.equal((await main(['--describe'])).status,'prepared-no-run');
assert(Buffer.byteLength(prepared.config.env.NODE_OPTIONS)<65536);
for(const key of ['image','digest','parent-digest','qualifier-sha256','source-envelope-sha256','source-tree-sha256','compiler-receipt-sha256',
  'native-sha256','artifact-binding','artifact-binding-sha256','revision','source-kind']){
  const changed=[...argv],at=changed.indexOf('--'+key);changed.splice(at,2);assert.throws(()=>options(changed),key);
}
for(const [key,bad]of [['parent-digest','sha256:'+'a'.repeat(64)],['digest',parentDigest],
  ['digest','sha256:404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245'],
  ['image','registry.fly.io/rudder-workers-exla:4.0.0-alpha.21'],['image','registry.fly.io/rudder-workers-exla:4.0.0-alpha.22'],
  ['revision','short'],['source-kind','automatic'],['host','127.0.0.1'],['host','example.internal'],['host','EXAMPLE.com'],
  ['nonce','00000000-0000-0000-0000-000000000000']]){
  const changed=[...argv],at=changed.indexOf('--'+key);if(at<0)changed.push('--'+key,bad);else changed[at+1]=bad;assert.throws(()=>options(changed),key);
}
const provenance=provenanceFor(value);assert.equal(provenance.workerVersion,'4.0.0-alpha.21');assert.equal(provenance.proposedVersion,'4.0.0-alpha.22');
assert.equal(provenance.sourceBaseRevision,value.revision);assert.equal(provenance.baseRevisionIsCallerDeclared,true);
assert.equal(provenance.nativeSha256,value.nativeSha256);assert.equal(provenance.compilerReceiptSha256,value.compilerReceiptSha256);
assert.equal(provenance.nativeBinaryExecuted,false);assert(!Object.hasOwn(provenance,'shippedRevision'));
assert(!Object.hasOwn(provenance,'qualifierSourceRevision'));assert.equal(provenance.parentDigest,parentDigest);assert(canonicalHost('rudder.exla.ai'));

// Execute serialized helper fixtures with fake fs only; no actual /proc reads.
const parser=new vm.Script('('+JSON.parse(JSON.stringify(parseProcStatus.toString()))+')').runInNewContext();
const ownProcNamespaces=0;
const tab=String.fromCharCode(9),lf=String.fromCharCode(10),cr=String.fromCharCode(13);
const zeroFields={Uid:[1500,1500,1500,1500].join(tab),Gid:[1500,1500,1500,1500].join(tab),Groups:'',NoNewPrivs:'1',
  CapEff:'0000000000000000',CapPrm:'0000000000000000',CapInh:'0000000000000000',CapAmb:'0000000000000000',CapBnd:'0000000000000000'};
for(const newline of [lf,cr+lf])for(const groups of ['', '1500 1501']){const fields={...zeroFields,Groups:groups,CapAmb:'0000000000000008'};
  const text=Object.entries(fields).map(([key,value])=>key+':'+tab+value).join(newline)+newline;
  assert.deepEqual(normalize(parser(text)),fields);}
const publicPins=['1.1.1.1','8.8.8.8'];assert.deepEqual(sealDnsAnswers('rudder.exla.ai',[...publicPins].reverse(),assert).ipv4,publicPins);
let dnsRejections=0;
for(const answers of [[],['127.0.0.1'],['10.0.0.1','8.8.8.8'],['169.254.169.254'],['100.64.0.1'],['192.0.2.1'],['198.18.0.1'],['224.0.0.1'],
  ['1.1.1.1','1.1.1.1'],['::1'],['::ffff:8.8.8.8'],['01.1.1.1'],['1.1.1.256'],['1.1.1.1','8.8.8.8','9.9.9.9','11.1.1.1','12.1.1.1']]){
  assert.throws(()=>sealDnsAnswers('rudder.exla.ai',answers,assert));dnsRejections++;}
const longError='TypeError: meaningful failure\n at witness (data:text/javascript;base64,'+'A'.repeat(90000)+':12:3)\n at private-file.mjs:8:2';
const compact=privateErrorDetail({stack:longError});assert(compact.formatted.length<8192&&compact.shortened);assert.equal(compact.raw.length,Buffer.byteLength(longError));
assert(compact.formatted.toString().includes('meaningful failure')&&compact.formatted.toString().includes('private-file'));

const parent={user:{device:'1',inode:'100'},net:{device:'1',inode:'200'},mnt:{device:'1',inode:'300'}};
const summary={kind:'connected-network-qualification-v1',status:'passed',transportVersion:1,networkProfileVersion:1,profileSha256:'4'.repeat(64),listenerPort:12345,
  physical:{uid:1500,separateUser:true,separateNetwork:true,socketNamespaceIoctl:true,listenerRemovedBeforeExec:true,allFiveCapabilitiesZero:true,
    noNewPrivileges:true,loOnly:true,actualGuestReaped:true,resolverChildrenReaped:true,proxyWorkersJoined:true},guestUser:{device:'1',inode:'101'},guestNetwork:{device:'1',inode:'201'},
  client:{kind:'public-tls-head-v1',certificateVerified:true,tlsVersion:'TLSv1.3',httpStatus:200,responseHeaderBytes:512},
  counters:{accepted:1,completed:1,rejected:0,active_connections:0,maximum_active_connections:1,dns_jobs:1,active_dns_jobs:0,maximum_active_dns_jobs:1,
    upstream_connections:1,charged_bytes:4096,cancellation_requested:true},
  limits:{lifetimeConnections:32,dnsProcesses:1,connectionWorkers:2,combinedBufferBytesPerConnection:32768,totalBytes:2097152,connectionTotalMs:20000},
  limitations:['standalone synthetic grant','no managed activation','no account/model calls','local inventory only','opaque tunnel contents']};
const counts={qualified:1,unsupported:0,failed:0,publicHttpsConnectCasesPassed:1};assert.deepEqual(validateConnectedProof(summary,input,parent,assert),counts);
let proofRejections=0;
for(const mutate of [x=>x.status='unsupported',x=>x.transportVersion=2,x=>x.physical.uid=0,x=>x.physical.loOnly=false,x=>x.physical.socketNamespaceIoctl=false,
  x=>x.physical.actualGuestReaped=false,x=>x.physical.resolverChildrenReaped=false,x=>x.physical.proxyWorkersJoined=false,x=>x.physical.allFiveCapabilitiesZero=false,
  x=>x.physical.listenerRemovedBeforeExec=false,x=>x.client.certificateVerified=false,x=>x.client.httpStatus=401,x=>x.client.responseHeaderBytes=16385,
  x=>x.guestNetwork=clone(parent.net),x=>x.guestUser=clone(parent.user),x=>x.counters.active_dns_jobs=1,x=>x.counters.active_connections=1,
  x=>x.counters.maximum_active_dns_jobs=2,x=>x.counters.upstream_connections=0,x=>x.counters.dns_jobs=0,x=>x.counters.cancellation_requested=false,
  x=>x.counters.charged_bytes=2097153,x=>x.limits.combinedBufferBytesPerConnection=65536]){const changed=clone(summary);mutate(changed);assert.throws(()=>validateConnectedProof(changed,input,parent,assert));proofRejections++;}

function fakeRuntime(scenario='positive',customInput=input){
  let serial=10,dnsCalls=0,spawns=0,proofResolve;
  const proof=new Promise(resolve=>proofResolve=resolve),files=new Map(),descriptors=new Map(),timers=new Map(),writes=new Map();
  const fixtureName='connect-00000000-0000-0000-0000-000000000002';
  let ino=1000;
  const add=(name,type,bytes=Buffer.alloc(0),uid=0,gid=0,mode=type==='directory'?0o755:0o644)=>{
    const value={name,type,bytes:Buffer.from(bytes),uid,gid,mode,ino:++ino,nlink:1};files.set(name,value);return value;};
  for(const name of ['/usr','/usr/local','/usr/local/bin','/usr/local/share','/opt','/opt/rudder'])add(name,'directory');
  add('/workspace','directory',[],1500,1500);add(customInput.binary,'file',binaryBytes,0,0,0o755);
  add(customInput.provenancePath,'file',JSON.stringify(customInput.provenance),0,0,0o644);add('/opt/rudder/package.json','file',JSON.stringify({version:'4.0.0-alpha.21'}),0,0,0o664);
  add(customInput.gatePath,'file',JSON.stringify(customInput.gate),1500,1500,0o600);
  const status=Object.entries(zeroFields).map(([key,value])=>key+':'+tab+value).join(lf)+lf;add('/proc/self/status','file',status,1500,1500,0o444);
  for(const [kind,value]of Object.entries(parent))add('/proc/self/ns/'+kind,'namespace',[],1500,1500,0o444).ino=Number(value.inode);
  if(scenario==='wrong-gate')files.get(customInput.gatePath).bytes=Buffer.from('{}');
  if(scenario==='credential-env'){} // set in mocked process below
  if(scenario==='binary-hash')files.get(customInput.binary).bytes=Buffer.from('wrong bytes');
  if(scenario==='binary-fifo')files.get(customInput.binary).type='fifo';
  if(scenario==='binary-mode')files.get(customInput.binary).mode=0o775;
  if(scenario==='parent-writable')files.get('/usr/local/bin').mode=0o775;
  if(scenario==='provenance-mismatch')files.get(customInput.provenancePath).bytes=Buffer.from('{}');
  const error=code=>Object.assign(Error('mock private failure'),{code});
  const resolve=name=>{const match=/^\/proc\/self\/fd\/(\d+)(?:\/(.*))?$/.exec(name);if(!match)return name;
    const item=descriptors.get(Number(match[1]));if(!item)throw error('EBADF');return match[2]?item.entry.name+'/'+match[2]:item.entry.name;};
  const metadata=(entry,bigint=false)=>({uid:entry.uid,gid:entry.gid,mode:(entry.type==='directory'?0o040000:entry.type==='fifo'?0o010000:0o100000)|entry.mode,
    nlink:entry.nlink,size:entry.bytes.length,dev:bigint?1n:1,ino:bigint?BigInt(entry.ino):entry.ino,
    isDirectory:()=>entry.type==='directory',isFile:()=>entry.type==='file',isSymbolicLink:()=>entry.type==='symlink'});
  const fake={constants:fs.constants,
    openSync(name,flags,mode){name=resolve(name);let entry=files.get(name);
      if(flags&fs.constants.O_CREAT){if(entry&&flags&fs.constants.O_EXCL)throw error('EEXIST');entry=add(name,'file',[],1500,1500,mode);}
      if(!entry)throw error('ENOENT');if(entry.type==='symlink'&&flags&fs.constants.O_NOFOLLOW)throw error('ELOOP');
      if(entry.type==='fifo')assert(flags&fs.constants.O_NONBLOCK);if(flags&fs.constants.O_DIRECTORY)assert.equal(entry.type,'directory');
      const fd=serial++;descriptors.set(fd,{entry,offset:0});return fd;},
    fstatSync(fd,options){return metadata(descriptors.get(fd).entry,options?.bigint);},
    lstatSync(name){const entry=files.get(resolve(name));if(!entry)throw error('ENOENT');return metadata(entry);},
    statSync(name,options){const entry=files.get(resolve(name));if(!entry)throw error('ENOENT');return metadata(entry,options?.bigint);},
    statfsSync(name){assert.equal(files.get(resolve(name)).type,'namespace');return {type:0x6e736673};},
    closeSync(fd){assert(descriptors.delete(fd));},
    readSync(fd,buffer,offset,length){const state=descriptors.get(fd);assert(!['fifo','symlink','directory'].includes(state.entry.type));
      const count=Math.min(length,state.entry.bytes.length-state.offset);state.entry.bytes.copy(buffer,offset,state.offset,state.offset+count);state.offset+=count;return count;},
    readFileSync(name,encoding){const bytes=files.get(resolve(name)).bytes;return encoding?bytes.toString(encoding):bytes;},
    writeFileSync(name,bytes){const entry=typeof name==='number'?descriptors.get(name).entry:files.get(resolve(name));entry.bytes=Buffer.from(bytes);writes.set(entry.name,Buffer.from(bytes));
      if(entry.name===customInput.proofPath)proofResolve(JSON.parse(bytes));},
    fsyncSync(){},existsSync:name=>files.has(resolve(name)),realpathSync:name=>name,
    mkdirSync(name,{mode}){assert(!files.has(name));add(name,'directory',[],1500,1500,mode);},
    fchownSync(fd,uid,gid){Object.assign(descriptors.get(fd).entry,{uid,gid});},
    linkSync(from,to){if(files.has(to))throw error('EEXIST');const entry=files.get(resolve(from));entry.nlink++;files.set(to,entry);},
    unlinkSync(name){const entry=files.get(name);assert(entry);entry.nlink--;files.delete(name);},
    opendirSync(name,{bufferSize}){assert.equal(bufferSize,2);const target=resolve(name),names=[...files.keys()].filter(key=>key.startsWith(target+'/')&&!key.slice(target.length+1).includes('/')).map(key=>({name:key.slice(target.length+1)}));
      return {readSync:()=>names.shift()??null,closeSync(){}};},
  };
  const childSummary=clone(summary);if(scenario==='bad-cert')childSummary.client.certificateVerified=false;
  const cp={spawn(binary,args,options){spawns++;assert.equal(binary,customInput.binary);assert.deepEqual(normalize(args),['qualify','--base',customInput.fixtureBase,'--host',customInput.host,'--ipv4',publicPins.join(',')]);
    assert.equal(options.env.HOME,customInput.home);assert(!Object.hasOwn(options.env,'NODE_OPTIONS'));assert(!Object.keys(options.env).some(key=>/TOKEN|SECRET|KEY/i.test(key)));
    const child=new EventEmitter();child.stdout=new EventEmitter();child.stderr=new EventEmitter();child.stdout.destroy=child.stderr.destroy=()=>{};child.unref=()=>{};
    child.kill=()=>{queueMicrotask(()=>{child.emit('exit',null,'SIGKILL');child.emit('close');});return true;};
    queueMicrotask(()=>{
      if(scenario==='child-timeout')return;
      if(scenario==='parent-net-change')files.get('/proc/self/ns/net').ino=900;
      if(scenario==='child-nonzero'||scenario==='dns-changed'){
        child.stderr.emit('data',Buffer.from('synthetic private child failure'));
        add(customInput.fixtureBase+'/'+fixtureName,'directory',[],1500,1500,0o700);add(customInput.fixtureBase+'/'+fixtureName+'/failure.private.txt','file','private synthetic helper failure',1500,1500,0o600);
        child.stdout.emit('data',Buffer.from('{"status":"failed","error":"DestinationDenied"}\n'));child.emit('exit',1,null);
      }else{child.stdout.emit('data',Buffer.from(scenario==='invalid-child-json'?'not-json\n':JSON.stringify(childSummary)+'\n'));child.emit('exit',0,null);}
      child.emit('close');});return child;}};
  const modules={'node:fs':fake,'node:child_process':cp,'node:crypto':crypto,'node:assert/strict':{default:assert},'node:os':{release:()=> 'mock-kernel'},
    'node:dns/promises':{Resolver:class{constructor(options){assert.equal(options.timeout,2000);assert.equal(options.tries,1);}async resolve4(host){dnsCalls++;assert.equal(host,customInput.host+'.');
      if(scenario==='dns-failure')throw error('ENOTFOUND');if(scenario==='mixed-dns')return ['8.8.8.8','10.0.0.1'];if(scenario==='too-many-dns')return ['1.1.1.1','8.8.8.8','9.9.9.9','11.1.1.1','12.1.1.1'];return [...publicPins].reverse();}}}};
  const process={env:{...prepared.config.env,RUDDER_WORKER_FUSE_READY:'1',...(scenario==='credential-env'?{ANTHROPIC_API_KEY:'mock-only-must-not-echo'}:{})},version:'v22.23.4',getuid:()=>1500,getgid:()=>1500,exit(){throw Error('independent fuse must not run during selftest');}};
  return {fake,files,descriptors,writes,proof,modules,process,counters:()=>({dnsCalls,spawns}),
    setTimeout(callback,ms){const id=serial++;timers.set(id,{callback,ms});if(ms===55000&&scenario==='child-timeout')queueMicrotask(callback);else if(ms<1000)queueMicrotask(callback);return id;},clearTimeout(id){timers.delete(id);}};
}

async function exercisePreload(scenario){
  const runtime=fakeRuntime(scenario),context=vm.createContext({Buffer,process:runtime.process,setTimeout:runtime.setTimeout,clearTimeout:runtime.clearTimeout,console:{log(){throw Error('no preload stdout');}}});
  const modules=new Map();
  const moduleFor=async name=>{if(modules.has(name))return modules.get(name);assert(Object.hasOwn(runtime.modules,name));const values=runtime.modules[name];
    const module=new vm.SyntheticModule(Object.keys(values),function(){for(const [key,value]of Object.entries(values))this.setExport(key,value);},{context});modules.set(name,module);await module.link(()=>{throw Error('no nested imports');});await module.evaluate();return module;};
  const module=new vm.SourceTextModule(preloadSource(input),{context,importModuleDynamically:moduleFor});await module.link(()=>{throw Error('no static imports');});
  let evaluationSettled=false;const evaluation=module.evaluate();evaluation.then(()=>{evaluationSettled=true;},()=>{evaluationSettled=true;});
  let timer;const result=await Promise.race([runtime.proof,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('mock preload failed to settle: '+scenario)),3000);})]);clearTimeout(timer);
  await Promise.resolve();assert.equal(evaluationSettled,false);assert.equal(runtime.descriptors.size,0);assert.equal(result.supervisorModuleEvaluated,false);
  const positive=scenario==='positive';assert.equal(result.qualified,positive,scenario);assert.equal(result.completed,positive,scenario);
  if(positive){assert.deepEqual(normalize(result.counts),counts);assert.equal(result.sealedTarget.lookupCalls,1);assert.deepEqual(normalize(result.sealedTarget.ipv4),publicPins);assert.equal(result.parentNamespacesAndStatusUnchanged,true);}
  else{assert(result.failedStage);assert(!JSON.stringify(result).includes('mock-only-must-not-echo'));assert(runtime.writes.has(input.failurePath));}
  if(['wrong-gate','credential-env','binary-hash','binary-fifo','binary-mode','parent-writable','provenance-mismatch'].includes(scenario))assert.deepEqual(runtime.counters(),{dnsCalls:0,spawns:0});
  if(['mixed-dns','too-many-dns','dns-failure'].includes(scenario))assert.deepEqual(runtime.counters(),{dnsCalls:1,spawns:0});
  return {result,runtime};
}
const preloadScenarios=['positive','wrong-gate','credential-env','binary-hash','binary-fifo','binary-mode','parent-writable','provenance-mismatch',
  'dns-failure','mixed-dns','too-many-dns','parent-net-change','bad-cert','invalid-child-json','child-nonzero','dns-changed','child-timeout'];
const preloadResults=new Map();for(const scenario of preloadScenarios)preloadResults.set(scenario,await exercisePreload(scenario));

// Exercise the exact serialized root probe. The fake runtime is only file data;
// no child command or resolver is invoked by these scripts.
let rootProbeCases=0;
for(const scenario of ['positive','binary-hash','binary-fifo','binary-mode','parent-writable','provenance-mismatch']){
  const runtime=fakeRuntime(scenario);let output,stderr=Buffer.alloc(0);const process={getuid:()=>0,stderr:{write:bytes=>stderr=Buffer.concat([stderr,bytes])}};
  new vm.Script(rootProbeSource(input)).runInNewContext({Buffer,process,require:name=>{
    if(name==='node:fs')return runtime.fake;if(name==='node:crypto')return crypto;if(name==='node:assert/strict')return assert;throw Error('unexpected import');
  },console:{log:bytes=>output=JSON.parse(bytes)}});
  assert.equal(output.status,scenario==='positive'?'passed':'failed');assert(stderr.length<=8192);assert.equal(runtime.descriptors.size,0);rootProbeCases++;
}

// Atomic gate must not overwrite an existing file, including a foreign gate.
{
  const runtime=fakeRuntime();runtime.files.delete(input.gatePath);let output;
  const context={Buffer,require:name=>name==='node:fs'?runtime.fake:crypto,console:{log:bytes=>output=JSON.parse(bytes)}};
  new vm.Script(gateSource(input)).runInNewContext(context);assert.equal(output.published,true);
  assert.deepEqual(JSON.parse(runtime.fake.readFileSync(input.gatePath)),input.gate);
  const original=Buffer.from(runtime.fake.readFileSync(input.gatePath));
  assert.throws(()=>new vm.Script(gateSource(input)).runInNewContext(context));assert.deepEqual(runtime.fake.readFileSync(input.gatePath),original);
  assert.equal(runtime.descriptors.size,0);
}

const machine={id:'1234567890abcd',name:prepared.name,state:'started',region:'sjc',instance_id:'01MOCKCONNECTEDINSTANCE',
  config:clone(prepared.config),image_ref:{digest:value.digest}};
let ownershipRejections=0;
for(const mutate of [x=>x.id='8edd225a7d0378',x=>x.instance_id='01M670MG4A8E0DWE9GA1C0PQ92',x=>x.id='not-an-id',x=>x.name+='foreign',
  x=>x.image_ref.digest='sha256:404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245',
  x=>x.config.metadata.qualification='foreign',x=>x.config.metadata.rudder_role='home',x=>x.config.metadata.qualification_revision='f'.repeat(40),
  x=>x.config.metadata.qualification_source_kind='other',x=>x.config.metadata.qualification_source_envelope='0'.repeat(64),
  x=>x.config.image='registry.fly.io/other:image',x=>x.config.services=[{}],x=>x.config.mounts=[{}],x=>x.config.files=[{}],
  x=>x.config.env.UNAPPROVED='mock',x=>x.config.env.RUDDER_WORKER_MANAGED_EXECUTION='0',x=>x.config.guest.memory_mb=4096]){
  const changed=clone(machine);mutate(changed);assert.equal(ownershipFor(prepared,value).matches(changed),false);ownershipRejections++;
}
{
  const owned=ownershipFor(prepared,value),normalized=clone(machine);normalized.config.image=value.image+'@'+value.digest;
  assert(owned.matches(normalized));owned.remember(normalized);assert.equal(owned.id(),machine.id);
  const changed=clone(normalized);changed.image_ref.digest='sha256:'+'9'.repeat(64);assert(!owned.matches(changed));
  const wrong=clone(machine);wrong.image_ref.digest='sha256:'+'8'.repeat(64);wrong.config.image=value.image+'@'+wrong.image_ref.digest;
  const cleanupOwner=ownershipFor(prepared,value);assert(cleanupOwner.matches(wrong));cleanupOwner.remember(wrong);assert.equal(cleanupOwner.descriptor().observedDigest,wrong.image_ref.digest);
}
{
  const step={};assert.throws(()=>parseOwnedExecJson('Exit code: 1\n',step));assert.equal(step.failure,'OWNED_GUEST_EXIT_NONZERO');assert.equal(step.guestExitCode,1);
  const invalid={};assert.throws(()=>parseOwnedExecJson('not-json',invalid));assert.equal(invalid.failure,'OWNED_EXEC_INVALID_JSON');
  const writes=new Map();await preserveOwnedExecOutput(Buffer.from('Exit code: 1\n'),Buffer.from('private mock stderr'),'/private/stdout','/private/stderr',async(name,bytes,options)=>{
    assert.equal(options.mode,0o600);assert.equal(options.flag,'wx');writes.set(name,Buffer.from(bytes));});
  assert.equal(writes.size,2);assert.equal(writes.get('/private/stderr').toString(),'private mock stderr');
}

function diagnosticsFrom(runtime,mutate=()=>{}){
  mutate(runtime);let output;new vm.Script(fixtureDiagnosticsSource(input)).runInNewContext({Buffer,
    require:name=>name==='node:fs'?runtime.fake:name==='node:crypto'?crypto:assert,console:{log:bytes=>output=JSON.parse(bytes)}});
  assert.equal(runtime.descriptors.size,0);assert(Buffer.byteLength(JSON.stringify(output))<=65536);return output;
}
const diagnosticPositive=diagnosticsFrom(preloadResults.get('child-nonzero').runtime);
assert.equal(diagnosticPositive.files.length,3);assert(diagnosticPositive.files.some(file=>file.label==='helper-failure'&&file.status==='read'));
const saved=new Map();await saveFixtureDiagnostics(diagnosticPositive,input,'/private/test',async(name,bytes,options)=>{
  assert.equal(options.mode,0o600);assert.equal(options.flag,'wx');saved.set(name,Buffer.from(bytes));});assert.equal(saved.size,4);
let diagnosticCases=0;
for(const scenario of ['fifo','symlink','foreign-uid','foreign-gid','world-write','hardlink','oversize','extra-fixture','base-symlink']){
  const {runtime}=await exercisePreload('child-nonzero');
  const bundle=diagnosticsFrom(runtime,({files})=>{
    const base=input.fixtureBase,helper=[...files.keys()].find(name=>name.startsWith(base+'/')&&name.endsWith('/failure.private.txt'));
    const entry=files.get(helper);
    if(scenario==='fifo')entry.type='fifo';if(scenario==='symlink')entry.type='symlink';if(scenario==='foreign-uid')entry.uid=0;
    if(scenario==='foreign-gid')entry.gid=0;if(scenario==='world-write')entry.mode=0o606;if(scenario==='hardlink')entry.nlink=2;
    if(scenario==='oversize')entry.bytes=Buffer.alloc(8193);if(scenario==='extra-fixture')files.set(base+'/unexpected',{...entry,name:base+'/unexpected',type:'directory'});
    if(scenario==='base-symlink')files.get(base).type='symlink';
  });
  assert(!bundle.files.some(file=>file.label==='helper-failure'&&file.status==='read'),scenario);diagnosticCases++;
}
for(const mutate of [x=>x.nonce='foreign',x=>x.fixtureBase='/home/real',x=>x.files[0].source='/home/real/auth.json',
  x=>x.files[0].label='provider-auth',x=>x.files[0].sha256='0'.repeat(64),x=>x.files[0].metadata.uid=0,
  x=>x.files[0].bodyBase64+='====',x=>x.files.push(clone(x.files[0]))]){
  const changed=clone(diagnosticPositive);mutate(changed);let writes=0;
  await assert.rejects(()=>saveFixtureDiagnostics(changed,input,'/private/test',async()=>{writes++;}));assert.equal(writes,0);
}
let cleanupRequired=0;for(const failure of ['collect','validate','save']){
  try{const outcome=await attemptFailureDiagnostics(async()=>{if(failure==='collect')throw Error('private synthetic collection failure');return failure==='validate'?{}:diagnosticPositive;},
    bundle=>saveFixtureDiagnostics(bundle,input,'/private/test',async()=>{throw Error('private disk failure');}));assert.equal(outcome.completed,false);}
  finally{cleanupRequired++;}}

async function exerciseMain(scenario){
  const nonce=crypto.randomUUID(),args=[...argv],at=args.indexOf('--nonce');args[at+1]=nonce;
  const ownValue=options(args),ownPrepared=plan(ownValue);let created=false,destroyed=false,destroyCount=0,execCount=0,admitted=false,runnerCalls=0;
  const live={id:'8edd225a7d0378',name:'protected-home',instance_id:'01M670MG4A8E0DWE9GA1C0PQ92',config:{metadata:{qualification:'foreign'}}};
  const own={id:'1234567890abcd',name:ownPrepared.name,state:'started',region:'sjc',instance_id:'01MOCKOWNEDINSTANCE',
    config:clone(ownPrepared.config),image_ref:{digest:ownValue.digest}};
  if(scenario==='wrong-digest'){own.image_ref.digest='sha256:'+'8'.repeat(64);own.config.image=ownValue.image+'@'+own.image_ref.digest;}
  const fullProof={formatVersion:1,kind:ownPrepared.role,nonce,provenance:ownPrepared.provenance,admittedAfterResolvedDigest:true,
    supervisorModuleEvaluated:false,productionWired:false,managedConnectedExecutionQualified:false,modelProviderAccountCalls:false,modelCalls:false,
    sourceAcceptanceQualified:false,transportOnly:true,expiryFencingPhysicalQualified:false,completed:true,qualified:true,attempted:1,runtime:{qualifierSha256:ownValue.qualifierSha256,workerVersion:'4.0.0-alpha.21'},
    boot:{uid:1500,gid:1500,fields:zeroFields,credentialEnvironmentFields:0},parentBefore:parent,parentAfter:parent,parentNamespacesAndStatusUnchanged:true,
    sealedTarget:sealDnsAnswers(ownValue.host,publicPins,assert),summary,counts};
  if(scenario==='qualification-failure'){fullProof.qualified=false;fullProof.completed=false;fullProof.counts={qualified:0,unsupported:0,failed:1,publicHttpsConnectCasesPassed:0};}
  const rawProof=Buffer.from(JSON.stringify(fullProof,null,2)+'\n');
  const fakeCommand=async(command,limits)=>{
    runnerCalls++;let output,stderr=Buffer.alloc(0),ok=true;
    if(command[0]==='secrets')output='[]';
    else if(command[0]==='machine'&&command[1]==='list')output=JSON.stringify([live,...((created&&!destroyed)||scenario==='preexisting-collision'?[own]:[])]);
    else if(command[1]==='run'){assert(!created);created=true;if(scenario==='uncertain-create')ok=false;output='mock create';}
    else if(command[1]==='destroy'){assert(created);assert.equal(command[2],own.id);assert.notEqual(command[2],live.id);destroyed=true;destroyCount++;output='mock destroy';}
    else if(command[1]==='exec'){
      assert(created&&!destroyed);assert.equal(command[2],own.id);execCount++;
      if(limits.ownedStdoutPath.includes('root-owned-image')){
        if(scenario==='probe-error'){output='Exit code: 1\n';stderr=Buffer.from('private mock root-probe failure');}
        else output=JSON.stringify({formatVersion:1,nonce,uid:0,status:'passed',qualifierSha256:ownValue.qualifierSha256,
          provenanceSha256:'a'.repeat(64),provenance:ownPrepared.provenance,workerVersion:'4.0.0-alpha.21'});
      }else if(limits.ownedStdoutPath.includes('publish-atomic')){admitted=true;output=JSON.stringify({nonce,published:true});}
      else if(limits.ownedStdoutPath.includes('read-bounded')){assert(admitted);output=JSON.stringify({status:'ready',sha256:hash(rawProof),proof:fullProof});}
      else if(limits.ownedStdoutPath.includes('collect-exact')){output=JSON.stringify({formatVersion:1,kind:'owned-connected-private-diagnostics',nonce,
        fixtureBase:ownPrepared.input.fixtureBase,home:ownPrepared.input.home,fixtureName:null,completed:true,
        files:[{label:'preload-failure',source:ownPrepared.input.failurePath,status:'read',bytes:7,sha256:hash(Buffer.from('private')),
          metadata:{uid:1500,gid:1500,mode:0o600,nlink:1,size:7},bodyBase64:Buffer.from('private').toString('base64')}]});}
      else throw Error('unexpected owned mock exec');
    }else throw Error('unexpected mock provider operation');
    const stdout=Buffer.from(output),diagnostics={status:ok?0:1,signal:null,failure:ok?null:'MOCK_UNCERTAIN',settled:true,
      stdoutBytes:stdout.length,stderrBytes:stderr.length,stdoutSha256:hash(stdout),stderrSha256:hash(stderr),retainedStdoutBytes:stdout.length,retainedStderrBytes:stderr.length};
    assert(stdout.length<=limits.stdoutLimit&&stderr.length<=8192);
    if(limits.ownedStdoutPath)Object.assign(diagnostics,await preserveOwnedExecOutput(stdout,stderr,limits.ownedStdoutPath,limits.diagnosticPath));
    return {ok,stdout:stdout.toString('utf8'),diagnostics};
  };
  const result=await main(args,{runCommand:fakeCommand,emitProgress:()=>{}}),report=JSON.parse(await fsp.readFile(result.reportPath));
  assert(result.testOnlyMock&&report.testOnlyMock);assert.equal(result.providerCalls,0);assert(runnerCalls>0);assert(result.withinLifetimeBound);
  if(scenario==='preexisting-collision'){assert.equal(destroyCount,0);assert.equal(execCount,0);assert.equal(created,false);assert.equal(result.cleanupVerified,false);assert.equal(report.noDestructiveActionWithoutCreateAttempt,true);}
  else{assert.equal(destroyCount,1);assert(result.cleanupVerified&&result.independentAbsence);}
  assert.equal(result.qualified,['positive','uncertain-create'].includes(scenario));
  if(scenario==='probe-error'){assert.equal(admitted,false);assert(report.steps.some(step=>step.failure==='OWNED_GUEST_EXIT_NONZERO'));
    const step=report.steps.find(step=>step.failure==='OWNED_GUEST_EXIT_NONZERO');assert.equal((await fsp.readFile(step.privateStderrFile)).toString(),'private mock root-probe failure');assert(report.failureDiagnostics.attempted);}
  if(scenario==='qualification-failure')assert(report.failureDiagnostics.completed);
  if(scenario==='wrong-digest')assert.equal(execCount,0);
  return {scenario,reportPath:result.reportPath,cleanup:result.cleanupVerified};
}
const mainScenarios=['positive','probe-error','qualification-failure','uncertain-create','wrong-digest','preexisting-collision'];
const mainResults=[];for(const scenario of mainScenarios)mainResults.push(await exerciseMain(scenario));
console.log(JSON.stringify({kind:'alpha22-connected-controller-selftest',defaultAndDescribeNoProviderCalls:true,candidateAndWorkerIdentitySeparate:true,
  serializedProcStatusFixturesPassed:true,actualSerializedProcStatusPassed:false,actualSerializedOwnNsfsWitnesses:ownProcNamespaces,realTabNewlineFixtures:4,publicDnsRejections:dnsRejections,
  proofMismatchRejections:proofRejections,serializedPreloadMocks:preloadScenarios.length,serializedRootProbes:rootProbeCases,
  ownershipMismatchRejections:ownershipRejections,tagDigestNormalizationAccepted:true,wrongExpectedDigestStillOwnedForCleanup:true,
  atomicGateNoReplacement:true,privatePipePersistenceBeforeParse:true,boundedPrivateDiagnosticScenarios:diagnosticCases+1,
  diagnosticsFailureStillRequiresCleanup:cleanupRequired,mockedFullControllerScenarios:mainScenarios.length,
  preexistingCollisionNeverAdoptedOrDestroyed:true,independentCleanupMocked:true,providerCalls:0,dnsCalls:0,productEdits:false,
  actualConnectedGuestQualification:false,mainResults}));
