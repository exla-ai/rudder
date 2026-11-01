#!/usr/bin/env node
// Single-server SQL qualification only. Creates/deletes at most its exact owned VM.
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import http from 'node:http';
import assert from 'node:assert/strict';
import {createHash, randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {fileURLToPath} from 'node:url';

const exec = promisify(execFile);
const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(scriptDir, '../..');
const fly = process.env.FLYCTL || '/workspace/home/.fly/bin/flyctl';
const app = 'rudder-workers-exla';
const name = process.env.RUDDER_MANAGED_PG_MACHINE_NAME || 'rudder-managed-pg-v3-20261101-a1';
assert.match(name,/^rudder-managed-pg-[a-z0-9-]+$/);
const role = 'managed-storage-pg-qualification';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_RENEWAL));
const mixedRenewal = process.env.RUDDER_MANAGED_PG_RENEWAL === '1';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_HARDLINK));
const hardlinkImage = process.env.RUDDER_MANAGED_PG_HARDLINK === '1';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_TRANSPORT));
const hostTransport = process.env.RUDDER_MANAGED_PG_TRANSPORT === '1';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_IMAGE_REUSE));
const imageReuse = process.env.RUDDER_MANAGED_PG_IMAGE_REUSE === '1';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_BATCHES));
const coldBatches=process.env.RUDDER_MANAGED_PG_BATCHES==='1';
assert([undefined,'0','1'].includes(process.env.RUDDER_MANAGED_PG_IMMUTABLE_BATCHES));
const immutableBatches=process.env.RUDDER_MANAGED_PG_IMMUTABLE_BATCHES==='1';
assert(Number(mixedRenewal)+Number(hardlinkImage)+Number(hostTransport)+Number(imageReuse)+Number(coldBatches)+Number(immutableBatches)<=1,'separate qualification variants');
const followupFiles = immutableBatches?['tests/managed-owner-renewal-pg-followup.mjs','tests/managed-immutable-batch-retention-pg-followup.mjs']:coldBatches?['tests/managed-owner-renewal-pg-followup.mjs','tests/managed-image-batch-pg-followup.mjs']:imageReuse?['tests/managed-owner-renewal-pg-followup.mjs','tests/managed-image-publication-reuse-pg-followup.mjs']:
  hostTransport?['tests/managed-owner-renewal-pg-followup.mjs','tests/managed-host-transport-pg-followup.mjs']:
  mixedRenewal||hardlinkImage?['tests/managed-owner-renewal-pg-followup.mjs',...(hardlinkImage?['tests/managed-storage-hardlink-pg-followup.mjs']:[])]:[];
const forbidden = '8edd225a7d0378';
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const emit = value => console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const sourceFiles = ['cloud/package.json','cloud/package-lock.json',
  ...((await fs.readdir(path.join(root,'cloud/src/project-authority'))).filter(f=>f.endsWith('.ts')).map(f=>'cloud/src/project-authority/'+f)),
  ...((await fs.readdir(path.join(root,'cloud/src/managed-home-provisioning'))).filter(f=>f.endsWith('.ts')).map(f=>'cloud/src/managed-home-provisioning/'+f)),
  ...((await fs.readdir(path.join(root,'cloud/src'))).filter(f=>f.startsWith('fabric-')&&f.endsWith('.ts')).map(f=>'cloud/src/'+f)),
  'cloud/src/fly-custody.ts','tests/managed-storage-pg-qualification.mjs','tests/managed-storage-pg-fixtures.mjs',
  'examples/managed-storage-pg/qualify-managed-node.mjs','examples/managed-storage-pg/qualify-managed-guest.sh','examples/managed-storage-pg/retained-fixture-runner.py','examples/managed-storage-pg/Dockerfile',
  ...followupFiles];

async function officialImage() {
  const image=process.env.RUDDER_MANAGED_PG_IMAGE;
  assert(typeof image==='string'&&/^[a-z0-9./_-]+@sha256:[a-f0-9]{64}$/.test(image),'Root must supply exact derived PG+Python+OpenSSL image digest');
  return {requested:'Root-supplied component PG+Python image',platform:'linux/amd64',manifestDigest:image.slice(image.indexOf('@')+1),image,
    scope:'explicit Root fixture-image selection DATA only; no managed-runtime qualification'};
}


async function prepare(output) {
  output = path.resolve(output);
  await fs.mkdir(output,{recursive:true,mode:0o700});
  const disk=await fs.statfs(output);assert(disk.bavail*disk.bsize>3489660928+256*1024*1024,'Root disk floor plus bounded preparation reserve unavailable');
  const statePath=path.join(output,'state.json');
  await assert.rejects(fs.stat(statePath),error=>error.code==='ENOENT');
  const nonce=randomUUID();
  const stage=await fs.mkdtemp('/workspace/rudder-fleet-source-');
  const sourceHashes={};
  for (const file of sourceFiles) {
    const bytes=await fs.readFile(path.join(root,file));
    sourceHashes[file]=digest(bytes);
    await fs.mkdir(path.dirname(path.join(stage,file)),{recursive:true});
    await fs.writeFile(path.join(stage,file),bytes);
  }
  await fs.writeFile(path.join(stage,'SOURCE-SHA256SUMS'),Object.entries(sourceHashes).map(([file,sha])=>`${sha}  ${file}\n`).join(''));
  await fs.writeFile(path.join(output,'source-hashes.json'),JSON.stringify(sourceHashes,null,2)+'\n');
  const sourceArchive=path.join(output,'source.tar.gz');
  await exec('tar',['-czf',sourceArchive,'-C',stage,...sourceFiles,'SOURCE-SHA256SUMS']);
  await fs.rm(stage,{recursive:true});
  const runtimeStage=await fs.mkdtemp('/workspace/rudder-authority-node-runtime-');
  const runtimeHashes={};
  async function copyFile(source,destination) {
    const data=await fs.readFile(source); runtimeHashes[destination]=digest(data);
    await fs.mkdir(path.dirname(path.join(runtimeStage,destination)),{recursive:true});
    await fs.copyFile(source,path.join(runtimeStage,destination));
  }
  for(const directory of ['project-authority','managed-home-provisioning'])for(const file of await fs.readdir(path.join(root,'cloud/dist',directory))) {
    if(file.endsWith('.js'))await copyFile(path.join(root,'cloud/dist',directory,file),'cloud/dist/'+directory+'/'+file);
  }
  for(const file of await fs.readdir(path.join(root,'cloud/dist')))if(file.endsWith('.js')&&(file.startsWith('fabric-')||file==='fly-custody.js'))await copyFile(path.join(root,'cloud/dist',file),'cloud/dist/'+file);
  await copyFile(path.join(root,'cloud/package.json'),'cloud/package.json');
  const packages=new Set();
  async function dependency(name) {
    if(packages.has(name)) return; packages.add(name);
    const directory=path.join(root,'cloud/node_modules',name);
    const pkg=JSON.parse(await fs.readFile(path.join(directory,'package.json'),'utf8'));
    await fs.cp(directory,path.join(runtimeStage,'cloud/node_modules',name),{recursive:true});
    for(const child of Object.keys(pkg.dependencies||{})) await dependency(child);
  }
  await dependency('pg');
  await dependency('@aws-sdk/client-s3');
  await dependency('@aws-sdk/credential-provider-web-identity');
  if(hostTransport)await dependency('ws');
  await copyFile(process.execPath,'node');
  if(followupFiles.length){
    for(const module of followupFiles)await copyFile(path.join(root,module),module);
    const module=immutableBatches?'tests/managed-immutable-batch-retention-pg-followup.mjs':coldBatches?'tests/managed-image-batch-pg-followup.mjs':imageReuse?'tests/managed-image-publication-reuse-pg-followup.mjs':hostTransport?'tests/managed-host-transport-pg-followup.mjs':hardlinkImage?'tests/managed-storage-hardlink-pg-followup.mjs':'tests/managed-owner-renewal-pg-followup.mjs';
    const descriptor={followupModule:{path:'/var/tmp/rudder-authority-node-runtime/'+module,sha256:runtimeHashes[module]}};
    await fs.writeFile(path.join(runtimeStage,'managed-followup.json'),JSON.stringify(descriptor)+'\n',{mode:0o444});
    runtimeHashes['managed-followup.json']=digest(await fs.readFile(path.join(runtimeStage,'managed-followup.json')));
  }
  const runtimeArchive=path.join(output,'runtime.tar.gz');
  await exec('tar',['-czf',runtimeArchive,'-C',runtimeStage,'.'],{timeout:120000});
  await fs.rm(runtimeStage,{recursive:true});
  const runtime={archive:runtimeArchive,sha256:digest(await fs.readFile(runtimeArchive)),hashes:runtimeHashes,nodeVersion:process.version,packages:[...packages].sort()};
  const image=await officialImage();
  const state={format_version:1,name,nonce,role,app,output,mixedRenewal,hardlinkImage,hostTransport,imageReuse,coldBatches,immutableBatches,sourceHashes,sourceArchive,
    sourceArchiveSha256:digest(await fs.readFile(sourceArchive)),image,runtime};
  await fs.writeFile(statePath,JSON.stringify(state,null,2)+'\n',{mode:0o600});
  emit({step:'prepared-no-machine-created',name,image:image.image,state:statePath});
}

async function run(statePath) {
  const state=JSON.parse(await fs.readFile(statePath,'utf8'));
  for(const file of ['report.json','creation-intent.json'])await assert.rejects(fs.stat(path.join(state.output,file)),e=>e.code==='ENOENT');
  assert.equal(state.name,name); assert.equal(state.role,role); assert.equal(state.app,app);
  assert.equal(state.mixedRenewal,mixedRenewal);
  assert.equal(state.hardlinkImage,hardlinkImage);
  assert.equal(state.hostTransport,hostTransport);
  assert.equal(state.imageReuse??false,imageReuse);
  assert.equal(state.coldBatches??false,coldBatches);
  assert.equal(state.immutableBatches??false,immutableBatches);
  assert.match(state.nonce,/^[0-9a-f-]{36}$/);
  const bytes=await fs.readFile(state.sourceArchive);
  const runtimeBytes=await fs.readFile(state.runtime.archive); assert.equal(digest(runtimeBytes),state.runtime.sha256);
  assert.equal(digest(bytes),state.sourceArchiveSha256);
  const report={format_version:1,kind:'managed-home-storage-postgresql-component-qualification',provider:'Fly.io',app,
    name,qualification:state.nonce,startedAt:new Date().toISOString(),image:state.image,
    sourceArchiveSha256:state.sourceArchiveSha256,sourceHashes:state.sourceHashes,runtime:state.runtime,steps:[],
    scope:'One disposable PostgreSQL server with verified TLS factories; fixture Fly/Root evidence and memory-backed actual FabricCas/S3 adapter; no fleet/HA/realRoot/model proof'};
  let owned, server, phase='preflight',calls=0,createUncertain=false;
  report.providerCallLimit=48;report.cleanupReserve=8;
  async function command(args,label,timeout=30000) {
    phase=label;
    const cleanup=label.startsWith('destroy-')||label.startsWith('absence-')||label==='cleanup-identity';
    assert(calls<(cleanup?48:40),'provider call bound reached; cleanup reserve retained');calls++;
    const began=Date.now();
    const result=await exec(fly,args,{timeout,maxBuffer:8*1024*1024});
    report.steps.push({step:label,ms:Date.now()-began});
    return result.stdout;
  }
  const inventory=async()=>JSON.parse(await command(['machine','list','-a',app,'--json'],'read-machine-inventory'));
  const matches=machine=>machine.id!==forbidden && machine.instance_id!=='01M670MG4A8E0DWE9GA1C0PQ92' && machine.config?.image===state.image.image
    && !JSON.stringify(machine.image_ref||{}).includes('404f46396aca77324d9b53c55d2824a5401af56ba5362d0bdae67e16108fc245') && machine.name===name
    && machine.config?.metadata?.qualification===state.nonce
    && machine.config?.metadata?.rudder_role===role;
  const remote=async(script,label)=>{
    assert(owned && owned!==forbidden);
    const current=(await inventory()).find(m=>m.id===owned);assert(current&&matches(current)&&(!report.instance||current.instance_id===report.instance),'original exact fixture VM changed');
    return command(['machine','exec',owned,'-a',app,'/bin/bash -c '+quote(script),'--timeout','15'],label,25000);
  };
  try {
    for (const file of sourceFiles) assert.equal(digest(await fs.readFile(path.join(root,file))),state.sourceHashes[file],`source changed: ${file}`);
    const secrets=JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-no-app-secrets'));
    assert.deepEqual(secrets,[]); report.appSecretCount=0;
    assert(!(await inventory()).some(machine=>machine.name===name),'name already exists; no mutation allowed');
    const config={init:{exec:['/bin/sleep','900']},guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},
      env:{},services:[],mounts:[],restart:{policy:'no'},auto_destroy:true,
      metadata:{rudder_role:role,qualification:state.nonce,purpose:'single-server-postgresql-sql-evidence'}};
    const configPath=path.join(state.output,'machine-config.json');
    await fs.writeFile(configPath,JSON.stringify(config,null,2)+'\n');
    report.requestedConfig=config;
    emit({step:'about-to-provision-exact-disposable-name',name,image:state.image.image});
    const intentFile=await fs.open(path.join(state.output,'creation-intent.json'),'wx',0o600);try{await intentFile.writeFile(JSON.stringify({nonce:state.nonce,name,role,image:state.image.image,sourceArchiveSha256:state.sourceArchiveSha256,at:new Date().toISOString()})+'\n');await intentFile.sync();}finally{await intentFile.close();}
    const intentDirectory=await fs.open(state.output,'r');try{await intentDirectory.sync();}finally{await intentDirectory.close();}
    createUncertain=true;let creationFailure;
    try {
      const output=await command(['machine','run',state.image.image,'-a',app,'--machine-config',configPath,
        '--name',name,'--region','iad','--detach','--skip-dns-registration'],'create-owned-disposable',120000);
      owned=/Machine ID:\s*([0-9a-f]+)/.exec(output)?.[1];
    } catch (error) {creationFailure=error;
      for(const [field,file]of [['stdout','create-stdout.private.log'],['stderr','create-stderr.private.log']]){
        const raw=Buffer.from(String(error[field]||''));await fs.writeFile(path.join(state.output,file),raw.subarray(0,65536),{mode:0o600,flag:'wx'});
        report[field+'CreateDiagnostic']={bytes:raw.length,sha256:digest(raw),privateFile:file,truncated:raw.length>65536};}
    }
    if(!owned){const candidates=(await inventory()).filter(matches);if(candidates.length!==1)throw creationFailure||new Error('original create remains uncertain');owned=candidates[0].id;}
    assert(owned && owned!==forbidden);createUncertain=false; report.machine=owned;
    emit({step:'owned-disposable-created',machine:owned,name});
    let machine;
    const bootDeadline=Date.now()+180000;
    for(let polls=0;polls<6&&Date.now()<bootDeadline;polls++) {
      machine=(await inventory()).find(value=>value.id===owned && matches(value));
      assert(machine,'owned identity missing');
      if(machine.state==='started') break;
      await delay(10000);
    }
    assert.equal(machine.state,'started');
    assert.deepEqual(machine.config.init.exec,config.init.exec);
    assert(!machine.config.services?.length && !machine.config.mounts?.length);
    assert(Object.keys(machine.config.env||{}).every(key=>['PRIMARY_REGION','FLY_PRIMARY_REGION'].includes(key)));
    report.instance=machine.instance_id; report.region=machine.region;
    report.resolvedImage=machine.image_ref; report.environmentKeys=Object.keys(machine.config.env||{});
    const address=Object.values(os.networkInterfaces()).flat().find(value=>value?.family==='IPv6' && value.address.startsWith('fdaa:'))?.address;
    assert(address,'private 6PN address unavailable');
    const route='/'+state.nonce+'/source.tar.gz';
    const runtimeRoute='/'+state.nonce+'/runtime.tar.gz';
    const payloads=new Map([[route,bytes],[runtimeRoute,runtimeBytes]]);
    server=http.createServer((req,res)=>{
      if(req.method!=='GET' || !payloads.has(req.url) || req.socket.remoteAddress!==machine.private_ip) {
        res.writeHead(404); res.end(); return;
      }
      const responseBytes=payloads.get(req.url);
      res.writeHead(200,{'Content-Length':responseBytes.length,'Content-Type':'application/octet-stream',Connection:'close'});
      res.end(responseBytes);
    });
    server.requestTimeout=15000;
    await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,address,resolve);});
    const port=server.address().port;
    const marker=state.nonce+':'+owned;
    assert((await remote(`set -e; printf '%s\\n' ${quote(marker)} >/run/rudder-authority-node-qualification; chmod 600 /run/rudder-authority-node-qualification; printf 'MARKER:${state.nonce}\\n'`,'write-owned-nonce-marker')).includes('MARKER:'+state.nonce));
    const download=`set -e; exec 3<>/dev/tcp/${address}/${port}; printf 'GET ${route} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >/var/tmp/rudder-authority-node-source.tar.gz; exec 3<&-; exec 3>&-; printf '%s  %s\\n' ${quote(state.sourceArchiveSha256)} /var/tmp/rudder-authority-node-source.tar.gz | sha256sum -c -; mkdir /var/tmp/rudder-authority-node-source; tar -xzf /var/tmp/rudder-authority-node-source.tar.gz --no-same-owner -C /var/tmp/rudder-authority-node-source; cd /var/tmp/rudder-authority-node-source; sha256sum -c SOURCE-SHA256SUMS; printf 'VERIFIED:${state.nonce}\\n'`;
    assert((await remote(download,'private-source-download-and-sha256')).includes('VERIFIED:'+state.nonce));
    report.fixtureTransport='Private 6PN, exact newly owned source IP plus random nonce route, source archive and each file SHA256 checked; no credentials';
    const runtimeDownload=download.replace(route,runtimeRoute)
      .replaceAll('/var/tmp/rudder-authority-node-source','/var/tmp/rudder-authority-node-runtime')
      .replace(state.sourceArchiveSha256,state.runtime.sha256)
      .replace('sha256sum -c SOURCE-SHA256SUMS;','test -x node;');
    assert((await remote(runtimeDownload,'private-runtime-download-and-sha256')).includes('VERIFIED:'+state.nonce));
    const followup=followupFiles.length?' /var/tmp/rudder-authority-node-runtime/managed-followup.json':'';
    const launch=`(timeout 180 /bin/bash /var/tmp/rudder-authority-node-source/examples/managed-storage-pg/qualify-managed-guest.sh /var/tmp/rudder-authority-node-source /var/tmp/rudder-authority-node-runtime ${quote(state.nonce)} ${quote(owned)}${followup}; result=$?; printf '%s\\n' "$result" >/var/tmp/rudder-authority-node-done) >/var/tmp/rudder-authority-node-output 2>&1 </dev/null & printf 'LAUNCHED:${state.nonce}\\n'`;
    assert((await remote(launch,'launch-contained-postgresql-qualification')).includes('LAUNCHED:'+state.nonce));
    const deadline=Date.now()+210000;
    let finished=false,lastNotice=0;
    await delay(15000);
    for(let polls=0;polls<8&&Date.now()<deadline;polls++) {
      const output=await remote("if test -f /var/tmp/rudder-authority-node-done; then printf FINISHED:; cat /var/tmp/rudder-authority-node-done; else printf PENDING; fi",'poll-owned-qualification');
      if(output.includes('FINISHED:')) {report.guestExitCode=Number(/FINISHED:(\d+)/.exec(output)?.[1]);finished=true;break;}
      if(Date.now()-lastNotice>30000) {emit({step:'postgresql-qualification-running',machine:owned});lastNotice=Date.now();}
      await delay(20000);
    }
    assert(finished,'guest deadline exceeded');
    const encoded=(await remote('base64 -w0 /var/tmp/rudder-authority-node-output','read-public-fixture-output')).trim();
    assert(!encoded || /^[A-Za-z0-9+/]+=*$/.test(encoded));
    await fs.writeFile(path.join(state.output,'guest-output.log'),Buffer.from(encoded,'base64'));
    const artifact=(await remote('if test -f /var/tmp/rudder-managed-storage-pg-evidence.tar.gz; then base64 -w0 /var/tmp/rudder-managed-storage-pg-evidence.tar.gz; fi','read-owned-evidence-archive')).trim();
    if(artifact) {
      assert(/^[A-Za-z0-9+/]+=*$/.test(artifact));
      const artifactPath=path.join(state.output,'guest-evidence.tar.gz');
      const artifactBytes=Buffer.from(artifact,'base64');
      await fs.writeFile(artifactPath,artifactBytes); report.guestEvidenceSha256=digest(artifactBytes);
      const listing=await exec('tar',['-tzf',artifactPath]);
      assert(listing.stdout.split('\n').filter(Boolean).every(entry=>entry.startsWith('./')&&!entry.split('/').includes('..')));
      const guest=path.join(state.output,'guest'); await fs.mkdir(guest,{recursive:true});
      await exec('tar',['-xzf',artifactPath,'-C',guest,'--no-same-owner','--no-same-permissions']);
      try {report.nodeProof=JSON.parse(await fs.readFile(path.join(guest,'node-proof.json'),'utf8'));}
      catch(error) {if(error.code!=='ENOENT') throw error;}
      try {report.guestFailure=JSON.parse(await fs.readFile(path.join(guest,'failure.json'),'utf8'));}
      catch(error) {if(error.code!=='ENOENT') throw error;}
      report.guestCleanup=JSON.parse(await fs.readFile(path.join(guest,'guest-cleanup.json'),'utf8'));
    }
    phase='guest-'+(report.guestFailure?.phase||'completed');
    assert.equal(report.guestExitCode,0,'guest returned failure; see public fixture log');
    assert(report.nodeProof?.ok&&report.nodeProof.scope==='managed-home-v1-v2-v3-real-PG-component-only');
    assert.equal(report.nodeProof.v4MixedQualification,mixedRenewal);
    assert.equal(report.nodeProof.hostTransportPgQualification,hostTransport);
    if(immutableBatches){const b=report.nodeProof.separateFollowup;assert.equal(b?.scope,'real-PG-immutable-batch-retention-component-only');for(const key of ['actualPostgres','frozenMigration','oneStartedAndAckPerBatch','storedAckReconciliation','unknownStartedNoDispatch','expiredOriginalRefused','historicalAckNoClockRenewal','batchPublication','runtimeRlsAndRestrictedGrants','historicalAckCannotContinue','originalImmutableHold','zeroObjectLeaseReads','syntheticPolicyReadback'])assert.equal(b[key],true);for(const key of ['actualS3','realWorkloadRole','physicalRoot','physicalNative','productionEnablement','models','wholeImage300sQualified'])assert.equal(b[key],false);}
    if(coldBatches){const b=report.nodeProof.separateFollowup;assert.equal(b?.scope,'real-PG-cold-image-batch-component-only');for(const key of ['actualPostgres','frozenMigration','oneStartedAndAckPerBatch','storedAckReconciliation','unknownStartedNoDispatch','expiredOriginalRefused','historicalAckNoClockRenewal','batchPublication','runtimeRlsAndRestrictedGrants'])assert.equal(b[key],true);for(const key of ['actualS3','physicalRoot','physicalNative','productionEnablement','models','wholeImage300sQualified'])assert.equal(b[key],false);}
    if(imageReuse){const reused=report.nodeProof.separateFollowup;
      assert.equal(reused?.scope,'real-PG-account-image-reuse-component-only');
      for(const field of ['actualPostgres','actualFabricCas','sameAccount','differentPreparedProject','currentOperationAck','originalPublicationUnchanged','immutableOriginalReplay','wrongPolicyRefused','forgedTenantRefused','frozenStorageRefused','missingPinRefused'])assert.equal(reused[field],true);
      for(const field of ['actualS3','physicalNative','realRootRegistration','newVmPhysicallyQualified','normalRootRetirement','productionEnablement','models'])assert.equal(reused[field],false);
      assert.equal(reused.casPayloadBodyReads,0);assert.equal(reused.warmObjectPuts,0);assert.equal(reused.warmRootReceiptGets,2);
    }
    if(hostTransport){const t=report.nodeProof.separateFollowup;
      assert(t?.actualTransportPgQualification&&t.scope==='actual-PG-host-transport-generation-component-only');
      for(const field of ['actualPostgres','transportMigrationFrozen','restrictedRegistrationRole','tenantRls','actualLoopbackWebSockets','replacement','currentRegistrationRefusal','staleSerialRefused','expiredOriginalRefused','unchangedOriginalAckReconciliation','committedAckLossRetained','authRowLockRevocationSerialized','fixtureClockAdmission'])assert.equal(t[field],true);
      for(const field of ['physicalRoot','physicalNative','actualConfiguredSidecar','actualTransportHmacOnWebSocket','actualS3','qualifiedClockContinuity','normalRootRetirement','productionEnablement'])assert.equal(t[field],false);
    }
    if(hardlinkImage){const h=report.nodeProof.separateFollowup;assert(h&&h.aliases===513&&h.logicalPathBytes>2147483648&&h.retainedUniqueObjectBytes<=2147483648&&h.successfulTargetBodyReads===1&&h.cpuDigestInvocationCountQualified===false);}
    assert(report.nodeProof.realSessionBlockers?.length>=2&&report.nodeProof.verifiedTLS&&report.nodeProof.bootstrapAndOwner&&report.nodeProof.wrongServiceCredentialRefused);
    assert(report.nodeProof.realRootRegistration===false&&report.nodeProof.normalRetirementBlocked===true&&report.nodeProof.normalCleanupPerformed===false);
    assert(report.guestCleanup.cluster_stop_confirmed&&report.guestCleanup.cluster_removed&&report.guestCleanup.normal_runtime_retirement===false&&report.guestCleanup.exact_vm_teardown_required);
    for(const file of sourceFiles) assert.equal(digest(await fs.readFile(path.join(root,file))),state.sourceHashes[file],`source changed during run: ${file}`);
    report.ok=true;
  } catch(error) {
    report.ok=false;report.failedPhase=phase;report.errorCode=error.code||'assertion';
    report.error='qualification failed at '+phase;
    process.exitCode=1;emit({step:'qualification-failed',phase,code:report.errorCode});
  } finally {
    if(server) {server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
    try {
      if(owned){const listed=JSON.parse(await command(['machine','list','-a',app,'--json'],'cleanup-identity'));const actual=listed.find(m=>m.id===owned);
        if(actual){assert(matches(actual)&&(!report.instance||actual.instance_id===report.instance));await command(['machine','destroy',owned,'-a',app,'--force'],'destroy-exact-owned-disposable');}}
      const observations=[];
      for(let check=0;check<2;check++){const values=JSON.parse(await command(['machine','list','-a',app,'--json'],'absence-'+check));
        observations.push({ownedIdPresent:values.some(m=>m.id===owned),exactNoncePresent:values.some(matches)});if(check===0)await delay(500);}
      report.cleanupVerified=!createUncertain&&observations.every(v=>!v.ownedIdPresent&&!v.exactNoncePresent);
      report.cleanup={originalCreateStillUncertain:createUncertain,observations,ownedId:owned||null};
    } catch {report.cleanupVerified=false;process.exitCode=1;}
    report.providerCalls=calls;
    report.verifiedAt=new Date().toISOString();
    await fs.writeFile(path.join(state.output,'report.json'),JSON.stringify(report,null,2)+'\n');
    emit({step:'qualification-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,
      report:path.join(state.output,'report.json')});
  }
}

if(process.argv[2]==='--prepare' && process.argv[3]) await prepare(process.argv[3]);
else if(process.argv[2]==='--run' && process.argv[3]) await run(path.resolve(process.argv[3]));
else throw new Error('usage: qualify-managed-node.mjs --prepare OUTPUT_DIRECTORY | --run STATE_JSON');
