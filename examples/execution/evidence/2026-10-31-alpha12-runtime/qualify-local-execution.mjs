// Private opt-in LOCAL EXECUTION controller. Preparing/checking this file creates no resources.
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
const nonce=randomUUID(),name='rudder-alpha12-execqual-'+nonce.slice(0,8),role='alpha12-local-execution-qualification';
const forbidden='8edd225a7d0378';
const directory=path.join(path.dirname(fileURLToPath(import.meta.url)),'local-execution-'+values.version+'-'+nonce);
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
  liveAttemptAuthorized:false,networkQualificationSeparate:true,specVersions:[1,2],remoteExecutionQualified:false,
  allFiveGuestCapabilitiesQualified:false,guest:{cpu_kind:'performance',cpus:1,memory_mb:2048},maxPreloadLifetimeSeconds:1200,
  fixtureSupplements:['busybox-static in derived image only','/usr/bin/node alias to /usr/local/bin/node in derived image only'],limits:[
    'Owned disposable only; no services/mounts/app secrets, user/provider/workspace credentials or models.',
    'Real managed image bootstrap runs; Node preload never evaluates home/supervisor modules.',
    'Derived image digest is checked before host-written admission gate permits namespace probes.',
    'Baked provenance plus binary hash checked; base ancestry still requires retained pinned-FROM build evidence.',
    'Legacy local Spec1/2 fixture only; not all-five guest capabilities, network isolation, general packet egress, remote execution or production authority.',
    'Derived-image-only busybox-static and /usr/bin/node alias supplement fixture tools; stock SDK/default profile is not qualified.',
    'Generated fixture own-auth/OWN_API_KEY/broker tokens are disposable random test values, never user/provider credentials.',
    'Network six-mode qualification is a separate controller/report; optional Git fixture is disabled here.',
    'Performance1/2048MiB matches prior execution fixture evidence; this does not qualify shared CPU scheduling or stock profile tool availability.'
  ]};

async function preload(input) {
  // Independent finite preload lifetime; synchronous child calls also have
  // bounded process timeouts. Controller cleanup requires exact VM absence.
  setTimeout(()=>process.exit(124),1200000);
  const fs=await import('node:fs'),cp=await import('node:child_process'),crypto=await import('node:crypto');
  const {default:assert}=await import('node:assert/strict'),os=await import('node:os');
  const result={formatVersion:1,kind:'alpha12-local-execution-qualification',nonce:input.nonce,version:input.version,
    revision:input.revision,supervisorModuleEvaluated:false,egressQualified:false,liveAttemptAuthorized:false,
    specVersions:[1,2],networkQualificationSeparate:true,networkIsolated:false,allFiveGuestCapabilitiesQualified:false,remoteExecutionQualified:false,
    fixtureSupplements:['busybox-static in derived image only','/usr/bin/node alias to /usr/local/bin/node in derived image only'],modes:[]};
  let stage='admission-gate',parentUser,parentNet,parentMnt;
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
  const digestFile=file=>{
    const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW),stat=fs.fstatSync(fd);
    try{
      assert(stat.isFile()&&stat.nlink===1&&stat.uid===0&&stat.size>0&&stat.size<=256*1024*1024);
      assert.equal(stat.mode&0o777,0o555);
      const hash=crypto.createHash('sha256'),buffer=Buffer.alloc(64*1024);let bytes=0;
      for(;;){const count=fs.readSync(fd,buffer,0,buffer.length,null);if(!count)break;
        bytes+=count;assert(bytes<=256*1024*1024);hash.update(buffer.subarray(0,count));}
      assert.equal(bytes,stat.size);return hash.digest('hex');
    }finally{fs.closeSync(fd);}
  };
  const object=(value,kind)=>{
    assert(value&&value.kind===kind&&value.algorithm==='sha256');assert.match(value.digest,/^[0-9a-f]{64}$/);
    assert(Number.isSafeInteger(value.size_bytes)&&value.size_bytes>=0);
  };
  const uuid=value=>assert.match(value,/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  const integer=(value,min=0)=>assert(Number.isSafeInteger(value)&&value>=min);
  const validateLocal=events=>{
    const names=['static','node','teardown','interrupt','crash'];assert.equal(events.length,6);
    assert.deepEqual(events.slice(0,5).map(value=>value.execution_case_passed),names);
    const final=events[5];
    for(const field of ['actual_terminal_pane','read_only_runtime','namespace_proc_fd_denial','selected_home_env_only',
      'no_source_cas_credentials','valid_mcp_and_forged_token_denial','static_shell_node_cc'])assert.equal(final[field],true);
    assert.equal(final.profile_uid,1500);assert.equal(final.network_isolated,false);assert.equal(final.cgroup_limited,false);
    assert.deepEqual(final.records.map(value=>value.mode),names);
    for(const record of final.records){integer(record.witnessed_processes_exited);
      if(record.mode==='crash'){
        assert.equal(typeof record.phase,'string');assert.notEqual(record.phase,'checkpointed');assert.equal(record.no_forged_checkpoint,true);
        integer(record.witnessed_processes_exited,3);assert(record.checkpoint==null);
      }else{
        assert.equal(record.phase,'checkpointed');assert.equal(record.agent_exit,record.mode==='interrupt'?130:0);
        assert.equal(record.checkpoint.format_version,1);object(record.checkpoint.directory,'directory');object(record.checkpoint.hardlinks,'blob');
        if(record.mode==='teardown')integer(record.witnessed_processes_exited,3);
      }
    }
    return{casesPassed:5,specVersion:1,summary:final};
  };
  const validateRpc=events=>{
    assert.equal(events.length,1);const final=events[0];assert.equal(final.kind,'workspace-observation-rpc');assert.equal(final.format_version,1);
    assert.equal(final.uid,1500);assert.equal(final.gid,1500);assert.equal(final.parent_mount_namespace_unchanged,true);
    assert.equal(final.journal_mounted,false);assert.equal(final.cas_mounted,false);assert.equal(final.default_profile_changed,false);
    assert.equal(final.adoption_witness,'unsupported; no peer fsync assertion accepted');
    assert.deepEqual(final.records.map(value=>value.mode),['live','revoke','epoch','wrong-scope']);
    for(const record of final.records){uuid(record.execution_id);uuid(record.view_id);uuid(record.namespace);
      if(record.mode==='live'){
        assert.equal(record.phase,'checkpointed');assert.equal(record.kernel_namespace_drained_before_checkpoint,true);assert.equal(record.rpc_settled_before_unmount,true);
        const proof=record.guest_proof;assert.equal(proof.namespace,record.namespace);object(proof.profile,'blob');
        assert.equal(proof.open_fd_during_observe,true);assert.equal(proof.readonly_projection,true);assert.equal(proof.unsupported,'unsupported');
        for(const field of ['foreign','seal'])assert(typeof proof[field]==='string'&&proof[field].length>0&&proof[field].length<=128);
        assert(proof.git==null,'optional Git qualification must not be implicitly enabled');
        for(const observation of [proof.first,proof.second]){
          assert.equal(observation.format_version,1);assert.equal(observation.consistency,'live_best_effort');assert.equal(observation.view_id,record.view_id);
          assert.deepEqual(observation.owner,{owner:'rpc-owner',epoch:1});object(observation.directory,'directory');uuid(observation.request_nonce);
          assert.match(observation.exclusion_policy_digest,/^[0-9a-f]{64}$/);
          assert.equal(observation.pin_id,'vfs:observe:'+record.view_id+':'+observation.request_nonce);
          for(const cursor of [observation.begin,observation.end]){
            assert.equal(cursor.namespace,record.namespace);assert.equal(cursor.view_id,record.view_id);assert.deepEqual(cursor.owner,observation.owner);integer(cursor.generation);
          }
          assert(observation.end.generation>=observation.begin.generation);
        }
        assert(proof.second.end.generation>proof.first.end.generation);assert.notEqual(proof.first.request_nonce,proof.second.request_nonce);
        assert.equal(proof.first.exclusion_policy_digest,proof.second.exclusion_policy_digest);
      }else if(record.mode==='wrong-scope'){
        assert(record.phase==null||typeof record.phase==='string');assert.notEqual(record.phase,'checkpointed');assert.equal(record.denied_before_guest_effects,true);
      }else{
        assert.equal(record.phase,'orphaned');assert.equal(record.stale_rpc_denied,true);assert.equal(record.no_checkpoint_on_authority_loss,true);
        integer(record.witnessed_processes_exited,3);integer(record.revocation_ms);assert(record.revocation_ms<6000);
      }
    }
    return{casesPassed:4,specVersion:2,summary:final};
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
    const provenancePath='/opt/rudder/qualification/execution-provenance.json';
    const manifestFd=fs.openSync(provenancePath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);
    let manifestBytes;
    try{const stat=fs.fstatSync(manifestFd);assert(stat.isFile()&&stat.nlink===1&&stat.uid===0&&stat.size<=4096);assert.equal(stat.mode&0o777,0o444);
      manifestBytes=fs.readFileSync(manifestFd);assert.deepEqual(JSON.parse(manifestBytes),input.provenance);
    }finally{fs.closeSync(manifestFd);}
    result.provenance=input.provenance;result.provenanceSha256=crypto.createHash('sha256').update(manifestBytes).digest('hex');
    const binary='/opt/rudder/qualification/rudder-execution-qualifier';assert.equal(digestFile(binary),input.provenance.qualifierSha256);
    result.qualifierSha256=input.provenance.qualifierSha256;
    stage='runtime-prerequisites';
    const native='/opt/rudder/dist/native/rudder-native';
    result.prerequisites=[];
    for(const [label,file] of [['busybox','/usr/bin/busybox'],['node','/usr/bin/node'],['gcc','/usr/bin/gcc'],['shell','/bin/sh'],['sleep','/usr/bin/sleep'],['stty','/usr/bin/stty'],['native',native]]){
      stage='runtime-prerequisite-'+label;const stat=fs.statSync(file);assert(stat.isFile()&&stat.uid===0);assert.equal(stat.mode&0o022,0);
      fs.accessSync(file,fs.constants.X_OK);result.prerequisites.push({label,path:file,resolved:fs.realpathSync(file),executable:true});
    }
    stage='runtime-node-alias';assert.equal(fs.realpathSync('/usr/bin/node'),fs.realpathSync('/usr/local/bin/node'));
    stage='runtime-owned-workspace';
    assert.equal(fs.statSync('/workspace').uid,1500);
    stage='runtime-fuse-uid1500';
    const fuse=fs.lstatSync('/dev/fuse');assert(fuse.isCharacterDevice());assert.equal(fuse.uid,1500);assert.equal(fuse.gid,1500);assert.equal(fuse.mode&0o777,0o600);
    const fuseFd=fs.openSync('/dev/fuse','r+');fs.closeSync(fuseFd);result.fuseOpenedAsUid1500=true;
    stage='fresh-short-fixture-roots';
    for(const root of [input.executionRoot,input.rpcRoot]){assert.match(root,/^\/workspace\/(ex|rp)-[0-9a-f]{8}$/);assert(!fs.existsSync(root));}
    assert.notEqual(input.executionRoot,input.rpcRoot);
    parentUser=witness('user');parentNet=witness('net');parentMnt=witness('mnt');
    result.parentBefore={user:identity(parentUser),net:identity(parentNet),mnt:identity(parentMnt)};
    const childEnv={PATH:'/usr/local/bin:/usr/bin:/bin',HOME:'/workspace',TMPDIR:'/workspace',LANG:'C',LC_ALL:'C',RUDDER_EXECUTION_GIT_QUALIFY:'0'};
    assert(!Object.hasOwn(childEnv,'NODE_OPTIONS'));
    const modes=[{name:'qualify',root:input.executionRoot,timeout:420000},{name:'rpc-qualify',root:input.rpcRoot,timeout:240000}];
    for(const mode of modes){
      stage=mode.name;
      const output=cp.spawnSync(binary,[mode.name,native,mode.root],{cwd:'/workspace',env:childEnv,
        stdio:['ignore','pipe','pipe'],encoding:'utf8',timeout:mode.timeout,killSignal:'SIGKILL',maxBuffer:256*1024});
      const record={mode:mode.name,specVersion:mode.name==='qualify'?1:2,fixtureRoot:mode.root,outcome:'failed',
        exitCode:output.status,signal:output.signal,stdout:output.stdout||'',stderr:output.stderr||''};
      result.modes.push(record);
      assert.deepEqual({user:current('user'),net:current('net'),mnt:current('mnt')},result.parentBefore);
      assert.deepEqual(status(),initial);record.parentNamespacesAndStatusUnchanged=true;
      assert(Buffer.byteLength(record.stdout)<=256*1024&&Buffer.byteLength(record.stderr)<=64*1024);
      assert(!output.error);assert.equal(output.signal,null);assert.equal(output.status,0);
      const lines=record.stdout.trim().split('\n').filter(Boolean);assert(lines.length>0&&lines.length<=8);
      const events=lines.map(line=>JSON.parse(line));record.events=events;
      const validated=mode.name==='qualify'?validateLocal(events):validateRpc(events);
      record.casesPassed=validated.casesPassed;record.summary=validated.summary;record.outcome='qualified';
    }
    result.parentAfter={user:current('user'),net:current('net'),mnt:current('mnt')};assert.deepEqual(result.parentAfter,result.parentBefore);
    assert.deepEqual(status(),initial);result.parentNamespacesAndStatusUnchanged=true;result.completed=true;
    result.qualified=result.modes.length===2&&result.modes.every(mode=>mode.outcome==='qualified');
  }catch(error){result.completed=false;result.qualified=false;result.failedStage=stage;
    result.errorCode=typeof error.code==='string'?error.code:'ASSERTION';}
  finally{
    if(parentUser)fs.closeSync(parentUser.fd);if(parentNet)fs.closeSync(parentNet.fd);if(parentMnt)fs.closeSync(parentMnt.fd);
    result.counts={qualified:result.modes.filter(mode=>mode.outcome==='qualified').length,
      unsupported:0,failed:result.modes.filter(mode=>mode.outcome==='failed').length,
      localExecutionCasesPassed:result.modes.find(mode=>mode.mode==='qualify'&&mode.outcome==='qualified')?.casesPassed??0,
      workspaceRpcCasesPassed:result.modes.find(mode=>mode.mode==='rpc-qualify'&&mode.outcome==='qualified')?.casesPassed??0};
    result.verifiedAt=new Date().toISOString();
    fs.writeFileSync(input.proofPath,JSON.stringify(result,null,2)+'\n',{mode:0o600,flag:'wx'});
  }
  // Never return to evaluate home/supervisor, including unsupported/failure.
  setTimeout(()=>process.exit(0),480000);await new Promise(()=>{});
}

const gatePath='/workspace/execution-gate-'+nonce+'.json',proofPath='/workspace/execution-proof-'+nonce+'.json';
const input={nonce,version:values.version,revision:values.revision,digest:values.digest,provenance,gatePath,proofPath,
  executionRoot:'/workspace/ex-'+nonce.slice(0,8),rpcRoot:'/workspace/rp-'+nonce.slice(0,8)};
const preloadSource='await ('+preload.toString()+')('+JSON.stringify(input)+');';
await fs.writeFile(path.join(directory,'preload.mjs'),preloadSource+'\n',{mode:0o600});
const config={image:values.image,guest:{cpu_kind:'performance',cpus:1,memory_mb:2048},restart:{policy:'no'},auto_destroy:false,
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
  for(const field of ['cpu_kind','cpus','memory_mb'])assert.equal(machine.config.guest?.[field],config.guest[field]);
  const allowed=[...Object.keys(config.env),'PRIMARY_REGION','FLY_PRIMARY_REGION'];assert(Object.keys(machine.config.env||{}).every(key=>allowed.includes(key)));
  for(const[key,value]of Object.entries(config.env))assert.equal(machine.config.env[key],value);
  report.machine={id:owned,name,nonce,role,instance:machine.instance_id,region:machine.region,digest:machine.image_ref.digest};
  assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'recheck-empty-app-secrets')),[]);
  const gate=JSON.stringify({nonce,digest:values.digest,revision:values.revision});
  const writeGate=`const fs=require('node:fs');const target=${JSON.stringify(gatePath)},temporary=target+'.pending';const fd=fs.openSync(temporary,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);fs.writeFileSync(fd,${JSON.stringify(gate)});fs.fchownSync(fd,1500,1500);fs.fsyncSync(fd);fs.closeSync(fd);if(fs.existsSync(target))throw Error('admission gate already exists');fs.renameSync(temporary,target);const directory=fs.openSync('/workspace',fs.constants.O_RDONLY|fs.constants.O_DIRECTORY);fs.fsyncSync(directory);fs.closeSync(directory);`;
  await ownedExec('/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e '+quote(writeGate),'release-after-verified-derived-digest');
  emit('owned-image-verified-and-gate-released');
  let proof;
  const proofDeadline=Date.now()+780000;
  while(Date.now()<proofDeadline){
    try{const output=await ownedExec('/bin/cat '+quote(proofPath),'read-bounded-generated-proof');assert(Buffer.byteLength(output)<=1024*1024);proof=JSON.parse(output.trim());break;}
    catch{await delay(2000);}}
  assert(proof&&proof.nonce===nonce&&proof.version===values.version&&proof.revision===values.revision);
  await fs.writeFile(path.join(directory,'guest-proof.json'),JSON.stringify(proof,null,2)+'\n',{mode:0o600});
  report.proofSha256=sha(await fs.readFile(path.join(directory,'guest-proof.json')));
  report.completed=proof.completed===true;report.qualified=proof.qualified===true;report.counts=proof.counts;
  assert.equal(proof.completed,true);assert.equal(proof.admittedAfterResolvedDigest,true);
  assert.equal(proof.parentNamespacesAndStatusUnchanged,true);assert.equal(proof.egressQualified,false);
  assert.equal(proof.liveAttemptAuthorized,false);assert.equal(proof.supervisorModuleEvaluated,false);
  assert.deepEqual(proof.specVersions,[1,2]);assert.equal(proof.networkQualificationSeparate,true);assert.equal(proof.networkIsolated,false);
  assert.equal(proof.allFiveGuestCapabilitiesQualified,false);assert.equal(proof.remoteExecutionQualified,false);
  assert.equal(proof.fuseOpenedAsUid1500,true);assert.deepEqual(proof.counts,{qualified:2,unsupported:0,failed:0,localExecutionCasesPassed:5,workspaceRpcCasesPassed:4});
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
