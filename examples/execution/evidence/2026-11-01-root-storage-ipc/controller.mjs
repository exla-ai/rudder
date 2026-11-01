// PRIVATE. Import/default performs no provider call or filesystem mutation.
import fs from 'node:fs/promises';
import path from 'node:path';
import {parseArgs} from 'node:util';
import {fileURLToPath} from 'node:url';
import {plan,ownership,requireValue as need,requirePhysicalInputs,APP,ROLE,IMAGE,REVISION,VERSION,ROOT_FLAGS,FORBIDDEN_ID,FORBIDDEN_INSTANCE,QUALIFIER_PATH,QUALIFIER_TEST,QUALIFIER_ARGS,sha} from './ownership.mjs';
import {guestRoot,guestSmoke} from './guest.mjs';
import {command} from './transport.mjs';
import {validateQualifierOutput} from './qualifier-marker.mjs';
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
export function validateQualifier(q,p) {
  need(q&&JSON.stringify(q.argv)===JSON.stringify([QUALIFIER_PATH,...QUALIFIER_ARGS]),'QUALIFIER_ARGV');
  need(q.artifact?.path===QUALIFIER_PATH&&q.artifact.sha256===p.input.qualifierElfSha256
    &&q.sourceTreeSha256===p.input.sourceTreeSha256&&q.elfMagicVerified===true,'QUALIFIER_ARTIFACT');
  const c=q.child;
  need(c?.exitObserved===true&&c.settled===true&&c.code===0&&c.signal===null&&c.failure===null,'QUALIFIER_NOT_SETTLED');
  const stdout=decodeLog(q.stdout,131072),stderr=decodeLog(q.stderr,16384);
  need(c.stdoutBytes===stdout.length&&c.stderrBytes===stderr.length&&c.stdoutSha256===sha(stdout)&&c.stderrSha256===sha(stderr),'QUALIFIER_OUTPUT_RECEIPT');
  const marker=validateQualifierOutput(stdout.toString('utf8'),QUALIFIER_TEST,p.input.qualifierElfSha256);
  need(JSON.stringify(marker)===JSON.stringify(q.receipt),'QUALIFIER_MARKER_RECEIPT');
  return {label:'root-storage-ipc-qualifier',stdout,stderr};
}
export function validateProof(proof,p) {
  need(proof?.formatVersion===1&&proof.kind===ROLE&&proof.qualified===true,'GUEST_SMOKE_FAILED');
  for(const [key,value]of Object.entries(p.input))need(proof[key]===value,'PROOF_BINDING_CHANGED');
  need(proof.stockImage===false&&proof.derivedImage===true&&proof.admittedAfterResolvedDigest===true
    &&proof.modelCalls===false&&proof.supervisorLoaded===false&&proof.rootIpcComponent===true&&proof.productionStartup===false&&proof.spec3===false
    &&proof.models===false&&proof.acceptedJournalAuthority===false&&proof.normalCleanupPerformed===false
    &&proof.normalRetirementBlocked===true&&proof.retainedUntilVmTeardown===true,'PROOF_SCOPE');
  need(proof.packageVersion===VERSION&&/^v22\./.test(proof.node?.version||'')&&proof.node?.resolvedPath==='/usr/local/bin/node','ROOT_RUNTIME_IDENTITY');
  const smoke=proof.smoke;
  need(smoke?.formatVersion===1&&smoke.kind==='stock23-uid1500-smoke'&&smoke.status==='passed'&&smoke.nonce===p.input.nonce&&smoke.packageVersion===VERSION&&smoke.modelCalls===false&&smoke.supervisorLoaded===false&&smoke.environmentCleared===true,'UID1500_PROOF');
  need(smoke.identity?.uid===1500&&smoke.identity.gid===1500&&Array.isArray(smoke.identity.groups)&&smoke.identity.groups.length===0&&smoke.identity.noNewPrivs===1,'UID1500_IDENTITY');
  for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])need(/^0+$/.test(smoke.identity.capabilities?.[key]||''),'UID1500_CAPABILITIES');
  const paths={node:'/usr/local/bin/node',native:'/opt/rudder/dist/native/rudder-native',buddy:'/opt/rudder/dist/native/rudder-jj',cli:'/opt/rudder/dist/index.js',package:'/opt/rudder/package.json',qualifier:QUALIFIER_PATH};
  need(JSON.stringify(Object.keys(proof.artifacts||{}).sort())===JSON.stringify(Object.keys(paths).sort()),'ARTIFACT_SET');
  for(const [key,file]of Object.entries(paths)){const a=proof.artifacts[key],b=smoke.artifacts?.[key];
    need(a?.path===file&&/^[a-f0-9]{64}$/.test(a.sha256||'')&&Number.isSafeInteger(a.bytes)&&a.bytes>0&&a.bytes<=536870912,'ARTIFACT_IDENTITY');
    need(JSON.stringify(a)===JSON.stringify(b),'ARTIFACT_CHANGED_ACROSS_UID_DROP');}
  need(JSON.stringify(proof.artifacts.qualifier)===JSON.stringify(proof.qualifier?.artifact),'QUALIFIER_ELF_CHANGED');
  const qualifierLog=validateQualifier(proof.qualifier,p);
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
  const logs=smoke.commands.map((result,index)=>{
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
  return [...logs,qualifierLog];
}
export async function main(argv,{transport=command,pause=delay,now=Date.now}={}) {
  const {values:v}=parseArgs({args:argv,strict:true,options:{run:{type:'boolean'},image:{type:'string'},revision:{type:'string'},digest:{type:'string'},nonce:{type:'string'}}});
  if(!v.run)return {status:'prepared-no-run',providerCalls:0};
  // Root's frozen source/ELF/derived-image receipts are mandatory before any
  // physical run files or provider calls. Mocks never qualify real evidence.
  if(transport===command)requirePhysicalInputs(v);
  const started=now(),p=plan(v,started,guestRoot,guestSmoke),o=ownership(p),mocked=transport!==command||pause!==delay||now!==Date.now;
  const monotonicStarted=performance.now();
  const directory=path.join(HERE,'run-root-storage-ipc-'+p.input.nonce);await fs.mkdir(directory,{mode:0o700});
  const save=(name,bytes)=>fs.writeFile(path.join(directory,name),bytes,{mode:0o600,flag:'wx'});
  const report={formatVersion:1,kind:ROLE,...p.input,requestedImage:IMAGE,testOnlyMock:mocked,providerCalls:0,transportCalls:0,maxTransportCalls:26,proofReads:0,
    proofPollPolicy:{maxReads:5,initialWaitMs:15000,pendingWaitMs:12000,foregroundCallLimit:18,recoveryCallLimit:20},
    rootIpcComponent:true,productionStartup:false,spec3:false,models:false,acceptedJournalAuthority:false,
    normalCleanupPerformed:false,normalRetirementBlocked:false,retainedUntilVmTeardown:false,
    smokePassed:false,qualified:false,cleanupVerified:false,independentAbsence:false,ownedDestruction:false,steps:[],maxLifetimeSeconds:360};
  const mainDeadline=started+220000,cleanupDeadline=started+350000;
  let serial=0,cleanup=false,recovery=false,creationAttempted=false,secretChecks=0;
  const call=async(args,label,maximum=15000,limit=1048576)=>{
    need(!args.some(value=>value===FORBIDDEN_ID||value===FORBIDDEN_INSTANCE),'PROTECTED_HOME_TARGET');
    const deadline=cleanup?cleanupDeadline:mainDeadline;
    const timeout=Math.min(maximum,deadline-now()-2000,
      deadline-started-(performance.now()-monotonicStarted)-2000);need(timeout>0,'CONTROLLER_DEADLINE');
    need(report.transportCalls<(cleanup?26:recovery?20:18),'CONTROLLER_CALL_BOUND');
    report.transportCalls++;if(!mocked)report.providerCalls++;
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
  const readSource=(recovery=false)=>`const fs=require('node:fs'),crypto=require('node:crypto');const root=${JSON.stringify(p.root)},nonce=${JSON.stringify(p.input.nonce)};const fail=()=>{throw Error('PRIVATE_FILE_REJECTED');};const d=fs.lstatSync(root);if(!d.isDirectory()||d.isSymbolicLink()||d.uid!==0||d.gid!==0||(d.mode&4095)!==448)fail();const read=name=>{const f=fs.openSync(root+'/'+name,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);try{const s=fs.fstatSync(f);if(!s.isFile()||s.uid!==0||s.gid!==0||s.nlink!==1||(s.mode&4095)!==384||s.size>1048576)fail();const b=fs.readFileSync(f),t=fs.lstatSync(root+'/'+name);if(b.length!==s.size||t.dev!==s.dev||t.ino!==s.ino||t.size!==s.size||t.nlink!==1)fail();return b;}finally{fs.closeSync(f);}};${recovery?`const logs={};for(const name of ['child.stdout.private','child.stderr.private','qualifier.stdout.private','qualifier.stderr.private'])try{const b=read(name);logs[name]={bytes:b.length,sha256:crypto.createHash('sha256').update(b).digest('hex'),base64:b.toString('base64')};}catch(e){if(e.code!=='ENOENT')throw e;}console.log(JSON.stringify({nonce,logs}));`:`try{console.log(read('proof.json').toString('utf8'));}catch(e){if(e.code!=='ENOENT')throw e;console.log(JSON.stringify({status:'pending',nonce}));}`}`;
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
    // Each proof read costs a fresh ownership inventory plus exec. Normal
    // work stops at 18 calls, retaining two for diagnostic recovery and six
    // for exact-owned teardown. Waiting never renews the original deadline.
    const waitProof=async ms=>{
      const remaining=Math.min(mainDeadline-now(),mainDeadline-started-(performance.now()-monotonicStarted));
      need(remaining>ms+5000,'SMOKE_PROOF_DEADLINE');await pause(ms);
    };
    let proof;await waitProof(15000);
    for(let read=0;read<5;read++){
      need(report.transportCalls+2<=18,'SMOKE_PROOF_POLL_BOUND');report.proofReads++;
      proof=await ownedExec(readSource(),'proof');if(proof.status!=='pending')break;
      if(read<4)await waitProof(12000);
    }
    need(proof&&proof.status!=='pending','SMOKE_PROOF_POLL_BOUND');
    await save('guest-proof.json',JSON.stringify(proof,null,2)+'\n');
    const logs=validateProof(proof,p);for(const item of logs)for(const stream of ['stdout','stderr'])await save(item.label+'.'+stream+'.private',item[stream]);
    report.smokePassed=true;report.rootIpcComponentPassed=true;report.normalRetirementBlocked=true;report.retainedUntilVmTeardown=true;report.artifacts=proof.artifacts;report.node=proof.node;report.packageVersion=proof.packageVersion;report.qualifier=proof.qualifier;
  }catch(error){report.failure=/^[A-Z0-9_]+$/.test(error?.code||'')?error.code:'SMOKE_REJECTED';
    if(o.pinned()&&now()<mainDeadline-5000)try{
      recovery=true;const recovered=await ownedExec(readSource(true),'recover-private-logs');need(recovered.nonce===p.input.nonce,'RECOVERY_NONCE');
      for(const [name,envelope]of Object.entries(recovered.logs||{})){need(['child.stdout.private','child.stderr.private','qualifier.stdout.private','qualifier.stderr.private'].includes(name),'RECOVERY_PATH');await save('recovered-'+name,decodeLog(envelope,1048576));}
      report.privateLogsRecovered=true;
    }catch{report.privateLogsRecovered=false;}
  }finally{
    cleanup=true;recovery=false;report.secretChecks=secretChecks;
    try{
      const all=await inventory(),found=all.filter(o.matches);need(found.length<=1,'MULTIPLE_CLEANUP_MATCHES');
      need(!all.filter(collision).some(machine=>!o.matches(machine)),'FOREIGN_INSTANCE_REQUIRES_RECONCILIATION');
      if(found.length){if(!o.pinned())report.machine=o.remember(found[0]);const pin=o.pinned();
        // Re-read immediately before mutation. A matching ID with a replaced
        // instance/config/image never authorizes a destroy.
        const fresh=(await inventory()).filter(machine=>machine.id===pin.id);need(fresh.length===1&&o.matches(fresh[0]),'DESTROY_OWNERSHIP_CHANGED');
        try{await call(['machine','destroy',pin.id,'-a',APP,'--force'],'destroy-exact-owned',20000);report.ownedDestruction=true;}catch{report.destroyReplyUncertain=true;}}
      const until=Math.min(cleanupDeadline-30000,now()+25000);let remaining=await inventory();
      while(remaining.some(collision)&&now()<until&&report.transportCalls<25){await pause(500);remaining=await inventory();}
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
    report.qualified=!mocked&&report.smokePassed&&report.ownedDestruction&&report.cleanupVerified&&report.independentAbsence&&report.withinLifetimeBound&&!report.reconciliationRequired;
    if(mocked)report.mockPassed=report.smokePassed&&report.ownedDestruction&&report.cleanupVerified&&report.independentAbsence&&!report.reconciliationRequired;
    await save('report.json',JSON.stringify(report,null,2)+'\n');
  }
  return {report:path.join(directory,'report.json'),qualified:report.qualified,smokePassed:report.smokePassed,
    cleanupVerified:report.cleanupVerified,independentAbsence:report.independentAbsence,ownedDestruction:report.ownedDestruction,
    reconciliationRequired:report.reconciliationRequired,cleanupRequired:report.cleanupRequired,
    withinLifetimeBound:report.withinLifetimeBound,providerCalls:report.providerCalls,transportCalls:report.transportCalls,maxTransportCalls:26,testOnlyMock:mocked,...(mocked?{mockPassed:report.mockPassed}:{})};
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  main(process.argv.slice(2)).then(result=>{console.log(JSON.stringify(result));if(result.status!=='prepared-no-run'&&!result.qualified)process.exitCode=1;})
    .catch(()=>{console.log(JSON.stringify({status:'rejected',failure:'CONTROLLER_INPUT_OR_LOCAL_IO'}));process.exitCode=1;});
}
