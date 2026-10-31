// Private opt-in controller. Preparing/checking this file creates no resources.
// Root supplies the final tagged source/build/image identities before --run.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash,randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {parseArgs} from 'node:util';
import {fileURLToPath} from 'node:url';

const {values}=parseArgs({options:{run:{type:'boolean'},version:{type:'string'},revision:{type:'string'},
  image:{type:'string'},digest:{type:'string'},'base-worker-digest':{type:'string'},'qualifier-sha256':{type:'string'}},
  strict:true,allowPositionals:false});
assert.equal(values.run,true,'explicit --run required');
assert.match(values.version||'',/^\d+\.\d+\.\d+-alpha\.\d+$/);
assert.match(values.revision||'',/^[0-9a-f]{40}$/);
assert.match(values.image||'',/^registry\.fly\.io\/rudder-workers-exla:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/);
for(const key of ['digest','base-worker-digest'])assert.match(values[key]||'',/^sha256:[0-9a-f]{64}$/);
assert.match(values['qualifier-sha256']||'',/^[0-9a-f]{64}$/);
assert.notEqual(values.digest,values['base-worker-digest'],'derived image must differ from base');
const baseWorkerImage='registry.fly.io/rudder-workers-exla:'+values.version;
assert.notEqual(values.image,baseWorkerImage,'use a separate derived qualification image tag');
const exec=promisify(execFile),fly='/workspace/home/.fly/bin/flyctl',app='rudder-workers-exla';
const nonce=randomUUID(),name='rudder-alpha12-netqual-'+nonce.slice(0,8),role='alpha12-network-qualification';
const forbidden='8edd225a7d0378';
const directory=path.join(path.dirname(fileURLToPath(import.meta.url)),'network-'+values.version+'-'+nonce);
const reportPath=path.join(directory,'report.json');
await fs.mkdir(directory,{mode:0o700});
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const quote=value=>"'"+value.replaceAll("'","'\\''")+"'";
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const emit=step=>console.log(JSON.stringify({step,report:reportPath}));
const provenance={formatVersion:1,version:values.version,revision:values.revision,baseWorkerImage,
  baseWorkerDigest:values['base-worker-digest'],qualifierSha256:values['qualifier-sha256']};
const report={formatVersion:1,kind:role,nonce,name,requestedImage:values.image,expectedDigest:values.digest,
  provenance,startedAt:new Date().toISOString(),steps:[],productionWired:false,egressQualified:false,
  liveAttemptAuthorized:false,limits:[
    'Owned disposable only; no services/mounts/app secrets, user/provider/workspace credentials or models.',
    'Real managed image bootstrap runs; Node preload never evaluates home/supervisor modules.',
    'Derived image digest is checked before host-written admission gate permits namespace probes.',
    'Baked provenance plus binary hash checked; base ancestry still requires retained pinned-FROM build evidence.',
    'Kernel/config primitives only, not connected guest networking, public egress, packet transport or previews.'
  ]};

async function preload(input) {
  const fs=await import('node:fs'),cp=await import('node:child_process'),crypto=await import('node:crypto');
  const {default:assert}=await import('node:assert/strict'),os=await import('node:os');
  const result={formatVersion:1,kind:'alpha12-network-qualification',nonce:input.nonce,version:input.version,
    revision:input.revision,supervisorModuleEvaluated:false,egressQualified:false,liveAttemptAuthorized:false,modes:[]};
  let stage='admission-gate',parentUser,parentNet;
  const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
  const status=()=>{
    const text=fs.readFileSync('/proc/self/status','utf8');
    const wanted=['Uid','Gid','Groups','NoNewPrivs','CapEff','CapPrm','CapInh','CapAmb','CapBnd'],fields=new Map();
    // A regex literal survives function.toString() without string/template
    // escape levels. Keep values on their own real tab/newline-delimited row.
    for(const match of text.matchAll(/^([A-Za-z]+):[ \t]*([^\r\n]*)$/gm)){
      if(wanted.includes(match[1]))fields.set(match[1],match[2].trim());
    }
    return Object.fromEntries(wanted.map(key=>[key,fields.get(key)]));
  };
  const witness=kind=>{
    const fd=fs.openSync('/proc/self/ns/'+kind,'r'),stat=fs.fstatSync(fd,{bigint:true});
    return {fd,device:stat.dev.toString(),inode:stat.ino.toString()};
  };
  const identity=value=>({device:value.device,inode:value.inode});
  const current=kind=>{const value=witness(kind);fs.closeSync(value.fd);return identity(value);};
  const zero=value=>{
    for(const field of ['effective','permitted','inheritable','ambient','bounding'])assert.equal(value[field],'0000000000000000');
    assert.equal(value.noNewPrivileges,true);assert(value.supportedBits>0&&value.supportedBits<=64);
  };
  const digestFile=file=>{
    const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW),stat=fs.fstatSync(fd);
    try{
      assert(stat.isFile()&&stat.nlink===1&&stat.uid===0&&stat.size>0&&stat.size<=128*1024*1024);
      assert.equal(stat.mode&0o222,0);assert(stat.mode&0o001);
      const hash=crypto.createHash('sha256'),buffer=Buffer.alloc(64*1024);let bytes=0;
      for(;;){const count=fs.readSync(fd,buffer,0,buffer.length,null);if(!count)break;
        bytes+=count;assert(bytes<=128*1024*1024);hash.update(buffer.subarray(0,count));}
      assert.equal(bytes,stat.size);return hash.digest('hex');
    }finally{fs.closeSync(fd);}
  };
  try{
    const deadline=Date.now()+180000;let admitted=false;
    while(Date.now()<deadline){
      try{
        const fd=fs.openSync(input.gatePath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);
        try{const stat=fs.fstatSync(fd);assert(stat.isFile()&&stat.uid===1500&&stat.nlink===1&&stat.size<=1024);
          const gate=JSON.parse(fs.readFileSync(fd,'utf8'));assert.deepEqual(gate,{nonce:input.nonce,digest:input.digest,revision:input.revision});admitted=true;
        }finally{fs.closeSync(fd);}
        break;
      }catch(error){if(error.code!=='ENOENT')throw error;await sleep(100);}
    }
    assert(admitted);result.admittedAfterResolvedDigest=true;
    stage='boot-identity';
    assert.equal(Object.keys(process.env).filter(key=>/TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|AUTH_KEY|SSH_AUTH_SOCK|AWS_ACCESS_KEY/i.test(key)).length,0);
    assert.equal(process.getuid(),1500);assert.equal(process.getgid(),1500);
    const initial=status();assert.equal(initial.Uid,'1500\t1500\t1500\t1500');assert.equal(initial.Gid,'1500\t1500\t1500\t1500');
    assert.equal(initial.Groups,'');assert.equal(initial.NoNewPrivs,'1');
    for(const field of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])assert.match(initial[field],/^0+$/);
    assert.equal(process.env.RUDDER_WORKER_MANAGED_EXECUTION,'1');assert.equal(process.env.RUDDER_WORKER_SKIP_AGENT_UPDATE,'1');
    assert.equal(process.env.RUDDER_WORKER_FUSE_READY,'1');
    assert.equal(JSON.parse(fs.readFileSync('/opt/rudder/package.json','utf8')).version,input.version);
    result.boot={uid:process.getuid(),gid:process.getgid(),fields:initial,node:process.version,kernel:os.release(),credentialEnvironmentFields:0};
    stage='binary-provenance';
    const directory=fs.lstatSync('/opt/rudder/qualification');assert(directory.isDirectory()&&!directory.isSymbolicLink()&&directory.uid===0);
    assert.equal(directory.mode&0o022,0);
    const provenancePath='/opt/rudder/qualification/provenance.json';
    const manifestFd=fs.openSync(provenancePath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);
    let manifestBytes;
    try{const stat=fs.fstatSync(manifestFd);assert(stat.isFile()&&stat.nlink===1&&stat.uid===0&&stat.size<=4096);assert.equal(stat.mode&0o222,0);
      manifestBytes=fs.readFileSync(manifestFd);assert.deepEqual(JSON.parse(manifestBytes),input.provenance);
    }finally{fs.closeSync(manifestFd);}
    result.provenance=input.provenance;result.provenanceSha256=crypto.createHash('sha256').update(manifestBytes).digest('hex');
    const binary='/opt/rudder/qualification/rudder-network-qualifier';assert.equal(digestFile(binary),input.provenance.qualifierSha256);
    result.qualifierSha256=input.provenance.qualifierSha256;
    parentUser=witness('user');parentNet=witness('net');
    result.parentBefore={user:identity(parentUser),net:identity(parentNet)};
    const childEnv={PATH:'/usr/local/bin:/usr/bin:/bin',HOME:'/nonexistent',TMPDIR:'/workspace',LANG:'C',LC_ALL:'C'};
    assert(!Object.hasOwn(childEnv,'NODE_OPTIONS'));
    const modes=['namespace-baseline','namespace-setup','namespace-and-exec','proof-after-user-reset','drop-and-exec','proof-wrong-process'];
    for(const mode of modes){
      stage=mode;
      const output=cp.spawnSync(binary,['--qualification-only',mode],{cwd:'/workspace',env:childEnv,
        stdio:['ignore','pipe','pipe'],encoding:'utf8',timeout:20000,killSignal:'SIGKILL',maxBuffer:64*1024});
      const record={mode,outcome:'failed',exitCode:output.status,signal:output.signal,stdout:output.stdout||'',stderr:output.stderr||''};
      result.modes.push(record);
      assert(!output.error);assert.equal(output.signal,null);assert(Buffer.byteLength(record.stdout)<=64*1024&&Buffer.byteLength(record.stderr)<=4096);
      assert.deepEqual({user:current('user'),net:current('net')},result.parentBefore);
      assert.deepEqual(status(),initial);record.parentNamespacesAndStatusUnchanged=true;
      const events=record.stdout.trim().split('\n').filter(Boolean).map(line=>JSON.parse(line));record.events=events;
      assert(events.every(event=>event.hostFallback!==true&&event.egressQualified!==true&&event.liveAttemptAuthorized!==true));
      if(output.status===77){assert.equal(events.length,1);assert.equal(events[0].status,'unsupported');assert.equal(events[0].hostFallback,false);
        record.outcome='unsupported';continue;}
      assert.equal(output.status,0);
      if(mode==='namespace-baseline'){
        assert.equal(events.length,1);assert.equal(events[0].status,'verified_no_namespace_change');assert.equal(events[0].error,'ForeignNamespace');
      }else if(mode==='namespace-setup'||mode==='namespace-and-exec'){
        assert.equal(events.length,mode==='namespace-and-exec'?2:1);const event=events[0];
        assert.equal(event.status,'qualified_kernel_primitives_only');assert.equal(event.uid,1500);assert.equal(event.loopbackVerified,true);
        assert.equal(event.parentWitnessesClosed,true);assert.equal(event.allowedNamespaceWitnesses,2);
        assert.equal(event.egressQualified,false);assert.equal(event.liveAttemptAuthorized,false);assert.equal(event.hostFallback,false);
        assert.deepEqual(identity(event.parentUser),result.parentBefore.user);assert.deepEqual(identity(event.parentNet),result.parentBefore.net);
        assert.equal(event.childUser.kind,'User');assert.equal(event.childNet.kind,'Network');
        assert.notDeepEqual(identity(event.childUser),identity(event.parentUser));assert.notDeepEqual(identity(event.childNet),identity(event.parentNet));
        assert(BigInt('0x'+event.beforeDrop.effective)&(1n<<12n));assert.notEqual(event.beforeDrop.bounding,'0000000000000000');zero(event.afterDrop);
        if(mode==='namespace-and-exec'){assert.equal(events[1].kind,'capability-exec-readback');zero(events[1].after);assert.equal(events[1].liveAttemptAuthorized,false);}
      }else if(mode==='proof-after-user-reset'){
        assert.equal(events.length,1);assert.equal(events[0].status,'verified_reset_refused');
      }else if(mode==='drop-and-exec'){
        assert.equal(events.length,2);assert.equal(events[0].kind,'capability-drop');zero(events[0].after);
        assert.equal(events[1].kind,'capability-exec-readback');zero(events[1].after);assert.equal(events[1].liveAttemptAuthorized,false);
      }else{assert.equal(events.length,1);assert.equal(events[0].status,'verified_foreign_process_refused');}
      record.outcome='qualified';
    }
    result.parentAfter={user:current('user'),net:current('net')};assert.deepEqual(result.parentAfter,result.parentBefore);
    result.parentNamespacesAndStatusUnchanged=true;result.completed=true;
    result.counts={qualified:result.modes.filter(mode=>mode.outcome==='qualified').length,
      unsupported:result.modes.filter(mode=>mode.outcome==='unsupported').length,failed:0};
    result.qualified=result.counts.qualified===6&&result.counts.unsupported===0;
  }catch(error){result.completed=false;result.qualified=false;result.failedStage=stage;
    result.errorCode=typeof error.code==='string'?error.code:'ASSERTION';}
  finally{
    if(parentUser)fs.closeSync(parentUser.fd);if(parentNet)fs.closeSync(parentNet.fd);
    result.counts={qualified:result.modes.filter(mode=>mode.outcome==='qualified').length,
      unsupported:result.modes.filter(mode=>mode.outcome==='unsupported').length,
      failed:result.modes.filter(mode=>mode.outcome==='failed').length};
    result.verifiedAt=new Date().toISOString();
    fs.writeFileSync(input.proofPath,JSON.stringify(result,null,2)+'\n',{mode:0o600,flag:'wx'});
  }
  // Never return to evaluate home/supervisor, including unsupported/failure.
  setTimeout(()=>process.exit(0),480000);await new Promise(()=>{});
}

const gatePath='/workspace/network-gate-'+nonce+'.json',proofPath='/workspace/network-proof-'+nonce+'.json';
const input={nonce,version:values.version,revision:values.revision,digest:values.digest,provenance,gatePath,proofPath};
const preloadSource='await ('+preload.toString()+')('+JSON.stringify(input)+');';
await fs.writeFile(path.join(directory,'preload.mjs'),preloadSource+'\n',{mode:0o600});
const config={image:values.image,guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},restart:{policy:'no'},auto_destroy:false,
  services:[],mounts:[],env:{RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_HOME:'1',RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',
    NODE_OPTIONS:'--import=data:text/javascript;base64,'+Buffer.from(preloadSource).toString('base64')},
  metadata:{rudder_role:role,qualification:nonce,qualification_revision:values.revision}};
const configPath=path.join(directory,'machine.json');await fs.writeFile(configPath,JSON.stringify(config,null,2)+'\n',{mode:0o600});
report.controllerSha256=sha(await fs.readFile(fileURLToPath(import.meta.url)));report.preloadSha256=sha(Buffer.from(preloadSource));
report.configSha256=sha(await fs.readFile(configPath));
let phase='preflight',owned,createdDescriptor;
async function command(args,label,timeout=25000){phase=label;const began=Date.now();
  const result=await exec(fly,args,{timeout,maxBuffer:4*1024*1024});report.steps.push({step:label,ms:Date.now()-began});return result.stdout;}
async function inventory(){return JSON.parse(await command(['machine','list','-a',app,'--json'],'filtered-inventory'));}
function matchesRequested(machine){
  const empty=value=>value===undefined||(Array.isArray(value)&&value.length===0);
  return machine.id!==forbidden&&machine.name===name&&machine.config?.metadata?.qualification===nonce
    &&machine.config?.metadata?.rudder_role===role&&machine.config?.metadata?.qualification_revision===values.revision
    &&(machine.config?.image===config.image||(
      typeof machine.config?.image==='string'&&machine.config.image.startsWith(config.image+'@')
      &&/^sha256:[0-9a-f]{64}$/.test(machine.config.image.slice(config.image.length+1))
      &&(!machine.image_ref?.digest||machine.config.image===config.image+'@'+machine.image_ref.digest)
    ))&&empty(machine.config?.services)&&empty(machine.config?.mounts)&&empty(machine.config?.files);
}
function matches(machine){return matchesRequested(machine)&&(!owned||machine.id===owned)
  &&(!createdDescriptor?.observedDigest||machine.image_ref?.digest===createdDescriptor.observedDigest);}
function rememberCreated(machine){
  assert(matches(machine));
  if(owned)assert.equal(machine.id,owned);else owned=machine.id;
  const observed=machine.image_ref?.digest;
  if(observed!==undefined)assert.match(observed,/^sha256:[0-9a-f]{64}$/);
  if(!createdDescriptor){createdDescriptor={id:owned,name,nonce,role,revision:values.revision,
    requestedImage:config.image,observedDigest:observed??null,services:0,mounts:0,files:0};}
  else if(!createdDescriptor.observedDigest&&observed)createdDescriptor.observedDigest=observed;
  report.createdDescriptor={...createdDescriptor};
}
async function ownedExec(script,label){assert(owned&&owned!==forbidden);return command(['machine','exec',owned,'-a',app,
  '/bin/sh -c '+quote(script),'--timeout','15'],label);}
try{
  assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-empty-app-secrets')),[]);
  assert(!(await inventory()).some(machine=>machine.name===name||machine.config?.metadata?.qualification===nonce));
  emit('creating-owned-qualification');
  try{await command(['machine','run',values.image,'-a',app,'--machine-config',configPath,'--name',name,
    '--region','sjc','--detach','--skip-dns-registration'],'create-owned-qualification',120000);}
  catch{report.createReplyUncertain=true;}
  let machine,startRequested=false;const deadline=Date.now()+180000;
  while(Date.now()<deadline){const found=(await inventory()).filter(matches);assert(found.length<=1);
    if(found.length){machine=found[0];rememberCreated(machine);
      if(machine.state==='started')break;
      if(machine.state==='created'&&!startRequested){startRequested=true;
        try{await command(['machine','start',owned,'-a',app],'start-reconciled-owned-qualification',90000);}catch{report.startReplyUncertain=true;}}}
    await delay(2000);}
  assert(machine&&matches(machine));assert.equal(machine.state,'started');assert.equal(machine.image_ref.digest,values.digest);
  assert(!machine.config.services?.length&&!machine.config.mounts?.length&&!machine.config.files?.length);
  const allowed=[...Object.keys(config.env),'PRIMARY_REGION','FLY_PRIMARY_REGION'];assert(Object.keys(machine.config.env||{}).every(key=>allowed.includes(key)));
  for(const[key,value]of Object.entries(config.env))assert.equal(machine.config.env[key],value);
  report.machine={id:owned,name,nonce,role,instance:machine.instance_id,region:machine.region,digest:machine.image_ref.digest};
  assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'recheck-empty-app-secrets')),[]);
  const gate=JSON.stringify({nonce,digest:values.digest,revision:values.revision});
  const writeGate=`const fs=require('node:fs');const target=${JSON.stringify(gatePath)},temporary=target+'.pending';const fd=fs.openSync(temporary,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);fs.writeFileSync(fd,${JSON.stringify(gate)});fs.fchownSync(fd,1500,1500);fs.fsyncSync(fd);fs.closeSync(fd);if(fs.existsSync(target))throw Error('admission gate already exists');fs.renameSync(temporary,target);const directory=fs.openSync('/workspace',fs.constants.O_RDONLY|fs.constants.O_DIRECTORY);fs.fsyncSync(directory);fs.closeSync(directory);`;
  await ownedExec('/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e '+quote(writeGate),'release-after-verified-derived-digest');
  emit('owned-image-verified-and-gate-released');
  let proof;
  for(let attempt=0;attempt<65;attempt++){
    try{const output=await ownedExec('/bin/cat '+quote(proofPath),'read-bounded-generated-proof');assert(output.length<=256*1024);proof=JSON.parse(output.trim());break;}
    catch{await delay(2000);}}
  assert(proof&&proof.nonce===nonce&&proof.version===values.version&&proof.revision===values.revision);
  await fs.writeFile(path.join(directory,'guest-proof.json'),JSON.stringify(proof,null,2)+'\n',{mode:0o600});
  report.proofSha256=sha(await fs.readFile(path.join(directory,'guest-proof.json')));
  report.completed=proof.completed===true;report.qualified=proof.qualified===true;report.counts=proof.counts;
  assert.equal(proof.completed,true);assert.equal(proof.admittedAfterResolvedDigest,true);
  assert.equal(proof.parentNamespacesAndStatusUnchanged,true);assert.equal(proof.egressQualified,false);
  assert.equal(proof.liveAttemptAuthorized,false);assert.equal(proof.supervisorModuleEvaluated,false);
  assert.equal(proof.qualifierSha256,values['qualifier-sha256']);
  if(!report.qualified)process.exitCode=2;
}catch(error){report.completed=false;report.qualified=false;report.failedPhase=phase;
  report.errorCode=typeof error.code==='string'?error.code:'ASSERTION';process.exitCode=1;emit('qualification-failed');}
finally{
  try{const found=(await inventory()).filter(matches);assert(found.length<=1);
    for(const machine of found){rememberCreated(machine);
      // Cleanup binds the actual observed created image, even when it differs
      // from the expected qualification digest. Never substitute expectation
      // for ownership, and never accept a later changed image/configuration.
      assert(matches(machine));
      try{await command(['machine','destroy',owned,'-a',app,'--force'],'destroy-exact-owned-qualification');}catch{report.destroyReplyUncertain=true;}}
    const absent=machines=>!machines.some(machine=>machine.id===owned||machine.name===name||machine.config?.metadata?.qualification===nonce);
    report.cleanupVerified=absent(await inventory());report.independentAbsence=absent(await inventory());
    assert(report.cleanupVerified&&report.independentAbsence);
    await fs.writeFile(path.join(directory,'cleanup.json'),JSON.stringify({formatVersion:1,id:owned,name,nonce,role,
      absentByIdNameNonce:true,independentInventoryAbsent:true,verifiedAt:new Date().toISOString()},null,2)+'\n',{mode:0o600});
  }catch{report.cleanupVerified=false;process.exitCode=1;}
  report.verifiedAt=new Date().toISOString();await fs.writeFile(reportPath,JSON.stringify(report,null,2)+'\n',{mode:0o600});
  emit('qualification-finished');
}
