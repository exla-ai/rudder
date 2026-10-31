// Read-only local status and mock-only preload/ownership checks. Never evaluates
// controller main or starts a process, namespace transition, provider call or VM.
import fs from 'node:fs/promises';
import syncFs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
const source=await fs.readFile(new URL('./qualify-network.mjs',import.meta.url),'utf8');
const start=source.indexOf('async function preload(input) {'),end=source.indexOf('\nconst gatePath=',start);
assert(start>0&&end>start);const functionSource=source.slice(start,end);
// Extract from the actual function after toString(), as used by the preload.
// JSON round-trip the extracted closure source before compiling it in isolation.
// Never run preload itself with real imports or evaluate controller main.
const serializedPreload=new vm.Script('('+functionSource+')').runInNewContext().toString();
const statusStart=serializedPreload.indexOf('const status='),statusEnd=serializedPreload.indexOf('\n  const witness=',statusStart);
assert(statusStart>0&&statusEnd>statusStart);
const statusSource=JSON.parse(JSON.stringify(serializedPreload.slice(statusStart+'const status='.length,statusEnd).trim().replace(/;$/,'')));
const statusWith=adapter=>new vm.Script('('+statusSource+')').runInNewContext({fs:adapter});
const wanted=['Uid','Gid','Groups','NoNewPrivs','CapEff','CapPrm','CapInh','CapAmb','CapBnd'];
const actualText=syncFs.readFileSync('/proc/self/status','utf8'),expectedActual={};
for(const line of actualText.split(String.fromCharCode(10))){const colon=line.indexOf(':');if(colon<0)continue;
  const key=line.slice(0,colon);if(wanted.includes(key))expectedActual[key]=line.slice(colon+1).trim();}
assert.equal(Object.keys(expectedActual).length,wanted.length);
assert.deepEqual(JSON.parse(JSON.stringify(statusWith(syncFs)())),expectedActual);
const tab=String.fromCharCode(9),lf=String.fromCharCode(10),cr=String.fromCharCode(13);
const fixtureValues={Uid:[1500,1500,1500,1500].join(tab),Gid:[1500,1500,1500,1500].join(tab),Groups:'1500 1501',
  NoNewPrivs:'1',CapEff:'0000000000000001',CapPrm:'0000000000000002',CapInh:'0000000000000004',
  CapAmb:'0000000000000008',CapBnd:'000001ffffffffff'};
for(const newline of [lf,cr+lf])for(const groups of [fixtureValues.Groups,'']){
  const expected={...fixtureValues,Groups:groups};
  const fixture=['Name:'+tab+'fixture',...wanted.map(key=>key+':'+tab+expected[key]),'Threads:'+tab+'1','Other:'+tab+'must not spill'].join(newline)+newline;
  const result=statusWith({readFileSync(file,encoding){assert.equal(file,'/proc/self/status');assert.equal(encoding,'utf8');return fixture;}})();
  assert.deepEqual(JSON.parse(JSON.stringify(result)),expected);
}
const raw={};for(const mode of ['namespace-baseline','namespace-setup','namespace-and-exec','proof-after-user-reset']){
  raw[mode]=JSON.parse(await fs.readFile('/workspace/rudder-alpha12-network-runtime/'+mode+'.json','utf8'));
}
const setup=raw['namespace-setup'].stdout.trim().split('\n').map(JSON.parse),zero=setup[0].afterDrop;
const fixtures={...raw,
  'drop-and-exec':{exitCode:0,stdout:JSON.stringify({kind:'capability-drop',after:zero})+'\n'+JSON.stringify({kind:'capability-exec-readback',after:zero,liveAttemptAuthorized:false})+'\n',stderr:''},
  'proof-wrong-process':{exitCode:0,stdout:JSON.stringify({status:'verified_foreign_process_refused',hostFallback:false})+'\n',stderr:''}};
const binaryBytes=Buffer.from('mock qualification bytes, never executed');
const qualifierSha256=crypto.createHash('sha256').update(binaryBytes).digest('hex');
const input={nonce:'00000000-0000-0000-0000-000000000001',version:'4.0.0-alpha.12',revision:'1'.repeat(40),digest:'sha256:'+'2'.repeat(64),
  gatePath:'/workspace/mock-gate.json',proofPath:'/workspace/mock-proof.json',provenance:{formatVersion:1,version:'4.0.0-alpha.12',revision:'1'.repeat(40),
    baseWorkerImage:'registry.fly.io/rudder-workers-exla:4.0.0-alpha.12',baseWorkerDigest:'sha256:'+'3'.repeat(64),qualifierSha256}};
async function exercise(scenario){
  let next=10,spawned=0,currentNet=setup[0].parentNet.inode;const descriptors=new Map();let resolveProof;
  const ready=new Promise(resolve=>{resolveProof=resolve;});
  const gate=JSON.stringify({nonce:input.nonce,digest:scenario==='bad-gate'?'wrong':input.digest,revision:input.revision});
  const provenance=JSON.stringify({...input.provenance,...(scenario==='bad-provenance'?{revision:'0'.repeat(40)}:{})});
  const status='Uid:\t1500\t1500\t1500\t1500\nGid:\t1500\t1500\t1500\t1500\nGroups:\t \nNoNewPrivs:\t1\n'+
    ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'].map(key=>key+':\t'+(scenario==='bad-boot'&&key==='CapBnd'?'0000000000000001':'0000000000000000')).join('\n')+'\n';
  const regular=(uid,mode,size)=>({isFile:()=>true,uid,mode,nlink:1,size});
  const fsMock={constants:{O_RDONLY:0,O_NOFOLLOW:1},
    openSync(file){const fd=next++;descriptors.set(fd,{file,offset:0,inode:file.endsWith('/user')?setup[0].parentUser.inode:currentNet});return fd;},
    closeSync(fd){assert(descriptors.delete(fd));},
    fstatSync(fd){const entry=descriptors.get(fd);assert(entry);if(entry.file.includes('/proc/self/ns/'))return{dev:4n,ino:BigInt(entry.inode)};
      if(entry.file===input.gatePath)return regular(1500,0o600,gate.length);
      if(entry.file.endsWith('provenance.json'))return regular(0,0o444,provenance.length);
      return regular(0,0o555,binaryBytes.length);},
    lstatSync(){return{isDirectory:()=>true,isSymbolicLink:()=>false,uid:0,mode:0o555};},
    readFileSync(file){if(typeof file==='number'){const name=descriptors.get(file).file;return name===input.gatePath?gate:Buffer.from(provenance);}
      if(file==='/proc/self/status')return status;if(file==='/opt/rudder/package.json')return JSON.stringify({version:input.version});throw Error('unexpected mocked read');},
    readSync(fd,buffer,offset,length){const entry=descriptors.get(fd);const count=Math.min(length,binaryBytes.length-entry.offset);
      binaryBytes.copy(buffer,offset,entry.offset,entry.offset+count);entry.offset+=count;return count;},
    writeFileSync(file,bytes){assert.equal(file,input.proofPath);resolveProof(JSON.parse(bytes));}};
  const cpMock={spawnSync(binary,args,options){spawned++;assert.equal(binary,'/opt/rudder/qualification/rudder-network-qualifier');
    assert.equal(args[0],'--qualification-only');assert(!Object.hasOwn(options.env,'NODE_OPTIONS'));assert.equal(options.stdio[0],'ignore');
    if(scenario==='parent-change')currentNet='99999';
    const output=scenario==='unsupported'&&args[1]==='proof-wrong-process'
      ?{exitCode:77,stdout:JSON.stringify({status:'unsupported',hostFallback:false,errno:1})+'\n',stderr:''}:fixtures[args[1]];
    assert(output);return{status:output.exitCode,signal:null,stdout:output.stdout,stderr:output.stderr};}};
  const context=vm.createContext({Buffer,Date,process:{env:{RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',RUDDER_WORKER_FUSE_READY:'1'},
    getuid:()=>1500,getgid:()=>1500,version:'mock-only',exit(){throw Error('mock timer must not execute');}},setTimeout(){return 1;}});
  const mocks={'node:fs':fsMock,'node:child_process':cpMock,'node:crypto':{createHash:crypto.createHash},'node:assert/strict':{default:assert},'node:os':{release:()=> 'mock-only'}};
  const modules=new Map();
  const script=new vm.Script(functionSource+'\npreload',{importModuleDynamically:async name=>{
    if(!modules.has(name)){const mock=mocks[name];assert(mock);const module=new vm.SyntheticModule(Object.keys(mock),function(){for(const[key,value]of Object.entries(mock))this.setExport(key,value);},{context});
      await module.link(()=>{});await module.evaluate();modules.set(name,module);}return modules.get(name);}});
  const fn=script.runInContext(context);const local=vm.runInContext('JSON.parse('+JSON.stringify(JSON.stringify(input))+')',context);
  const running=fn(local);running.catch(error=>resolveProof({unexpectedMockFailure:String(error)}));
  let timer;try{const result=await Promise.race([ready,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('mock preload did not produce a report')),1000);})]);
    assert(!result.unexpectedMockFailure);assert.equal(descriptors.size,0);return{result,spawned};
  }finally{clearTimeout(timer);}
}
const positive=await exercise('positive');assert.equal(positive.result.qualified,true);assert.equal(positive.spawned,6);
assert.equal(positive.result.counts.qualified,6);assert.equal(positive.result.counts.unsupported,0);
const unsupported=await exercise('unsupported');assert.equal(unsupported.result.completed,true);assert.equal(unsupported.result.qualified,false);
assert.equal(unsupported.result.counts.unsupported,1);assert.equal(unsupported.result.counts.qualified,5);
for(const scenario of ['bad-gate','bad-provenance','bad-boot','parent-change']){
  const value=await exercise(scenario);assert.equal(value.result.completed,false);assert.equal(value.result.qualified,false);
  assert.equal(value.spawned,scenario==='parent-change'?1:0);
}
const matchStart=source.indexOf('function matchesRequested(machine)'),matchEnd=source.indexOf('\nasync function ownedExec',matchStart);
assert(matchStart>0&&matchEnd>matchStart);
const requestedImage='registry.fly.io/rudder-workers-exla:owned-test',observedDigest='sha256:'+'a'.repeat(64),wrongExpectedDigest='sha256:'+'b'.repeat(64);
const makeOwnership=()=>{
  const context={forbidden:'8edd225a7d0378',name:'owned',nonce:input.nonce,role:'qualification',values:{revision:input.revision,digest:wrongExpectedDigest},
    config:{image:requestedImage},owned:undefined,createdDescriptor:undefined,report:{},assert};
  const helpers=new vm.Script(source.slice(matchStart,matchEnd)+'\n({matches,rememberCreated})').runInNewContext(context);
  return{context,...helpers};
};
const ownership=makeOwnership(),{matches,rememberCreated}=ownership;
const candidate={id:'owned-id',name:'owned',image_ref:{digest:observedDigest},config:{image:requestedImage,services:[],mounts:[],files:[],
  metadata:{qualification:input.nonce,rudder_role:'qualification',qualification_revision:input.revision}}};
assert.equal(matches(candidate),true);rememberCreated(candidate);
assert.equal(ownership.context.createdDescriptor.observedDigest,observedDigest);
assert.notEqual(ownership.context.values.digest,observedDigest);
assert.equal(matches(candidate),true,'wrong qualification expectation must not prevent exact owned cleanup');
assert.equal(matches({...candidate,config:{...candidate.config,image:requestedImage+'@'+observedDigest}}),true,'Fly tag@resolved-digest is the same owned image');
assert.equal(matches({...candidate,config:{...candidate.config,image:requestedImage+'@'+wrongExpectedDigest}}),false,'a different pinned image cannot reuse ownership');
assert.equal(matches({...candidate,config:{...candidate.config,image:requestedImage+'@not-a-digest'}}),false);
const normalizedFirst=makeOwnership(),normalizedCandidate={...candidate,config:{...candidate.config,image:requestedImage+'@'+observedDigest}};
assert.equal(normalizedFirst.matches(normalizedCandidate),true);normalizedFirst.rememberCreated(normalizedCandidate);
assert.equal(normalizedFirst.context.createdDescriptor.observedDigest,observedDigest);
assert.equal(normalizedFirst.matches(normalizedCandidate),true);

assert.equal(matches({...candidate,id:'8edd225a7d0378'}),false);assert.equal(matches({...candidate,id:'another-id'}),false);
assert.equal(matches({...candidate,name:'another'}),false);assert.equal(matches({...candidate,config:{...candidate.config,image:'another-image'}}),false);
assert.equal(matches({...candidate,image_ref:{digest:wrongExpectedDigest}}),false);assert.equal(matches({...candidate,image_ref:{}}),false);
for(const field of ['qualification','rudder_role','qualification_revision']){
  assert.equal(matches({...candidate,config:{...candidate.config,metadata:{...candidate.config.metadata,[field]:'another'}}}),false);
}
for(const field of ['services','mounts','files'])for(const unsafe of [[{}],{},null]){
  assert.equal(matches({...candidate,config:{...candidate.config,[field]:unsafe}}),false);
}
assert.throws(()=>rememberCreated({...candidate,image_ref:{digest:wrongExpectedDigest}}));
assert.equal(ownership.context.createdDescriptor.observedDigest,observedDigest);
const lateDigest=makeOwnership(),notYetResolved={...candidate,image_ref:{}};
lateDigest.rememberCreated(notYetResolved);assert.equal(lateDigest.context.createdDescriptor.observedDigest,null);
lateDigest.rememberCreated(candidate);assert.equal(lateDigest.context.createdDescriptor.observedDigest,observedDigest);
assert.equal(lateDigest.matches({...candidate,image_ref:{digest:wrongExpectedDigest}}),false);
console.log(JSON.stringify({serializedActualStatusClosureRealProcPassed:true,realTabNewlineFixturesPassed:4,mockScenariosPassed:6,
  exactOwnershipGuardPassed:true,observedImageOwnershipGuardPassed:true,wrongExpectedDigestStillOwned:true,
  controllerMainExecuted:false,providerCalls:0,realGuestQualification:false}));
