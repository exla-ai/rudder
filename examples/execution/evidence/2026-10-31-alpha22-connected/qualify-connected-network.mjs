// PRIVATE opt-in controller. Import, default and --describe perform no provider
// calls, DNS, filesystem writes or process launches. Root alone invokes --run.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash, randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {parseArgs} from 'node:util';
import {fileURLToPath} from 'node:url';
import {verifyArtifactInputs} from './artifact-inputs.mjs';

const APP = 'rudder-workers-exla';
const FLY = '/workspace/home/.fly/bin/flyctl';
const ROLE = 'alpha22-connected-network-qualification';
const FORBIDDEN = '8edd225a7d0378';
const FORBIDDEN_INSTANCE = '01M670MG4A8E0DWE9GA1C0PQ92';
const FORBIDDEN_IMAGE = 'sha256:404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245';
const BINARY = '/usr/local/bin/rudder-connected-network-qualifier';
const PROVENANCE = '/usr/local/share/rudder-connected-network-qualifier-provenance.json';
const PARENT = 'sha256:53c763d9e5886cadc262fa03631e5d08338c44486e2f3ffbb8e0afba4c21d89f';
const MAX_PROOF = 1024 * 1024, MAX_STDERR = 8192;
const MAIN_MS = 400000, LIFETIME_MS = 540000;
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const digestPattern = /^sha256:[0-9a-f]{64}$/;
const shaPattern = /^[0-9a-f]{64}$/;
const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function canonicalHost(host) {
  return typeof host === 'string' && host.length <= 253 && host.includes('.') && /[a-z]/.test(host)
    && /^[a-z0-9.-]+$/.test(host) && !['localhost','local','localdomain','internal','invalid','test']
      .some(suffix => host === suffix || host.endsWith('.' + suffix))
    && host.split('.').every(part => part.length > 0 && part.length <= 63 && !part.startsWith('-') && !part.endsWith('-'));
}
export function options(argv) {
  const {values} = parseArgs({args: argv, strict: true, allowPositionals: false, options: {
    run: {type:'boolean'}, describe:{type:'boolean'}, image:{type:'string'}, digest:{type:'string'},
    'parent-digest':{type:'string'}, 'qualifier-sha256':{type:'string'}, 'source-envelope-sha256':{type:'string'},
    'native-sha256':{type:'string'},'source-tree-sha256':{type:'string'},'compiler-receipt-sha256':{type:'string'},
    'artifact-binding':{type:'string'},'artifact-binding-sha256':{type:'string'},
    revision:{type:'string'}, 'source-kind':{type:'string'}, nonce:{type:'string'}, host:{type:'string'},
  }});
  if (!values.run) return {run:false};
  assert(!values.describe);
  assert.match(values.image || '', /^registry\.fly\.io\/rudder-workers-exla:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$/);
  assert.match(values.digest || '', digestPattern);
  assert.equal(values['parent-digest'], PARENT);
  assert.notEqual(values.digest, PARENT);
  assert.notEqual(values.digest, FORBIDDEN_IMAGE);
  assert.match(values.image,/^registry\.fly\.io\/rudder-workers-exla:alpha22-connect-canary-[A-Za-z0-9_.-]{1,80}$/);
  assert.notEqual(values.image, 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.21');
  assert.notEqual(values.image, 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.22');
  for (const key of ['qualifier-sha256','source-envelope-sha256','source-tree-sha256','compiler-receipt-sha256',
    'native-sha256','artifact-binding-sha256']) assert.match(values[key] || '', shaPattern);
  assert(path.isAbsolute(values['artifact-binding']||''));
  assert.match(values.revision || '', /^[0-9a-f]{40}$/);
  assert.equal(values['source-kind'],'frozen-working-tree');
  const nonce = values.nonce || randomUUID(), host = values.host || 'rudder.exla.ai';
  assert.match(nonce, uuidPattern); assert.notEqual(nonce, '00000000-0000-0000-0000-000000000000');
  assert(canonicalHost(host));
  return {run:true,image:values.image,digest:values.digest,parentDigest:values['parent-digest'],
    qualifierSha256:values['qualifier-sha256'],sourceEnvelopeSha256:values['source-envelope-sha256'],
    sourceTreeSha256:values['source-tree-sha256'],compilerReceiptSha256:values['compiler-receipt-sha256'],
    nativeSha256:values['native-sha256'],artifactBinding:values['artifact-binding'],artifactBindingSha256:values['artifact-binding-sha256'],
    revision:values.revision,sourceKind:values['source-kind'],nonce,host};
}
export function provenanceFor(value) {
  return {formatVersion:1,kind:'rudder-connected-network-qualifier-image',proposedVersion:'4.0.0-alpha.22',
    workerVersion:'4.0.0-alpha.21',sourceKind:value.sourceKind,revision:value.revision,
    sourceBaseRevision:value.revision,baseRevisionIsCallerDeclared:true,sourceEnvelopeSha256:value.sourceEnvelopeSha256,
    sourceTreeSha256:value.sourceTreeSha256,compilerReceiptSha256:value.compilerReceiptSha256,
    qualifierSha256:value.qualifierSha256,nativeSha256:value.nativeSha256,nativeBinaryExecuted:false,parentDigest:value.parentDigest};
}
// Exact public-IPv4 qualification table from network profile1. This does not
// replace the Rust transport's separately refreshed host-address deny set.
export function sealDnsAnswers(host, addresses, assert) {
  assert(typeof host === 'string' && Array.isArray(addresses) && addresses.length >= 1 && addresses.length <= 4);
  const denied = [[0x00000000,8],[0x0a000000,8],[0x64400000,10],[0x7f000000,8],[0xa9fe0000,16],
    [0xac100000,12],[0xc0000000,24],[0xc0000200,24],[0xc01fc400,24],[0xc034c100,24],[0xc0586300,24],
    [0xc0a80000,16],[0xc0af3000,24],[0xc6120000,15],[0xc6336400,24],[0xcb007100,24],[0xe0000000,3]];
  const records = addresses.map(address => {
    assert(typeof address === 'string' && /^(0|[1-9][0-9]{0,2})(\.(0|[1-9][0-9]{0,2})){3}$/.test(address));
    const octets = address.split('.').map(Number); assert(octets.every(value => value <= 255));
    const value = octets.reduce((n, byte) => n * 256 + byte, 0);
    assert(!denied.some(([prefix,bits]) => ((value & (0xffffffff << (32-bits))) >>> 0) === prefix));
    return {address,value};
  }).sort((a,b) => a.value-b.value);
  assert.equal(new Set(records.map(row => row.value)).size, records.length);
  return {host,port:443,ipv4:records.map(row => row.address),lookupCalls:1,searchSuffixExpansion:false};
}
export function parseProcStatus(text) {
  const wanted = ['Uid','Gid','Groups','NoNewPrivs','CapEff','CapPrm','CapInh','CapAmb','CapBnd'];
  const fields = new Map();
  for (const match of text.matchAll(/^([A-Za-z]+):[ \t]*([^\r\n]*)$/gm)) {
    if (wanted.includes(match[1])) fields.set(match[1],match[2].trim());
  }
  return Object.fromEntries(wanted.map(key => [key,fields.get(key)]));
}
export function heldNamespaceWitness(fs, assert, kind) {
  assert(['user','net','mnt'].includes(kind));
  const fd = fs.openSync('/proc/self/ns/' + kind,fs.constants.O_RDONLY);
  try {
    const before = fs.fstatSync(fd,{bigint:true});
    assert.equal(fs.statfsSync('/proc/self/fd/' + fd).type,0x6e736673);
    const after = fs.fstatSync(fd,{bigint:true});
    assert.equal(after.dev,before.dev); assert.equal(after.ino,before.ino);
    return {fd,device:before.dev.toString(),inode:before.ino.toString()};
  } catch(error) {fs.closeSync(fd); throw error;}
}
export function privateErrorDetail(error) {
  const source = String(error?.stack || error);
  const compact = source.replace(/data:[^\s,]+;base64,[A-Za-z0-9+/=]+/g,'data:<inline-preload>');
  return {raw:Buffer.from(source),formatted:Buffer.from(compact),shortened:compact !== source};
}
export function readProtectedRuntime(input, fs, crypto, assert) {
  for (const directory of ['/usr','/usr/local','/usr/local/bin','/usr/local/share']) {
    const stat = fs.lstatSync(directory);
    assert(stat.isDirectory() && !stat.isSymbolicLink() && stat.uid === 0 && stat.gid === 0 && !(stat.mode & 0o022));
  }
  const read = (file,max,mode,hashOnly=false) => {
    const fd = fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try {
      const stat = fs.fstatSync(fd);
      assert(stat.isFile() && stat.uid === 0 && stat.gid === 0 && stat.nlink === 1 && (stat.mode & 0o7777) === mode && stat.size > 0 && stat.size <= max);
      const hash = crypto.createHash('sha256'), buffer = Buffer.alloc(Math.min(max+1,65536));
      let count=0; const pieces=[];
      for (;;) {const size=fs.readSync(fd,buffer,0,buffer.length,null);if(!size)break;
        count+=size;assert(count<=max && count<=stat.size);hash.update(buffer.subarray(0,size));
        if(!hashOnly)pieces.push(Buffer.from(buffer.subarray(0,size)));}
      assert.equal(count,stat.size);
      const end=fs.fstatSync(fd), named=fs.lstatSync(file);
      for(const next of [end,named]) assert(next.isFile() && !next.isSymbolicLink() && next.dev===stat.dev && next.ino===stat.ino
        && next.size===stat.size && next.uid===0 && next.gid===0 && next.nlink===1 && next.mode===stat.mode);
      return {bytes:hashOnly?null:Buffer.concat(pieces),sha256:hash.digest('hex'),size:count};
    } finally {fs.closeSync(fd);}
  };
  const provenance=read(input.provenancePath,4096,0o644);
  assert.deepEqual(JSON.parse(provenance.bytes),input.provenance);
  const binary=read(input.binary,256*1024*1024,0o755,true);
  assert.equal(binary.sha256,input.provenance.qualifierSha256);
  // Only a nonexecutable stock version fixture accepts the archive's root-group
  // 0664 spelling. Binary, provenance and parent requirements stay exact.
  const packagePath='/opt/rudder/package.json', meta=fs.lstatSync(packagePath);
  assert(meta.isFile() && !meta.isSymbolicLink() && meta.uid===0 && meta.gid===0 && meta.nlink===1 && [0o644,0o664].includes(meta.mode&0o7777));
  const pkg=read(packagePath,65536,meta.mode&0o7777);
  assert.equal(JSON.parse(pkg.bytes).version,'4.0.0-alpha.21');
  return {qualifierSha256:binary.sha256,qualifierBytes:binary.size,provenanceSha256:provenance.sha256,
    workerVersion:'4.0.0-alpha.21',workerPackageSha256:pkg.sha256};
}
export function validateConnectedProof(value, input, parent, assert) {
  const integer=(value,min,max=Number.MAX_SAFE_INTEGER)=>assert(Number.isSafeInteger(value)&&value>=min&&value<=max);
  const identity=value=>{assert(value && /^[0-9]{1,20}$/.test(value.device) && /^[1-9][0-9]{0,19}$/.test(value.inode));return value;};
  assert.equal(value.kind,'connected-network-qualification-v1');assert.equal(value.status,'passed');
  assert.equal(value.transportVersion,1);assert.equal(value.networkProfileVersion,1);assert.match(value.profileSha256,/^[0-9a-f]{64}$/);
  integer(value.listenerPort,1,65535);
  assert.equal(value.physical.uid,1500);
  for(const key of ['separateUser','separateNetwork','socketNamespaceIoctl','listenerRemovedBeforeExec','allFiveCapabilitiesZero',
    'noNewPrivileges','loOnly','actualGuestReaped','resolverChildrenReaped','proxyWorkersJoined'])assert.equal(value.physical[key],true);
  assert.notDeepEqual(identity(value.guestUser),parent.user);assert.notDeepEqual(identity(value.guestNetwork),parent.net);
  assert.equal(value.client.kind,'public-tls-head-v1');assert.equal(value.client.certificateVerified,true);
  assert(['TLSv1.2','TLSv1.3'].includes(value.client.tlsVersion));integer(value.client.httpStatus,200,399);
  integer(value.client.responseHeaderBytes,1,16384);
  const c=value.counters;assert.equal(c.accepted,1);integer(c.completed,0,1);integer(c.rejected,0,1);assert.equal(c.completed+c.rejected,1);
  assert.equal(c.active_connections,0);assert.equal(c.maximum_active_connections,1);assert.equal(c.dns_jobs,1);assert.equal(c.active_dns_jobs,0);
  assert.equal(c.maximum_active_dns_jobs,1);assert.equal(c.upstream_connections,1);assert.equal(c.cancellation_requested,true);integer(c.charged_bytes,1,2097152);
  assert.deepEqual(value.limits,{lifetimeConnections:32,dnsProcesses:1,connectionWorkers:2,combinedBufferBytesPerConnection:32768,totalBytes:2097152,connectionTotalMs:20000});
  assert(Array.isArray(value.limitations) && value.limitations.length >= 5 && value.limitations.every(row=>typeof row==='string'&&row.length<=256));
  assert(input.host && input.provenance.qualifierSha256);
  return {qualified:1,unsupported:0,failed:0,publicHttpsConnectCasesPassed:1};
}

export async function preload(input, parseProcStatus, heldNamespaceWitness, privateErrorDetail, readProtectedRuntime, sealDnsAnswers, validateConnectedProof) {
  // Independent workload fuse. Awaited DNS/children cannot block this timer.
  setTimeout(()=>process.exit(124),420000);
  const fs=await import('node:fs'),cp=await import('node:child_process'),crypto=await import('node:crypto');
  const {default:assert}=await import('node:assert/strict'),dns=await import('node:dns/promises'),os=await import('node:os');
  const result={formatVersion:1,kind:'alpha22-connected-network-qualification',nonce:input.nonce,provenance:input.provenance,
    supervisorModuleEvaluated:false,productionWired:false,managedConnectedExecutionQualified:false,modelProviderAccountCalls:false,
    modelCalls:false,sourceAcceptanceQualified:false,transportOnly:true,expiryFencingPhysicalQualified:false,completed:false,qualified:false,attempted:0};
  const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
  let stage='admission-gate',parentUser,parentNet,parentMnt;
  const status=()=>parseProcStatus(fs.readFileSync('/proc/self/status','utf8'));
  const witness=kind=>heldNamespaceWitness(fs,assert,kind);
  const identity=value=>({device:value.device,inode:value.inode});
  const current=kind=>{const value=witness(kind);fs.closeSync(value.fd);return identity(value);};
  const digest=bytes=>crypto.createHash('sha256').update(bytes).digest('hex');
  const writePrivate=(file,bytes)=>{const fd=fs.openSync(file,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK,0o600);
    try{fs.writeFileSync(fd,bytes);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}};
  const readGate=()=>{
    const fd=fs.openSync(input.gatePath,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
    try{const stat=fs.fstatSync(fd);assert(stat.isFile()&&stat.uid===1500&&stat.gid===1500&&stat.nlink===1&&(stat.mode&0o7777)===0o600&&stat.size<=4096);
      const bytes=Buffer.alloc(4097);let n=0;for(;;){const size=fs.readSync(fd,bytes,n,bytes.length-n,null);if(!size)break;n+=size;assert(n<=4096);}
      const end=fs.fstatSync(fd),named=fs.lstatSync(input.gatePath);
      for(const next of [end,named])assert(next.isFile()&&!next.isSymbolicLink()&&next.dev===stat.dev&&next.ino===stat.ino&&next.nlink===1&&next.size===n&&next.uid===1500&&next.gid===1500&&next.mode===stat.mode);
      assert.deepEqual(JSON.parse(bytes.subarray(0,n)),input.gate);
    }finally{fs.closeSync(fd);}
  };
  const run=pins=>new Promise(resolve=>{
    const child=cp.spawn(input.binary,['qualify','--base',input.fixtureBase,'--host',input.host,'--ipv4',pins.join(',')],{
      cwd:input.home,env:{PATH:'/usr/local/bin:/usr/bin:/bin',HOME:input.home,RUDDER_HOME:input.home+'/rudder',
        CLAUDE_CONFIG_DIR:input.home+'/claude',CODEX_HOME:input.home+'/codex',TMPDIR:input.home+'/tmp',LANG:'C',LC_ALL:'C'},stdio:['ignore','pipe','pipe']});
    const stdout={bytes:Buffer.alloc(0),count:0,hash:crypto.createHash('sha256')},stderr={bytes:Buffer.alloc(0),count:0,hash:crypto.createHash('sha256')};
    let failure,status,signal,ended=false,settleTimer;
    const finish=settled=>{if(ended)return;ended=true;clearTimeout(timer);if(settleTimer)clearTimeout(settleTimer);
      if(!settled){child.stdout.destroy();child.stderr.destroy();child.unref();}
      try{writePrivate(input.home+'/qualifier.stdout.private',stdout.bytes);writePrivate(input.home+'/qualifier.stderr.private',stderr.bytes);}catch{failure||='SAVE_PRIVATE_CHILD_LOGS';}
      const summary=value=>({bytes:value.count,sha256:value.hash.digest('hex'),retainedBytes:value.bytes.length,truncated:value.count>value.bytes.length});
      resolve({status,signal,failure:failure||null,settled,stdoutBytes:stdout.bytes,stdout:summary(stdout),stderr:summary(stderr)});};
    const stop=code=>{if(ended)return;failure||=code;child.kill('SIGKILL');settleTimer||=setTimeout(()=>finish(false),2000);};
    const timer=setTimeout(()=>stop('QUALIFIER_DEADLINE'),55000);
    const collect=(target,max)=>bytes=>{if(ended)return;target.hash.update(bytes);target.count+=bytes.length;
      target.bytes=Buffer.concat([target.bytes,bytes.subarray(0,Math.max(0,max-target.bytes.length))]);if(target.count>max)stop('QUALIFIER_OUTPUT_BOUND');};
    child.stdout.on('data',collect(stdout,1024*1024));child.stderr.on('data',collect(stderr,8192));
    child.on('error',()=>{failure||='QUALIFIER_SPAWN';});child.on('exit',(code,sig)=>{status=code;signal=sig;});child.on('close',()=>finish(true));
  });
  try{
    const deadline=Date.now()+120000;let admitted=false;
    while(Date.now()<deadline){try{readGate();admitted=true;break;}catch(error){
      if(error.code!=='ENOENT'){const meta=fs.lstatSync(input.gatePath);if(meta.isFile()&&meta.uid===1500&&meta.gid===1500&&meta.nlink===2){await sleep(20);continue;}throw error;}
      await sleep(100);}}
    assert(admitted);result.admittedAfterResolvedDigest=true;stage='boot-identity';
    assert.equal(Object.keys(process.env).filter(key=>/TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|AUTH_KEY|SSH_AUTH_SOCK|AWS_ACCESS_KEY/i.test(key)).length,0);
    assert.equal(process.getuid(),1500);assert.equal(process.getgid(),1500);
    const initial=status();assert.equal(initial.Uid,'1500\t1500\t1500\t1500');assert.equal(initial.Gid,'1500\t1500\t1500\t1500');
    assert.equal(initial.Groups,'');assert.equal(initial.NoNewPrivs,'1');for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])assert.match(initial[key],/^0+$/);
    assert.equal(process.env.RUDDER_WORKER_MANAGED_EXECUTION,'1');assert.equal(process.env.RUDDER_WORKER_SKIP_AGENT_UPDATE,'1');
    assert.equal(process.env.RUDDER_WORKER_FUSE_READY,'1');
    result.boot={uid:1500,gid:1500,fields:initial,node:process.version,kernel:os.release(),credentialEnvironmentFields:0};
    stage='baked-image-provenance';result.runtime=readProtectedRuntime(input,fs,crypto,assert);
    stage='private-fixtures';assert.equal(fs.lstatSync('/workspace').uid,1500);
    for(const file of [input.fixtureBase,input.home]){assert(/^\/workspace\/(cq|ch)-[0-9a-f]{8}$/.test(file));assert(!fs.existsSync(file));fs.mkdirSync(file,{mode:0o700});
      const stat=fs.lstatSync(file);assert(stat.isDirectory()&&!stat.isSymbolicLink()&&stat.uid===1500&&stat.gid===1500&&(stat.mode&0o7777)===0o700);assert.equal(fs.realpathSync(file),file);}
    for(const name of ['rudder','claude','codex','tmp'])fs.mkdirSync(input.home+'/'+name,{mode:0o700});
    parentUser=witness('user');parentNet=witness('net');parentMnt=witness('mnt');
    result.parentBefore={user:identity(parentUser),net:identity(parentNet),mnt:identity(parentMnt)};
    stage='seal-public-dns-once';const resolver=new dns.Resolver({timeout:2000,tries:1});
    const addresses=await resolver.resolve4(input.host+'.');result.sealedTarget=sealDnsAnswers(input.host,addresses,assert);
    stage='actual-connected-network';result.attempted=1;const child=await run(result.sealedTarget.ipv4);
    result.child={status:child.status,signal:child.signal,failure:child.failure,settled:child.settled,stdout:child.stdout,stderr:child.stderr};
    result.parentAfter={user:current('user'),net:current('net'),mnt:current('mnt')};assert.deepEqual(result.parentAfter,result.parentBefore);assert.deepEqual(status(),initial);
    result.parentNamespacesAndStatusUnchanged=true;
    assert(!child.failure);assert(child.settled);assert.equal(child.signal,null);assert.equal(child.status,0);
    const lines=child.stdoutBytes.toString('utf8').trim().split('\n').filter(Boolean);assert.equal(lines.length,1);const summary=JSON.parse(lines[0]);
    stage='validate-physical-report';result.counts=validateConnectedProof(summary,input,result.parentBefore,assert);
    result.summary=summary;result.summarySha256=digest(Buffer.from(lines[0]));result.completed=true;result.qualified=true;
  }catch(error){result.failedStage=stage;result.errorCode=typeof error.code==='string'?error.code:'ASSERTION';
    const detail=privateErrorDetail(error),retained=detail.formatted.subarray(0,8192);
    result.privateFailure={bytes:detail.raw.length,sha256:digest(detail.raw),retainedBytes:retained.length,retainedSha256:digest(retained),dataUrlFramesShortened:detail.shortened};
    try{writePrivate(input.failurePath,retained);}catch{}
  }finally{
    if(parentUser)fs.closeSync(parentUser.fd);if(parentNet)fs.closeSync(parentNet.fd);if(parentMnt)fs.closeSync(parentMnt.fd);
    if(!result.qualified)result.counts={qualified:0,unsupported:result.child?.status===77?1:0,failed:result.child?.status===77?0:1,publicHttpsConnectCasesPassed:0};
    result.verifiedAt=new Date().toISOString();const bytes=Buffer.from(JSON.stringify(result,null,2)+'\n');assert(bytes.length<=1024*1024);writePrivate(input.proofPath,bytes);
  }
  // Never return to evaluate the ordinary home/supervisor module.
  await new Promise(()=>{});
}
export function preloadSource(input) {
  return 'await ('+preload.toString()+')('+JSON.stringify(input)+','+parseProcStatus.toString()+','+heldNamespaceWitness.toString()+','
    +privateErrorDetail.toString()+','+readProtectedRuntime.toString()+','+sealDnsAnswers.toString()+','+validateConnectedProof.toString()+');\n';
}
export function plan(value) {
  const nonce=value.nonce,name='rudder-alpha22-connectqual-'+nonce.slice(0,8),provenance=provenanceFor(value);
  const input={nonce,digest:value.digest,provenance,binary:BINARY,provenancePath:PROVENANCE,host:value.host,
    fixtureBase:'/workspace/cq-'+nonce.slice(0,8),home:'/workspace/ch-'+nonce.slice(0,8),gatePath:'/workspace/connected-gate-'+nonce+'.json',
    proofPath:'/workspace/connected-proof-'+nonce+'.json',failurePath:'/workspace/connected-failure-'+nonce+'.private'};
  input.gate={formatVersion:1,nonce,digest:value.digest,revision:value.revision,sourceKind:value.sourceKind,
    sourceEnvelopeSha256:value.sourceEnvelopeSha256,sourceTreeSha256:value.sourceTreeSha256,
    compilerReceiptSha256:value.compilerReceiptSha256,nativeSha256:value.nativeSha256,
    qualifierSha256:value.qualifierSha256,parentDigest:value.parentDigest,host:value.host};
  const source=preloadSource(input);assert(Buffer.byteLength(source)<=48000);
  const config={image:value.image+'@'+value.digest,guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},restart:{policy:'no'},auto_destroy:false,
    services:[],mounts:[],env:{RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_HOME:'1',RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',
      NODE_OPTIONS:'--import=data:text/javascript;base64,'+Buffer.from(source).toString('base64')},
    metadata:{rudder_role:ROLE,qualification:nonce,qualification_revision:value.revision,
      qualification_source_kind:value.sourceKind,qualification_source_envelope:value.sourceEnvelopeSha256}};
  assert(Buffer.byteLength(config.env.NODE_OPTIONS)<65536);return {nonce,name,role:ROLE,provenance,input,source,config};
}
export function rootProbeSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const input=${JSON.stringify(input)},readProtectedRuntime=${readProtectedRuntime.toString()},privateErrorDetail=${privateErrorDetail.toString()};
const result={formatVersion:1,nonce:input.nonce,uid:process.getuid()};try{assert.equal(process.getuid(),0);Object.assign(result,readProtectedRuntime(input,fs,crypto,assert),{status:'passed',provenance:input.provenance});}
catch(error){result.status='failed';result.errorCode=typeof error.code==='string'?error.code:'ASSERTION';process.stderr.write(privateErrorDetail(error).formatted.subarray(0,8192));process.exitCode=1;}console.log(JSON.stringify(result));`;
}

export function ownershipFor(prepared, value) {
  let owned, createdDescriptor;
  const empty = item => item === undefined || Array.isArray(item) && item.length === 0;
  const requested = machine => {
    if (!machine || !/^[0-9a-f]{14}$/.test(machine.id) || machine.id === FORBIDDEN || machine.instance_id === FORBIDDEN_INSTANCE
      || machine.image_ref?.digest === FORBIDDEN_IMAGE || machine.name !== prepared.name) return false;
    const config = machine.config;
    if (!config || !Object.entries(prepared.config.metadata).every(([key, item]) => config.metadata?.[key] === item)) return false;
    const image = config.image, digest = machine.image_ref?.digest;
    if (!(image === prepared.config.image || image === value.image || typeof image === 'string' && image.startsWith(value.image + '@')
      && /^sha256:[0-9a-f]{64}$/.test(image.slice(value.image.length + 1))
      && (!digest || image === value.image + '@' + digest))) return false;
    if (digest !== undefined && !/^sha256:[0-9a-f]{64}$/.test(digest)) return false;
    if (!['services', 'mounts', 'files'].every(key => empty(config[key]))) return false;
    if (!Object.entries(prepared.config.guest).every(([key, item]) => config.guest?.[key] === item)) return false;
    if (!Object.entries(prepared.config.env).every(([key, item]) => config.env?.[key] === item)) return false;
    if (!Object.entries(config.env || {}).every(([key, item]) => Object.hasOwn(prepared.config.env, key)
      || ['PRIMARY_REGION', 'FLY_PRIMARY_REGION'].includes(key) && item === 'sjc')) return false;
    return true;
  };
  const matches = machine => requested(machine) && (!owned || machine.id === owned)
    && (!createdDescriptor?.observedDigest || machine.image_ref?.digest === createdDescriptor.observedDigest);
  const remember = machine => {
    assert(matches(machine), 'exact machine ownership mismatch');
    if (owned) assert.equal(machine.id, owned); else owned = machine.id;
    if (!createdDescriptor) createdDescriptor = {id: owned, name: prepared.name, nonce: prepared.nonce, role: ROLE,
      instance: machine.instance_id ?? null, revision: value.revision, sourceKind: value.sourceKind, sourceEnvelopeSha256: value.sourceEnvelopeSha256,
      requestedImage: prepared.config.image, observedDigest: machine.image_ref?.digest ?? null, services: 0, mounts: 0, files: 0};
    else if (!createdDescriptor.observedDigest && machine.image_ref?.digest) createdDescriptor.observedDigest = machine.image_ref.digest;
    return {...createdDescriptor};
  };
  return {matches, remember, id: () => owned, descriptor: () => createdDescriptor && {...createdDescriptor}};
}


export function gateSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto');const target=${JSON.stringify(input.gatePath)},tmp=target+'.pending-'+crypto.randomUUID();
const fd=fs.openSync(tmp,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK,0o600);
try{fs.writeFileSync(fd,${JSON.stringify(JSON.stringify(input.gate))});fs.fchownSync(fd,1500,1500);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
try{fs.linkSync(tmp,target);}finally{fs.unlinkSync(tmp);}const directory=fs.openSync('/workspace',fs.constants.O_RDONLY|fs.constants.O_DIRECTORY);try{fs.fsyncSync(directory);}finally{fs.closeSync(directory);}
console.log(JSON.stringify({nonce:${JSON.stringify(input.nonce)},published:true}));`;
}


export function parseOwnedExecJson(stdout, step) {
  const fail = (code, guestExitCode) => {
    step.ok = false; step.failure = code; step.validationFailure = code;
    if (guestExitCode !== undefined) step.guestExitCode = guestExitCode;
    throw Object.assign(new Error('owned exec rejected; inspect its private pipe files'), {code});
  };
  for (const match of stdout.matchAll(/^Exit code:[ \t]*(-?[0-9]{1,10})[ \t]*\r?$/gm)) {
    const code = Number(match[1]); if (code !== 0) fail('OWNED_GUEST_EXIT_NONZERO', code);
  }
  let value;
  try {value = JSON.parse(stdout.trim());} catch {fail('OWNED_EXEC_INVALID_JSON');}
  if (value?.status === 'failed') fail('OWNED_GUEST_REPORTED_FAILURE');
  step.jsonValidated = true; return value;
}


export async function preserveOwnedExecOutput(stdout, stderr, stdoutPath, stderrPath, write = fs.writeFile) {
  await write(stdoutPath, stdout, {mode: 0o600, flag: 'wx'});
  await write(stderrPath, stderr, {mode: 0o600, flag: 'wx'});
  return {privateStdoutFile: stdoutPath, privateStderrFile: stderrPath,
    retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
}


export async function attemptFailureDiagnostics(collect, save) {
  try {return await save(await collect());}
  catch (error) {return {attempted: true, completed: false, errorCode: typeof error.code === 'string' ? error.code : 'DIAGNOSTICS_REJECTED'};}
}


async function command(args, {timeout, stdoutLimit, diagnosticPath, ownedStdoutPath}) {
  return new Promise(resolve => {
    const child = spawn(FLY, args, {stdio: ['ignore', 'pipe', 'pipe']});
    let status, signal, failure, ended = false, settleTimer, stdout = Buffer.alloc(0), stderr = Buffer.alloc(0), stdoutBytes = 0, stderrBytes = 0;
    const stdoutHash = createHash('sha256'), stderrHash = createHash('sha256');
    const stop = code => {
      if (ended) return;
      failure ||= code; child.kill('SIGKILL');
      settleTimer ||= setTimeout(() => {failure ||= 'CLI_SETTLEMENT_DEADLINE'; finish(false);}, 2000);
    };
    const timer = setTimeout(() => stop('DEADLINE'), timeout);
    child.stdout.on('data', bytes => {
      if (ended) return;
      stdoutHash.update(bytes); stdoutBytes += bytes.length;
      const keep = bytes.subarray(0, Math.max(0, stdoutLimit - stdout.length)); stdout = Buffer.concat([stdout, keep]);
      if (stdoutBytes > stdoutLimit) stop('STDOUT_BOUND');
    });
    child.stderr.on('data', bytes => {
      if (ended) return;
      stderrHash.update(bytes); stderrBytes += bytes.length;
      stderr = Buffer.concat([stderr, bytes.subarray(0, Math.max(0, MAX_STDERR - stderr.length))]);
      if (stderrBytes > MAX_STDERR) stop('STDERR_BOUND');
    });
    child.on('error', () => {failure ||= 'SPAWN';});
    child.on('exit', (code, sig) => {status = code; signal = sig;});
    const finish = async settled => {
      if (ended) return;
      ended = true; clearTimeout(timer); if (settleTimer) clearTimeout(settleTimer);
      if (!settled) {child.stdout.destroy(); child.stderr.destroy(); child.unref();}
      const diagnostics = {status, signal, failure: failure || null, settled, stdoutBytes, stderrBytes,
        stdoutSha256: stdoutHash.digest('hex'), stderrSha256: stderrHash.digest('hex'),
        retainedStdoutBytes: stdout.length, retainedStderrBytes: stderr.length};
      try {
        if (ownedStdoutPath) Object.assign(diagnostics,
          await preserveOwnedExecOutput(stdout, stderr, ownedStdoutPath, diagnosticPath));
        else if (failure || status !== 0) {
          await fs.writeFile(diagnosticPath, stderr, {mode: 0o600, flag: 'wx'});
          diagnostics.privateStderrFile = diagnosticPath;
        }
        resolve({ok: settled && !failure && status === 0 && !signal, stdout: stdout.toString('utf8'), diagnostics});
      } catch {resolve({ok: false, stdout: '', diagnostics: {...diagnostics, failure: 'SAVE_PRIVATE_DIAGNOSTICS'}});}
    };
    child.on('close', () => finish(true));
  });
}

export function proofReadSource(input) {
  return `const fs=require('node:fs'),crypto=require('node:crypto');const p=${JSON.stringify(input.proofPath)};let fd;
try{fd=fs.openSync(p,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);const m=fs.fstatSync(fd);if(!m.isFile()||m.nlink!==1||m.uid!==1500||m.gid!==1500||(m.mode&0o7777)!==0o600||m.size>1048576)throw Error('unsafe-proof');
const b=Buffer.alloc(1048577);let n=0;for(;;){const q=fs.readSync(fd,b,n,b.length-n,null);if(!q)break;n+=q;if(n>1048576)throw Error('oversize-proof');}const end=fs.fstatSync(fd),named=fs.lstatSync(p);for(const x of [end,named])if(!x.isFile()||x.isSymbolicLink()||x.dev!==m.dev||x.ino!==m.ino||x.uid!==1500||x.gid!==1500||x.mode!==m.mode||x.nlink!==1||x.size!==n)throw Error('changed-proof');console.log(JSON.stringify({status:'ready',sha256:crypto.createHash('sha256').update(b.subarray(0,n)).digest('hex'),proof:JSON.parse(b.subarray(0,n))}));}
catch(e){console.log(JSON.stringify({status:e.code==='ENOENT'?'pending':'rejected'}));}finally{if(fd!==undefined)fs.closeSync(fd);}`;
}
// Exact nonce paths only. Never read a real HOME/profile, environment, source
// context, provider inventory or an arbitrary filename returned by the guest.
export function collectFixtureDiagnostics(input) {
  const fs=require('node:fs'),crypto=require('node:crypto'),assert=require('node:assert/strict');
  assert.match(input.nonce,/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  assert.equal(input.fixtureBase,'/workspace/cq-'+input.nonce.slice(0,8));assert.equal(input.home,'/workspace/ch-'+input.nonce.slice(0,8));
  assert.equal(input.failurePath,'/workspace/connected-failure-'+input.nonce+'.private');
  const result={formatVersion:1,kind:'owned-connected-private-diagnostics',nonce:input.nonce,fixtureBase:input.fixtureBase,home:input.home,fixtureName:null,files:[]};
  const flags=fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK;
  const beneath=(fd,leaf)=>{assert(/^[A-Za-z0-9_.-]+$/.test(leaf)&&!['.','..'].includes(leaf));return '/proc/self/fd/'+fd+'/'+leaf;};
  const directory=(name,workspace=false)=>{const fd=fs.openSync(name,flags|fs.constants.O_DIRECTORY);try{const stat=fs.fstatSync(fd);
    assert(stat.isDirectory()&&stat.uid===1500&&stat.gid===1500&&(workspace||((stat.mode&0o7777)===0o700)));return fd;}catch(error){fs.closeSync(fd);throw error;}};
  const missing=(label,source,status='missing-parent')=>result.files.push({label,source,status});
  const read=(parent,leaf,label,source)=>{let fd;try{const name=beneath(parent,leaf);fd=fs.openSync(name,flags);const stat=fs.fstatSync(fd);
    assert(stat.isFile()&&stat.uid===1500&&stat.gid===1500&&stat.nlink===1&&(stat.mode&0o7777)===0o600&&stat.size<=8192);
    const bytes=Buffer.alloc(8193);let n=0;for(;;){const count=fs.readSync(fd,bytes,n,bytes.length-n,null);if(!count)break;n+=count;assert(n<=8192);}assert.equal(n,stat.size);
    for(const next of [fs.fstatSync(fd),fs.lstatSync(name)])assert(next.isFile()&&!next.isSymbolicLink()&&next.dev===stat.dev&&next.ino===stat.ino&&next.uid===1500&&next.gid===1500&&next.nlink===1&&next.mode===stat.mode&&next.size===n);
    const body=bytes.subarray(0,n);result.files.push({label,source,status:'read',bytes:n,sha256:crypto.createHash('sha256').update(body).digest('hex'),
      metadata:{uid:stat.uid,gid:stat.gid,mode:stat.mode&0o7777,nlink:stat.nlink,size:stat.size},bodyBase64:body.toString('base64')});
    }catch(error){result.files.push({label,source,status:error.code==='ENOENT'?'missing':'rejected',errorCode:typeof error.code==='string'?error.code:'UNSAFE_OR_OVERSIZE'});}
    finally{if(fd!==undefined)fs.closeSync(fd);}};
  let workspace,home,base,fixture;
  try{workspace=directory('/workspace',true);read(workspace,input.failurePath.slice('/workspace/'.length),'preload-failure',input.failurePath);
    try{home=directory(beneath(workspace,input.home.slice('/workspace/'.length)));read(home,'qualifier.stderr.private','qualifier-stderr',input.home+'/qualifier.stderr.private');}
    catch(error){missing('qualifier-stderr',input.home+'/qualifier.stderr.private',error.code==='ENOENT'?'missing-parent':'rejected-parent');}
    try{base=directory(beneath(workspace,input.fixtureBase.slice('/workspace/'.length)));
      const entries=fs.opendirSync('/proc/self/fd/'+base,{bufferSize:2});let first,second;try{first=entries.readSync();second=entries.readSync();}finally{entries.closeSync();}
      if(!first){missing('helper-failure',null);}
      else{assert(!second&&/^connect-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(first.name));
        fixture=directory(beneath(base,first.name));result.fixtureName=first.name;
        read(fixture,'failure.private.txt','helper-failure',input.fixtureBase+'/'+first.name+'/failure.private.txt');}}
    catch(error){missing('helper-failure',null,error.code==='ENOENT'?'missing-parent':'rejected-parent');}
    result.completed=true;
  }catch(error){result.completed=false;result.errorCode=typeof error.code==='string'?error.code:'UNSAFE_ROOT';}
  finally{for(const fd of [fixture,base,home,workspace])if(fd!==undefined)fs.closeSync(fd);}
  console.log(JSON.stringify(result));
}
export function fixtureDiagnosticsSource(input) {
  return '('+collectFixtureDiagnostics.toString()+')('+JSON.stringify(input)+');';
}
export async function saveFixtureDiagnostics(bundle,input,directory,write=fs.writeFile) {
  assert.equal(bundle?.formatVersion,1);assert.equal(bundle.kind,'owned-connected-private-diagnostics');assert.equal(bundle.nonce,input.nonce);
  assert.equal(bundle.fixtureBase,input.fixtureBase);assert.equal(bundle.home,input.home);assert(Array.isArray(bundle.files)&&bundle.files.length<=3);
  if(bundle.fixtureName!==null)assert.match(bundle.fixtureName,/^connect-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  const allowed={'preload-failure':input.failurePath,'qualifier-stderr':input.home+'/qualifier.stderr.private',
    'helper-failure':bundle.fixtureName===null?null:input.fixtureBase+'/'+bundle.fixtureName+'/failure.private.txt'};
  const seen=new Set(),pending=[],metadata=[];
  for(const file of bundle.files){assert(Object.hasOwn(allowed,file.label)&&!seen.has(file.label));seen.add(file.label);assert.equal(file.source,allowed[file.label]);
    assert(['read','missing','missing-parent','rejected','rejected-parent'].includes(file.status));const row={label:file.label,source:file.source,status:file.status};
    if(file.status==='read'){assert(file.source&&typeof file.bodyBase64==='string'&&file.bodyBase64.length<=4*Math.ceil(8192/3));
      const bytes=Buffer.from(file.bodyBase64,'base64');assert.equal(bytes.toString('base64'),file.bodyBase64);assert(bytes.length<=8192);
      assert.equal(file.bytes,bytes.length);assert.equal(file.sha256,sha(bytes));assert.deepEqual(file.metadata,{uid:1500,gid:1500,mode:0o600,nlink:1,size:bytes.length});
      const destination=path.join(directory,'failure-diagnostics-'+file.label+'.private');Object.assign(row,{file:destination,bytes:bytes.length,sha256:file.sha256});pending.push([destination,bytes]);
    }else assert(!Object.hasOwn(file,'bodyBase64'));metadata.push(row);}
  for(const [destination,bytes]of pending)await write(destination,bytes,{mode:0o600,flag:'wx'});
  const result={attempted:true,completed:bundle.completed===true,files:metadata};
  await write(path.join(directory,'failure-diagnostics-manifest.json'),JSON.stringify(result,null,2)+'\n',{mode:0o600,flag:'wx'});return result;
}

export async function main(argv,{runCommand=command,emitProgress=console.log}={}) {
  const value=options(argv);
  if(!value.run)return {kind:ROLE,status:'prepared-no-run',providerCalls:0,productionWired:false,
    usage:'Root uses run-canary.mjs --run --binding ABS_RUN_BINDING_JSON; frozen compiler receipts and exact binary/image hashes are mandatory'};
  const mocked=runCommand!==command;
  const artifactInputs=mocked?{verified:false,testOnlyMock:true}:await verifyArtifactInputs(value);
  const started=Date.now(),mainDeadline=started+MAIN_MS,lifetimeDeadline=started+LIFETIME_MS;
  const prepared=plan(value),ownership=ownershipFor(prepared,value);
  if(!mocked)assert.equal(artifactInputs.verified,true);
  const directory=path.join(path.dirname(fileURLToPath(import.meta.url)),(mocked?'mock-connected-':'connected-')+value.nonce);
  await fs.mkdir(directory,{mode:0o700});const reportPath=path.join(directory,'report.json');
  const report={formatVersion:1,kind:ROLE,name:prepared.name,nonce:value.nonce,requestedImage:value.image,expectedDigest:value.digest,
    requestedPinnedImage:prepared.config.image,artifactInputs,provenance:prepared.provenance,
    startedAt:new Date(started).toISOString(),productionWired:false,managedConnectedExecutionQualified:false,
    modelProviderAccountCalls:false,modelCalls:false,transportOnly:true,testOnlyMock:mocked,steps:[],maxCanaryLifetimeSeconds:540,
    expiryFencingPhysicalQualified:false,nativeBinaryExecuted:false,
    limits:{proofBytes:MAX_PROOF,errorStderrBytes:MAX_STDERR,childTimeoutSeconds:55,preloadWorkloadFuseSeconds:420,
      baseAncestry:'requires retained Root pinned-FROM/full-source/release-ELF build evidence'}};
  const emit=step=>emitProgress(JSON.stringify({step,name:prepared.name,nonce:value.nonce,role:ROLE,expectedDigest:value.digest,report:reportPath}));
  await fs.writeFile(path.join(directory,'preload.mjs'),prepared.source,{mode:0o600,flag:'wx'});
  const configPath=path.join(directory,'machine.json');await fs.writeFile(configPath,JSON.stringify(prepared.config,null,2)+'\n',{mode:0o600,flag:'wx'});
  report.controllerSha256=sha(await fs.readFile(fileURLToPath(import.meta.url)));report.preloadSha256=sha(Buffer.from(prepared.source));report.configSha256=sha(await fs.readFile(configPath));
  let phase='preflight',serial=0,cleanup=false,diagnosing=false,creationAttempted=false,providerCalls=0;
  const call=async(args,label,timeout=15000,stdoutLimit=4*MAX_PROOF,ownedOutput=false)=>{
    phase=label;const remaining=(cleanup?lifetimeDeadline:diagnosing?Math.min(lifetimeDeadline-70000,Date.now()+20000):mainDeadline)-Date.now();
    assert(remaining>0,'controller lifetime exhausted');assert.match(label,/^[a-z][a-z0-9-]{0,80}$/);
    const prefix=path.join(directory,String(++serial).padStart(3,'0')+'-'+label),began=Date.now();if(!mocked)providerCalls++;
    const response=await runCommand(args,{timeout:Math.min(timeout,remaining),stdoutLimit,diagnosticPath:prefix+'-stderr.private',ownedStdoutPath:ownedOutput?prefix+'-stdout.private':undefined});
    const step={step:label,ms:Date.now()-began,ok:response.ok,...response.diagnostics};report.steps.push(step);assert(response.ok,'bounded provider command failed');
    return ownedOutput?parseOwnedExecJson(response.stdout,step):response.stdout;
  };
  const inventory=async()=>{const result=JSON.parse(await call(['machine','list','-a',APP,'--json'],'filtered-inventory'));assert(Array.isArray(result)&&result.length<=10000);return result;};
  const noSecrets=async label=>assert.deepEqual(JSON.parse(await call(['secrets','list','-a',APP,'--json'],label)),[]);
  const rootCommand='/usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin /usr/local/bin/node -e ';
  const ownedExec=async(script,label,stdoutLimit=4*MAX_PROOF)=>{
    const id=ownership.id();assert(id&&id!==FORBIDDEN);const current=(await inventory()).filter(machine=>machine.id===id);assert.equal(current.length,1);assert(ownership.matches(current[0]));
    return call(['machine','exec',id,'-a',APP,'/bin/sh -c '+quote(script),'--timeout','15'],label,20000,stdoutLimit,true);
  };
  try{
    await noSecrets('assert-empty-app-secrets');const before=await inventory();
    assert(!before.some(machine=>machine.name===prepared.name||machine.config?.metadata?.qualification===value.nonce),'preexisting name/nonce collision');
    report.noAppSecretsAtCreation=true;emit('creating-exact-owned-connected-qualification');creationAttempted=true;
    try{await call(['machine','run',prepared.config.image,'-a',APP,'--machine-config',configPath,'--name',prepared.name,'--region','sjc','--detach','--skip-dns-registration'],'create-owned-connected-qualification',70000);}
    catch{report.createReplyUncertain=true;}
    let machine,startRequested=false;const startDeadline=Math.min(mainDeadline,Date.now()+90000);
    while(Date.now()<startDeadline){const matches=(await inventory()).filter(ownership.matches);assert(matches.length<=1);
      if(matches.length){machine=matches[0];report.createdDescriptor=ownership.remember(machine);if(machine.state==='started')break;
        if(machine.state==='created'&&!startRequested){startRequested=true;try{await call(['machine','start',ownership.id(),'-a',APP],'start-reconciled-owned-qualification',30000);}catch{report.startReplyUncertain=true;}}}
      await delay(1000);}
    assert(machine&&ownership.matches(machine));assert.equal(machine.state,'started');assert.equal(machine.image_ref?.digest,value.digest);
    report.machine={id:ownership.id(),name:prepared.name,nonce:value.nonce,role:ROLE,region:machine.region,instance:machine.instance_id,digest:machine.image_ref.digest};
    await noSecrets('recheck-empty-app-secrets');report.noAppSecretsBeforeAdmission=true;
    const probe=await ownedExec(rootCommand+quote(rootProbeSource(prepared.input)),'root-owned-image-provenance-probe',16384);
    assert.equal(probe.status,'passed');assert.equal(probe.uid,0);assert.equal(probe.nonce,value.nonce);assert.equal(probe.qualifierSha256,value.qualifierSha256);
    assert.deepEqual(probe.provenance,prepared.provenance);assert.equal(probe.workerVersion,'4.0.0-alpha.21');assert.match(probe.provenanceSha256,shaPattern);report.rootImageProbe=probe;
    const admitted=await ownedExec(rootCommand+quote(gateSource(prepared.input)),'publish-atomic-admission-gate',4096);
    assert.equal(admitted.nonce,value.nonce);assert.equal(admitted.published,true);report.admitted=true;emit('owned-image-verified-and-admitted');
    let fetched;while(Date.now()<mainDeadline){fetched=await ownedExec(rootCommand+quote(proofReadSource(prepared.input)),'read-bounded-owned-proof',MAX_PROOF+16384);
      if(fetched.status==='ready')break;assert.equal(fetched.status,'pending');await delay(1500);}
    assert.equal(fetched?.status,'ready');const proof=fetched.proof;assert.equal(proof.kind,ROLE);assert.equal(proof.formatVersion,1);assert.equal(proof.nonce,value.nonce);
    assert.deepEqual(proof.provenance,prepared.provenance);const bytes=Buffer.from(JSON.stringify(proof,null,2)+'\n');assert(bytes.length<=MAX_PROOF);assert.equal(sha(bytes),fetched.sha256);
    await fs.writeFile(path.join(directory,'guest-proof.json'),bytes,{mode:0o600,flag:'wx'});report.guestProofSha256=fetched.sha256;
    report.completed=proof.completed===true;report.qualified=proof.qualified===true;report.counts=proof.counts;
    assert.equal(proof.admittedAfterResolvedDigest,true);assert.equal(proof.supervisorModuleEvaluated,false);
    for(const key of ['productionWired','managedConnectedExecutionQualified','modelProviderAccountCalls','modelCalls','sourceAcceptanceQualified'])assert.equal(proof[key],false);
    assert.equal(proof.expiryFencingPhysicalQualified,false);
    assert.equal(proof.transportOnly,true);assert.equal(proof.runtime.qualifierSha256,value.qualifierSha256);assert.equal(proof.runtime.workerVersion,'4.0.0-alpha.21');
    assert.equal(proof.parentNamespacesAndStatusUnchanged,true);assert.deepEqual(proof.parentAfter,proof.parentBefore);assert.equal(proof.boot.uid,1500);assert.equal(proof.boot.gid,1500);assert.equal(proof.boot.credentialEnvironmentFields,0);
    const initial=proof.boot.fields;assert.equal(initial.Uid,'1500\t1500\t1500\t1500');assert.equal(initial.Gid,'1500\t1500\t1500\t1500');assert.equal(initial.Groups,'');assert.equal(initial.NoNewPrivs,'1');
    for(const key of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd'])assert.equal(initial[key],'0000000000000000');
    assert.equal(proof.sealedTarget.host,value.host);assert.deepEqual(sealDnsAnswers(value.host,proof.sealedTarget.ipv4,assert),proof.sealedTarget);
    assert.equal(proof.completed,true);assert.equal(proof.qualified,true);assert.equal(proof.attempted,1);
    assert.deepEqual(validateConnectedProof(proof.summary,prepared.input,proof.parentBefore,assert),proof.counts);
  }catch(error){report.completed=false;report.qualified=false;report.failedPhase=phase;report.errorCode=typeof error.code==='string'?error.code:'ASSERTION';
    const detail=privateErrorDetail(error);report.controllerFailureSha256=sha(detail.raw);
    try{await fs.writeFile(path.join(directory,'controller-failure.private'),detail.formatted.subarray(0,MAX_STDERR),{mode:0o600,flag:'wx'});}catch{report.controllerFailureSaveFailed=true;}
    if(ownership.id()&&report.machine?.id===ownership.id()&&report.machine.digest===value.digest){diagnosing=true;
      report.failureDiagnostics=await attemptFailureDiagnostics(()=>ownedExec(rootCommand+quote(fixtureDiagnosticsSource(prepared.input)),'collect-exact-owned-private-failure-diagnostics',65536),bundle=>saveFixtureDiagnostics(bundle,prepared.input,directory));diagnosing=false;
    }else report.failureDiagnostics={attempted:false,completed:false,reason:'NO_DIGEST_VERIFIED_OWNED_MACHINE'};
    emit('qualification-failed-private-diagnostics-retained');
  }finally{
    cleanup=true;report.creationAttempted=creationAttempted;
    try{
      const absent=machines=>!machines.some(machine=>machine.id===ownership.id()||machine.name===prepared.name||machine.config?.metadata?.qualification===value.nonce);
      if(creationAttempted){const matches=(await inventory()).filter(ownership.matches);assert(matches.length<=1);
        for(const machine of matches){report.createdDescriptor=ownership.remember(machine);assert(ownership.matches(machine));
          try{await call(['machine','destroy',ownership.id(),'-a',APP,'--force'],'destroy-exact-owned-qualification',15000);}catch{report.destroyReplyUncertain=true;}}}
      else report.noDestructiveActionWithoutCreateAttempt=true;
      const absenceDeadline=Math.min(lifetimeDeadline-20000,Date.now()+25000);let remaining=await inventory();
      while(creationAttempted&&!absent(remaining)&&Date.now()<absenceDeadline){await delay(500);remaining=await inventory();}
      report.cleanupVerified=absent(remaining);report.independentAbsence=absent(await inventory());assert(report.cleanupVerified&&report.independentAbsence);
      assert(!(report.createReplyUncertain&&!ownership.id()),'unobserved uncertain create needs explicit reconciliation');
      const proof={formatVersion:1,id:ownership.id()??null,name:prepared.name,nonce:value.nonce,role:ROLE,
        creationAttempted,absentByIdNameNonce:true,independentInventoryAbsent:true,verifiedAt:new Date().toISOString()};const bytes=Buffer.from(JSON.stringify(proof,null,2)+'\n');
      await fs.writeFile(path.join(directory,'cleanup.json'),bytes,{mode:0o600,flag:'wx'});report.cleanupProofSha256=sha(bytes);
    }catch{report.cleanupVerified=false;report.independentAbsence=false;}
    report.elapsedMs=Date.now()-started;report.withinLifetimeBound=report.elapsedMs<LIFETIME_MS;report.verifiedAt=new Date().toISOString();report.providerCalls=providerCalls;
    const bytes=Buffer.from(JSON.stringify(report,null,2)+'\n');assert(bytes.length<=MAX_PROOF);await fs.writeFile(reportPath,bytes,{mode:0o600,flag:'wx'});emit('qualification-finished');
  }
  return {reportPath,qualified:report.qualified===true,cleanupVerified:report.cleanupVerified===true,independentAbsence:report.independentAbsence===true,
    withinLifetimeBound:report.withinLifetimeBound,providerCalls,testOnlyMock:mocked};
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  try{const result=await main(process.argv.slice(2));console.log(JSON.stringify(result));
    if(result.status!=='prepared-no-run'&&!(result.qualified&&result.cleanupVerified&&result.independentAbsence&&result.withinLifetimeBound))process.exitCode=1;
  }catch(error){console.log(JSON.stringify({kind:ROLE,status:'rejected-before-execution',errorCode:typeof error.code==='string'?error.code:'ASSERTION'}));process.exitCode=1;}
}
