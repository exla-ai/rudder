// PRIVATE. Import/default performs no provider call or filesystem mutation.
import fs from 'node:fs/promises';
import path from 'node:path';
import {parseArgs} from 'node:util';
import {fileURLToPath} from 'node:url';
import {plan,ownership,requireValue as need,requirePhysicalInputs,APP,ROLE,IMAGE,REVISION,VERSION,ROOT_FLAGS,FORBIDDEN_ID,FORBIDDEN_INSTANCE,sha} from './ownership.mjs';
import {guestRoot,guestSmoke} from './guest.mjs';
import {command} from './transport.mjs';
const HERE=path.dirname(fileURLToPath(import.meta.url));
const quote=text=>"'"+text.replaceAll("'","'\\''")+"'";
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const nodeCommand=source=>'/usr/bin/env -i '+Object.entries(ROOT_FLAGS).map(([k,v])=>k+'='+v).join(' ')+' /usr/local/bin/node -e '+quote(source);
const parse=text=>{try{const value=JSON.parse(text.trim());need(value&&typeof value==='object','INVALID_JSON_OBJECT');return value;}catch{throw Object.assign(new Error('INVALID_JSON'),{code:'INVALID_JSON'});}};
const encode=bytes=>({bytes:bytes.length,sha256:sha(bytes),base64:bytes.toString('base64')});
export function decodeLog(value, cap) {
  need(value&&typeof value.base64==='string'&&value.base64.length<=Math.ceil(cap/3)*4,'LOG_ENVELOPE_BOUND');
  const bytes=Buffer.from(value.base64,'base64');
  need(bytes.length<=cap&&bytes.toString('base64')===value.base64&&bytes.length===value.bytes&&sha(bytes)===value.sha256,'LOG_ENVELOPE_MISMATCH');
  return bytes;
}
export function validateProof(proof,p) {
  need(proof?.formatVersion===1&&proof.kind===ROLE,'GUEST_SMOKE_FAILED');
  for(const [key,value]of Object.entries(p.input))need(proof[key]===value,'PROOF_BINDING_CHANGED');
  need(proof.stockImage===true&&proof.admittedAfterResolvedDigest===true&&proof.modelCalls===false&&proof.supervisorLoaded===false,'PROOF_SCOPE');
  need(proof.packageVersion===VERSION&&/^v22\./.test(proof.node?.version||'')&&proof.node?.resolvedPath==='/usr/local/bin/node','ROOT_RUNTIME_IDENTITY');
  const smoke=proof.smoke;
  need(smoke?.formatVersion===1&&smoke.kind==='stock23-uid1500-smoke'&&smoke.status==='passed'&&smoke.nonce===p.input.nonce&&smoke.packageVersion===VERSION&&smoke.modelCalls===false&&smoke.supervisorLoaded===false&&smoke.environmentCleared===true,'UID1500_PROOF');
  need(smoke.identity?.uid===1500&&smoke.identity.gid===1500&&Array.isArray(smoke.identity.groups)&&smoke.identity.groups.length===0&&smoke.identity.noNewPrivs===1,'UID1500_IDENTITY');
  for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])need(/^0+$/.test(smoke.identity.capabilities?.[key]||''),'UID1500_CAPABILITIES');
  const paths={node:'/usr/local/bin/node',native:'/opt/rudder/dist/native/rudder-native',buddy:'/opt/rudder/dist/native/rudder-jj',cli:'/opt/rudder/dist/index.js',package:'/opt/rudder/package.json'};
  need(JSON.stringify(Object.keys(proof.artifacts||{}).sort())===JSON.stringify(Object.keys(paths).sort()),'ARTIFACT_SET');
  for(const [key,file]of Object.entries(paths)){const a=proof.artifacts[key],b=smoke.artifacts?.[key];
    need(a?.path===file&&/^[a-f0-9]{64}$/.test(a.sha256||'')&&Number.isSafeInteger(a.bytes)&&a.bytes>0&&a.bytes<=536870912,'ARTIFACT_IDENTITY');
    need(JSON.stringify(a)===JSON.stringify(b),'ARTIFACT_CHANGED_ACROSS_UID_DROP');}
  need(smoke.node?.sha256===proof.artifacts.node.sha256&&proof.node.sha256===proof.artifacts.node.sha256&&smoke.node.resolvedPath===proof.node.resolvedPath&&smoke.node.version===proof.node.version,'NODE_CHANGED');
  const child=proof.child;
  need(child?.exitObserved===true&&child.settled===true&&child.code===0&&child.signal===null&&child.failure===null,'UID1500_CHILD_NOT_SETTLED');
  const childBytes=Buffer.from(JSON.stringify(smoke)+'\n');
  need(childBytes.length===child.stdoutBytes&&sha(childBytes)===child.stdoutSha256,'UID1500_OUTPUT_RECEIPT');
  need(Number.isSafeInteger(child.stderrBytes)&&child.stderrBytes>=0&&child.stderrBytes<=8192&&/^[a-f0-9]{64}$/.test(child.stderrSha256||''),'UID1500_STDERR_RECEIPT');
  const jobs=[['cli-version',['/usr/local/bin/node','/opt/rudder/dist/index.js','--version']],['native-smoke',['/opt/rudder/dist/native/rudder-native','--smoke']],
    ['native-execution',['/opt/rudder/dist/native/rudder-native','fabric','execution','capabilities']],['buddy-version',['/opt/rudder/dist/native/rudder-jj','--version']],
    ['buddy-capabilities',['/opt/rudder/dist/native/rudder-jj','rudder-capabilities']],['git-version',['/usr/local/bin/git','--version']],['jj-version',['/usr/local/bin/jj','--version']]];
  need(Array.isArray(smoke.commands)&&smoke.commands.length===jobs.length,'COMMAND_SET');
  return smoke.commands.map((result,index)=>{
    const [label,argv]=jobs[index];need(result.label===label&&JSON.stringify(result.argv)===JSON.stringify(argv),'COMMAND_ARGV');
    need(result.child?.exitObserved===true&&result.child.settled===true&&result.child.code===0&&result.child.signal===null&&result.child.failure===null,'COMMAND_NOT_SETTLED');
    const stdout=decodeLog(result.stdout,16384),stderr=decodeLog(result.stderr,8192),text=stdout.toString('utf8').trim();
    if(label==='cli-version')need(text===VERSION,'CLI_VERSION');
    else if(label==='native-smoke')need(text==='rudder-native smoke ok','NATIVE_SMOKE');
    else if(label==='native-execution'){const c=parse(text);need(c.format_version===1&&c.platform==='linux'&&c.network_isolated===false,'EXECUTION_CAPABILITIES');}
    else if(label==='buddy-version')need(text.startsWith('rudder-jj 0.40.0-rudder-bridge-v1'),'BUDDY_VERSION');
    else if(label==='buddy-capabilities'){const c=parse(text);need(c.format_version===1&&c.jj_version==='0.40.0'&&c.vfs_default===false&&c.local_development===false&&c.automatic_jj_replacement===false,'BUDDY_CAPABILITIES');}
    else if(label==='git-version')need(/^git version 2\.56\.0(?:\s|$)/.test(text),'GIT_VERSION');
    else if(label==='jj-version')need(/^jj 0\.40\.0(?:-[0-9a-f]{40})?$/.test(text),'JJ_VERSION');
    return {label,stdout,stderr};
  });
}
export function validateReadOnlyProof(proof,p) {
  need(proof?.formatVersion===1&&proof.kind===ROLE,'RO_PROOF_BINDING');
  for(const [key,value]of Object.entries(p.input))need(proof[key]===value,'RO_PROOF_BINDING');
  need(proof.stockImage===true&&proof.admittedAfterResolvedDigest===true&&proof.modelCalls===false&&proof.supervisorLoaded===false,'RO_PROOF_BINDING');
  need(proof?.scope==='root-storage-capability-only'&&proof.productionSeedProducer===false
    &&proof.managedBackend===false&&proof.preparationReuse===false,'RO_SCOPE');
  const r=proof.storageRoot,s=proof.smoke,ro=s?.readOnly;
  need(r?.formatVersion===1&&r.scope==='root-storage-capability-only'&&r.nonce===p.input.nonce&&r.status==='passed','RO_ROOT_PROOF');
  const mountPoint='/workspace/seed-ro-'+p.input.nonce+'/objects',source='rudder-seed-ro-'+p.input.nonce;
  need(r.mountPoint===mountPoint&&r.maxBytes===16777216&&r.attempted===true&&r.initiallyAbsent===true,'RO_SCRATCH_IDENTITY');
  need(r.identity?.uid===0&&r.identity.gid===0&&r.identity.uids==='0\t0\t0\t0'&&r.identity.gids==='0\t0\t0\t0','ROOT_IDENTITY');
  for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])need(/^[a-f0-9]{16}$/.test(r.identity.capabilities?.[key]||''),'ROOT_CAPABILITIES');
  const cap=(1n<<21n);need(r.capSysAdminBit===21&&r.capSysAdminEffective===true
    &&(BigInt('0x'+r.identity.capabilities.CapEff)&cap)!==0n
    &&(BigInt('0x'+r.identity.capabilities.CapPrm)&cap)!==0n,'ROOT_CAP_SYS_ADMIN');
  need(/^mnt:\[[0-9]+\]$/.test(r.mountNamespace||'')&&ro?.mountNamespace===r.mountNamespace,'RO_SHARED_MOUNT_NAMESPACE');
  const mount=(value,readonly)=>{
    need(value?.mountPoint===mountPoint&&value.filesystem==='tmpfs'&&value.source===source
      &&Number.isSafeInteger(value.mountId)&&value.mountId>0&&Array.isArray(value.options)&&value.options.length<=64
      &&Array.isArray(value.superOptions)&&value.superOptions.length<=64,'RO_MOUNT_RECORD');
    for(const flag of [readonly?'ro':'rw','nosuid','nodev','noexec'])need(value.options.includes(flag),'RO_MOUNT_FLAGS');
    need(value.superOptions.includes(readonly?'ro':'rw'),'RO_SUPERBLOCK_FLAGS');
  };
  mount(r.beforeReadonly,false);mount(r.readonly,true);mount(ro,true);
  need(r.beforeReadonly.mountId===r.readonly.mountId&&ro.mountId===r.readonly.mountId,'RO_MOUNT_CHANGED');
  need(Number.isSafeInteger(r.capacityBytes)&&r.capacityBytes>0&&r.capacityBytes<=16777216&&ro.capacityBytes===r.capacityBytes,'RO_CAPACITY');
  const bytes=Buffer.from('rudder-seed-ro-fixture-v1\n'+p.input.nonce+'\n');
  for(const fixture of [r.fixture,ro.fixture])need(fixture?.file===mountPoint+'/fixture'&&fixture.uid===1500&&fixture.gid===1500
    &&fixture.mode===0o600&&fixture.bytes===bytes.length&&fixture.sha256===sha(bytes),'RO_FIXTURE_BINDING');
  need(r.fixture.createdBeforeReadonly===true&&r.rootWrite?.uid===0&&r.rootWrite.attempted===true
    &&r.rootWrite.error==='EROFS'&&r.rootWrite.unchangedAfterWrite===true,'ROOT_WRITE_NOT_EROFS');
  need(ro.formatVersion===1&&ro.scope==='root-storage-capability-only'&&ro.readPassed===true
    &&ro.writeAttempted===true&&ro.writeError==='EROFS'&&ro.unchangedAfterWrite===true,'UID1500_RO_READ_WRITE');
  need(s.identity?.uid===1500&&s.identity.gid===1500&&s.identity.noNewPrivs===1
    &&Array.isArray(s.identity.groups)&&s.identity.groups.length===0,'RO_UID1500_IDENTITY');
  for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])need(/^0+$/.test(s.identity.capabilities?.[key]||''),'RO_UID1500_CAPABILITIES');
  need(proof.child?.exitObserved===true&&proof.child.settled===true&&proof.child.signal===null&&proof.child.failure===null,'RO_CHILD_NOT_SETTLED');
  const childBytes=Buffer.from(JSON.stringify(s)+'\n');need(childBytes.length===proof.child.stdoutBytes&&sha(childBytes)===proof.child.stdoutSha256,'RO_CHILD_RECEIPT');
  const expected=[['mount-tmpfs',['/usr/bin/mount','-t','tmpfs','-o','size=16777216,nosuid,nodev,noexec,mode=0700,uid=1500,gid=1500',source,mountPoint]],
    ['remount-ro',['/usr/bin/mount','-o','remount,ro,nosuid,nodev,noexec,size=16777216','--',mountPoint]],['umount',['/usr/bin/umount','--',mountPoint]]];
  need(Array.isArray(r.actions)&&r.actions.length===expected.length,'RO_ACTION_SET');
  for(let i=0;i<expected.length;i++){const action=r.actions[i],[label,argv]=expected[i];
    need(action.label===label&&JSON.stringify(action.argv)===JSON.stringify(argv),'RO_ACTION_ARGV');
    need(action.child?.exitObserved===true&&action.child.settled===true&&action.child.code===0&&action.child.signal===null&&action.child.failure===null,'RO_ACTION_NOT_SETTLED');
    for(const stream of ['stdout','stderr'])need(Number.isSafeInteger(action.child[stream+'Bytes'])&&action.child[stream+'Bytes']>=0&&action.child[stream+'Bytes']<=8192&&/^[a-f0-9]{64}$/.test(action.child[stream+'Sha256']||''),'RO_ACTION_OUTPUT_BOUND');
  }
  for(const key of ['mount','umount']){const tool=r.tools?.[key];need(tool?.path==='/usr/bin/'+key&&Number.isSafeInteger(tool.bytes)&&tool.bytes>0&&tool.bytes<=16777216&&/^[a-f0-9]{64}$/.test(tool.sha256||''),'RO_TOOL_IDENTITY');}
  const c=r.cleanup;need(c?.childAbsentOrSettled===true&&c.rootCommandsSettled===true&&c.presentBefore===true&&c.attempted===true
    &&c.verifiedAbsent===true&&c.mountNamespace===r.mountNamespace&&!c.failure,'RO_NORMAL_UNMOUNT_NOT_PROVEN');
  return {scope:'root-storage-capability-only',mountNamespace:r.mountNamespace,capSysAdminEffective:true,maxBytes:r.maxBytes,
    capacityBytes:r.capacityBytes,rootWriteEROFS:true,uid1500WriteEROFS:true,normalUnmountVerified:true};
}
export async function main(argv,{transport=command,pause=delay,now=Date.now}={}) {
  const {values:v}=parseArgs({args:argv,strict:true,options:{run:{type:'boolean'},image:{type:'string'},revision:{type:'string'},digest:{type:'string'},nonce:{type:'string'}}});
  if(!v.run)return {status:'prepared-no-run',providerCalls:0};
  // Refuse real dispatch before run files/provider calls unless all exact
  // source/image pins match Root's stock23 candidate2 receipt.
  if(transport===command)requirePhysicalInputs(v);
  const started=now(),p=plan(v,started,guestRoot,guestSmoke),o=ownership(p),mocked=transport!==command||pause!==delay||now!==Date.now;
  const monotonicStarted=performance.now();
  const directory=path.join(HERE,'run-seed-ro-'+p.input.nonce);await fs.mkdir(directory,{mode:0o700});
  const save=(name,bytes)=>fs.writeFile(path.join(directory,name),bytes,{mode:0o600,flag:'wx'});
  const report={formatVersion:1,kind:ROLE,...p.input,requestedImage:IMAGE,testOnlyMock:mocked,providerCalls:0,
    scope:'root-storage-capability-only',productionSeedProducer:false,managedBackend:false,preparationReuse:false,
    smokePassed:false,rootStoragePassed:false,qualified:false,cleanupVerified:false,independentAbsence:false,ownedDestruction:false,steps:[],maxLifetimeSeconds:360};
  const mainDeadline=started+220000,cleanupDeadline=started+350000;
  let serial=0,cleanup=false,creationAttempted=false,secretChecks=0;
  const call=async(args,label,maximum=15000,limit=1048576)=>{
    need(!args.some(value=>value===FORBIDDEN_ID||value===FORBIDDEN_INSTANCE),'PROTECTED_HOME_TARGET');
    const deadline=cleanup?cleanupDeadline:mainDeadline;
    const timeout=Math.min(maximum,deadline-now()-2000,
      deadline-started-(performance.now()-monotonicStarted)-2000);need(timeout>0,'CONTROLLER_DEADLINE');
    if(!mocked)report.providerCalls++;
    const response=await transport(args,{timeout,stdoutLimit:limit,label});
    const step={label,...response.diagnostics};report.steps.push(step);
    const prefix=String(++serial).padStart(3,'0')+'-'+label;
    // Raw inventory/secret/provider errors can contain tokens or existing-home
    // environment. Persist receipts only; controlled env-i guest output is safe.
    const guestOutput=['admission','proof','recover-private-logs'].includes(label);
    await save(prefix+'.stdout.private',guestOutput?response.stdout:JSON.stringify({suppressedProviderOutput:true,bytes:step.stdoutBytes??null,sha256:step.stdoutSha256??null}));
    await save(prefix+'.stderr.private',JSON.stringify({suppressedProviderOutput:true,bytes:step.stderrBytes??null,sha256:step.stderrSha256??null}));
    need(response.ok&&step.settled===true&&step.status===0&&!step.signal&&!step.failure,'PROVIDER_COMMAND_FAILED');
    return response.stdout;
  };
  const json=async(...args)=>parse(await call(...args));
  const inventory=async()=>{const value=await json(['machine','list','-a',APP,'--json'],'inventory',15000,8388608);need(Array.isArray(value)&&value.length<=10000,'INVENTORY_BOUND');return value;};
  const noSecrets=async()=>{need(JSON.stringify(await json(['secrets','list','-a',APP,'--json'],'empty-app-secrets'))==='[]','APP_SECRETS_NOT_EMPTY');secretChecks++;};
  const collision=machine=>machine.id===o.pinned()?.id||machine.name===p.name||machine.config?.metadata?.qualification===p.input.nonce;
  const ownedExec=async(source,label)=>{
    const pin=o.pinned();need(pin&&pin.id!==FORBIDDEN_ID&&pin.instance!==FORBIDDEN_INSTANCE,'OWNED_PIN_REQUIRED');
    const found=(await inventory()).filter(machine=>machine.id===pin.id);need(found.length===1&&o.matches(found[0]),'OWNED_INSTANCE_CHANGED');
    const text=await call(['machine','exec',pin.id,'-a',APP,nodeCommand(source),'--timeout','15'],label,20000);
    for(const match of text.matchAll(/^Exit code:\s*(-?\d+)\s*$/gm))need(Number(match[1])===0,'REMOTE_EXEC_STATUS');
    return parse(text);
  };
  const readSource=(recovery=false)=>`const fs=require('node:fs'),crypto=require('node:crypto');const root=${JSON.stringify(p.root)},nonce=${JSON.stringify(p.input.nonce)};const fail=()=>{throw Error('PRIVATE_FILE_REJECTED');};const d=fs.lstatSync(root);if(!d.isDirectory()||d.isSymbolicLink()||d.uid!==0||d.gid!==0||(d.mode&4095)!==448)fail();const read=name=>{const f=fs.openSync(root+'/'+name,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);try{const s=fs.fstatSync(f);if(!s.isFile()||s.uid!==0||s.gid!==0||s.nlink!==1||(s.mode&4095)!==384||s.size>1048576)fail();const b=fs.readFileSync(f),t=fs.lstatSync(root+'/'+name);if(b.length!==s.size||t.dev!==s.dev||t.ino!==s.ino||t.size!==s.size||t.nlink!==1)fail();return b;}finally{fs.closeSync(f);}};${recovery?`const logs={};for(const name of ['child.stdout.private','child.stderr.private'])try{const b=read(name);logs[name]={bytes:b.length,sha256:crypto.createHash('sha256').update(b).digest('hex'),base64:b.toString('base64')};}catch(e){if(e.code!=='ENOENT')throw e;}console.log(JSON.stringify({nonce,logs}));`:`try{console.log(read('proof.json').toString('utf8'));}catch(e){if(e.code!=='ENOENT')throw e;console.log(JSON.stringify({status:'pending',nonce}));}`}`;
  try{
    need(/^flyctl v0\.4\.118(?:\s|$)/.test(await call(['version'],'fly-version',15000,8192)),'FLYCTL_VERSION');
    await noSecrets();need(!(await inventory()).some(collision),'PREEXISTING_NONCE_COLLISION');
    await save('machine.json',JSON.stringify(p.config));creationAttempted=true;
    // flyctl 0.4.118 has no skip-secrets flag. Empty app secrets are checked
    // before create and again immediately before digest-verified admission.
    try{await call(['machine','run',p.config.image,'-a',APP,'--machine-config',path.join(directory,'machine.json'),'--name',p.name,'--region','sjc','--detach','--skip-dns-registration'],'create',60000);}catch{report.createReplyUncertain=true;}
    const startDeadline=Math.min(mainDeadline,now()+65000);let machine,startRequested=false;
    while(now()<startDeadline){const all=await inventory(),found=all.filter(o.matches);need(found.length<=1,'MULTIPLE_OWNED_MACHINES');
      need(!all.filter(collision).some(candidate=>!o.matches(candidate)),'FOREIGN_NONCE_COLLISION');
      if(found.length){machine=found[0];report.machine=o.remember(machine);if(machine.state==='started')break;
        if(machine.state==='created'&&!startRequested){startRequested=true;try{await call(['machine','start',o.pinned().id,'-a',APP],'start-exact-owned',20000);}catch{report.startReplyUncertain=true;}}}
      await pause(500);
    }
    need(machine?.state==='started','OWNED_MACHINE_NOT_STARTED');
    await noSecrets();need(secretChecks===2,'TWO_SECRET_CHECKS_REQUIRED');
    const source=`const fs=require('node:fs');const root=${JSON.stringify(p.root)},input=${JSON.stringify(p.input)};if(process.getuid()!==0)throw Error('ROOT_REQUIRED');const m=fs.lstatSync(root);if(!m.isDirectory()||m.isSymbolicLink()||m.uid!==0||m.gid!==0||(m.mode&4095)!==448)throw Error('PRIVATE_ROOT');const f=fs.openSync(root+'/admission.json',fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,384);try{fs.writeFileSync(f,JSON.stringify(input));fs.fsyncSync(f);}finally{fs.closeSync(f);}console.log(JSON.stringify({status:'admitted',nonce:input.nonce,imageDigest:input.imageDigest}));`;
    const admitted=await ownedExec(source,'admission');need(admitted.status==='admitted'&&admitted.nonce===p.input.nonce&&admitted.imageDigest===p.input.imageDigest,'ADMISSION_REJECTED');
    let proof;while(now()<mainDeadline){proof=await ownedExec(readSource(),'proof');if(proof.status!=='pending')break;await pause(1000);}
    need(proof&&proof.status!=='pending','SMOKE_PROOF_DEADLINE');
    await save('guest-proof.json',JSON.stringify(proof,null,2)+'\n');
    let logs=[],smokeError,storageError;
    try{logs=validateProof(proof,p);report.smokePassed=true;}catch(error){smokeError=error;}
    try{report.rootStorage=validateReadOnlyProof(proof,p);report.rootStoragePassed=true;}catch(error){storageError=error;}
    for(const item of logs)for(const stream of ['stdout','stderr'])await save(item.label+'.'+stream+'.private',item[stream]);
    report.artifacts=proof.artifacts;report.node=proof.node;report.packageVersion=proof.packageVersion;
    if(smokeError)throw smokeError;if(storageError)throw storageError;
  }catch(error){report.failure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'SMOKE_REJECTED';
    if(o.pinned()&&now()<mainDeadline-5000)try{
      const recovered=await ownedExec(readSource(true),'recover-private-logs');need(recovered.nonce===p.input.nonce,'RECOVERY_NONCE');
      for(const [name,envelope]of Object.entries(recovered.logs||{})){need(['child.stdout.private','child.stderr.private'].includes(name),'RECOVERY_PATH');await save('recovered-'+name,decodeLog(envelope,1048576));}
      report.privateLogsRecovered=true;
    }catch{report.privateLogsRecovered=false;}
  }finally{
    cleanup=true;report.secretChecks=secretChecks;
    try{
      const all=await inventory(),found=all.filter(o.matches);need(found.length<=1,'MULTIPLE_CLEANUP_MATCHES');
      need(!all.filter(collision).some(machine=>!o.matches(machine)),'FOREIGN_INSTANCE_REQUIRES_RECONCILIATION');
      if(found.length){if(!o.pinned())report.machine=o.remember(found[0]);const pin=o.pinned();
        // Re-read immediately before mutation. A matching ID with a replaced
        // instance/config/image never authorizes a destroy.
        const fresh=(await inventory()).filter(machine=>machine.id===pin.id);need(fresh.length===1&&o.matches(fresh[0]),'DESTROY_OWNERSHIP_CHANGED');
        try{await call(['machine','destroy',pin.id,'-a',APP,'--force'],'destroy-exact-owned',20000);report.ownedDestruction=true;}catch{report.destroyReplyUncertain=true;}}
      const until=Math.min(cleanupDeadline-30000,now()+25000);let remaining=await inventory();
      while(remaining.some(collision)&&now()<until){await pause(500);remaining=await inventory();}
      report.cleanupVerified=!remaining.some(collision);
      report.independentAbsence=!(await inventory()).some(collision);
      report.reconciliationRequired=creationAttempted&&!o.pinned();
      need(!report.reconciliationRequired,'UNOBSERVED_CREATE_REQUIRES_RECONCILIATION');
      need(report.cleanupVerified&&report.independentAbsence,'OWNED_MACHINE_STILL_PRESENT');
      await save('cleanup.json',JSON.stringify({formatVersion:1,nonce:p.input.nonce,role:ROLE,revision:REVISION,
        ...o.pinned(),ownedDestruction:report.ownedDestruction,twoFreshInventoriesAbsent:true,utc:new Date(now()).toISOString()}));
    }catch(error){report.cleanupVerified=false;report.cleanupFailure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'CLEANUP_REJECTED';}
    report.reconciliationRequired ||= creationAttempted&&!o.pinned();
    report.cleanupRequired=creationAttempted&&(!report.cleanupVerified||!report.independentAbsence||report.reconciliationRequired);
    report.elapsedMs=Math.max(now()-started,performance.now()-monotonicStarted);report.withinLifetimeBound=report.elapsedMs>=0&&report.elapsedMs<360000;report.utc=new Date(now()).toISOString();
    report.qualified=!mocked&&report.smokePassed&&report.rootStoragePassed&&report.ownedDestruction&&report.cleanupVerified&&report.independentAbsence&&report.withinLifetimeBound&&!report.reconciliationRequired;
    if(mocked)report.mockPassed=report.smokePassed&&report.rootStoragePassed&&report.ownedDestruction&&report.cleanupVerified&&report.independentAbsence&&!report.reconciliationRequired;
    await save('report.json',JSON.stringify(report,null,2)+'\n');
  }
  return {report:path.join(directory,'report.json'),qualified:report.qualified,smokePassed:report.smokePassed,rootStoragePassed:report.rootStoragePassed,
    cleanupVerified:report.cleanupVerified,independentAbsence:report.independentAbsence,ownedDestruction:report.ownedDestruction,
    reconciliationRequired:report.reconciliationRequired,cleanupRequired:report.cleanupRequired,
    withinLifetimeBound:report.withinLifetimeBound,providerCalls:report.providerCalls,testOnlyMock:mocked,...(mocked?{mockPassed:report.mockPassed}:{})};
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  main(process.argv.slice(2)).then(result=>{console.log(JSON.stringify(result));if(result.status!=='prepared-no-run'&&!result.qualified)process.exitCode=1;})
    .catch(()=>{console.log(JSON.stringify({status:'rejected',failure:'CONTROLLER_INPUT_OR_LOCAL_IO'}));process.exitCode=1;});
}
