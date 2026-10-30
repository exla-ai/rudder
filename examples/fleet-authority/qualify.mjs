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
const name = process.env.RUDDER_FLEET_SQL_MACHINE_NAME || 'rudder-fleet-sql-20261030-9b68e4';
assert.match(name,/^rudder-fleet-sql-[a-z0-9-]+$/);
const role = 'fleet-sql-qualification';
const forbidden = '8edd225a7d0378';
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const emit = value => console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const sourceFiles = ['docs/architecture/fleet-authority.md','examples/fleet-authority/schema.sql',
  'examples/fleet-authority/qualify.mjs','examples/fleet-authority/qualify-protocol.sql',
  'examples/fleet-authority/qualify-guest.sh'];

async function officialImage() {
  const tokenResult = await fetch('https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/postgres:pull');
  assert(tokenResult.ok);
  const {token} = await tokenResult.json();
  const headers = {Authorization:'Bearer '+token,
    Accept:'application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'};
  const response = await fetch('https://registry-1.docker.io/v2/library/postgres/manifests/17-bookworm',{headers});
  assert(response.ok, 'official image manifest unavailable');
  const index = await response.json();
  const platform = index.manifests?.find(value=>value.platform?.os==='linux' && value.platform?.architecture==='amd64');
  assert.match(platform?.digest || '', /^sha256:[0-9a-f]{64}$/);
  const resolved = await fetch('https://registry-1.docker.io/v2/library/postgres/manifests/'+platform.digest,{headers});
  assert(resolved.ok);
  const bytes = Buffer.from(await resolved.arrayBuffer());
  assert.equal('sha256:'+digest(bytes), platform.digest);
  return {requested:'postgres:17-bookworm',indexDigest:response.headers.get('docker-content-digest'),
    platform:'linux/amd64',manifestDigest:platform.digest,image:'docker.io/library/postgres@'+platform.digest,
    primaryDocumentation:'https://hub.docker.com/_/postgres'};
}

function generatedUpdates(document) {
  const blocks = [...document.matchAll(/```sql\n([\s\S]*?)\n```/g)].map(match=>match[1]);
  assert.equal(blocks.length,2);
  const renew = blocks[0].match(/UPDATE projects SET[\s\S]*?RETURNING owner_epoch, lease_id, renew_serial, lease_until;/)?.[0];
  const integrate = blocks[1].match(/UPDATE projects AS p[\s\S]*?RETURNING integration_version, event_seq;/)?.[0];
  assert(renew && integrate);
  const rewrite = text=>text.replace(/:(\w+)/g,(_,key)=>'p_'+key);
  return `-- Generated without changing either documented UPDATE; parameter names become typed function arguments.\n`+
    `SET search_path=rudder_fleet_design,pg_catalog;\n`+
    `CREATE FUNCTION q_documented_renew(p_a uuid,p_acct text,p_p uuid,p_epoch bigint,p_engine uuid,p_lease uuid,p_expected_serial bigint,p_db_now timestamptz)\n`+
    `RETURNS TABLE(owner_epoch bigint,lease_id uuid,renew_serial bigint,lease_until timestamptz)\n`+
    `LANGUAGE sql VOLATILE SET search_path=rudder_fleet_design,pg_catalog AS $documented$\n${rewrite(renew)}\n$documented$;\n`+
    `CREATE FUNCTION q_documented_integration(p_a uuid,p_acct text,p_p uuid,p_epoch bigint,p_owner_engine uuid,p_lease uuid,p_expected_version bigint,p_old_head text,p_new_head text,p_source_root text,p_environment_root text,p_verification uuid)\n`+
    `RETURNS TABLE(integration_version bigint,event_seq bigint)\n`+
    `LANGUAGE sql VOLATILE SET search_path=rudder_fleet_design,pg_catalog AS $documented$\n${rewrite(integrate)}\n$documented$;\n`;
}

async function prepare(output) {
  output = path.resolve(output);
  await fs.mkdir(output,{recursive:true,mode:0o700});
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
  const statements=generatedUpdates(await fs.readFile(path.join(root,sourceFiles[0]),'utf8'));
  sourceHashes['documented-updates.sql']=digest(statements);
  await fs.writeFile(path.join(stage,'documented-updates.sql'),statements);
  await fs.writeFile(path.join(output,'documented-updates.sql'),statements);
  await fs.writeFile(path.join(stage,'SOURCE-SHA256SUMS'),Object.entries(sourceHashes).map(([file,sha])=>`${sha}  ${file}\n`).join(''));
  await fs.writeFile(path.join(output,'source-hashes.json'),JSON.stringify(sourceHashes,null,2)+'\n');
  const sourceArchive=path.join(output,'source.tar.gz');
  await exec('tar',['-czf',sourceArchive,'-C',stage,...sourceFiles,'documented-updates.sql','SOURCE-SHA256SUMS']);
  await fs.rm(stage,{recursive:true});
  const image=await officialImage();
  const state={format_version:1,name,nonce,role,app,output,sourceHashes,sourceArchive,
    sourceArchiveSha256:digest(await fs.readFile(sourceArchive)),image};
  await fs.writeFile(statePath,JSON.stringify(state,null,2)+'\n',{mode:0o600});
  emit({step:'prepared-no-machine-created',name,image:image.image,state:statePath});
}

async function run(statePath) {
  const state=JSON.parse(await fs.readFile(statePath,'utf8'));
  assert.equal(state.name,name); assert.equal(state.role,role); assert.equal(state.app,app);
  assert.match(state.nonce,/^[0-9a-f-]{36}$/);
  const bytes=await fs.readFile(state.sourceArchive);
  assert.equal(digest(bytes),state.sourceArchiveSha256);
  const report={format_version:1,kind:'postgresql-multisession-qualification',provider:'Fly.io',app,
    name,qualification:state.nonce,startedAt:new Date().toISOString(),image:state.image,
    sourceArchiveSha256:state.sourceArchiveSha256,sourceHashes:state.sourceHashes,steps:[],
    scope:'One disposable PostgreSQL server, synthetic guards and Unix-socket clients; no application/provider HA or RPO qualification'};
  let owned, server, phase='preflight';
  async function command(args,label,timeout=30000) {
    phase=label;
    const began=Date.now();
    const result=await exec(fly,args,{timeout,maxBuffer:8*1024*1024});
    report.steps.push({step:label,ms:Date.now()-began});
    return result.stdout;
  }
  const inventory=async()=>JSON.parse(await command(['machine','list','-a',app,'--json'],'read-machine-inventory'));
  const matches=machine=>machine.id!==forbidden && machine.name===name
    && machine.config?.metadata?.qualification===state.nonce
    && machine.config?.metadata?.rudder_role===role;
  const remote=async(script,label)=>{
    assert(owned && owned!==forbidden);
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
    try {
      const output=await command(['machine','run',state.image.image,'-a',app,'--machine-config',configPath,
        '--name',name,'--region','iad','--detach','--skip-dns-registration'],'create-owned-disposable',120000);
      owned=/Machine ID:\s*([0-9a-f]+)/.exec(output)?.[1];
    } catch (error) {
      // Reconcile an uncertain create by exact name/nonce; never retry creation.
      const candidates=(await inventory()).filter(matches);
      if(candidates.length!==1) throw error;
      owned=candidates[0].id;
    }
    assert(owned && owned!==forbidden); report.machine=owned;
    emit({step:'owned-disposable-created',machine:owned,name});
    let machine;
    const bootDeadline=Date.now()+180000;
    while(Date.now()<bootDeadline) {
      machine=(await inventory()).find(value=>value.id===owned && matches(value));
      assert(machine,'owned identity missing');
      if(machine.state==='started') break;
      await delay(2000);
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
    server=http.createServer((req,res)=>{
      if(req.method!=='GET' || req.url!==route || req.socket.remoteAddress!==machine.private_ip) {
        res.writeHead(404); res.end(); return;
      }
      res.writeHead(200,{'Content-Length':bytes.length,'Content-Type':'application/octet-stream',Connection:'close'});
      res.end(bytes);
    });
    server.requestTimeout=15000;
    await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,address,resolve);});
    const port=server.address().port;
    const marker=state.nonce+':'+owned;
    assert((await remote(`set -e; printf '%s\\n' ${quote(marker)} >/run/rudder-fleet-qualification; chmod 600 /run/rudder-fleet-qualification; printf 'MARKER:${state.nonce}\\n'`,'write-owned-nonce-marker')).includes('MARKER:'+state.nonce));
    const download=`set -e; exec 3<>/dev/tcp/${address}/${port}; printf 'GET ${route} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >/var/tmp/rudder-fleet-source.tar.gz; exec 3<&-; exec 3>&-; printf '%s  %s\\n' ${quote(state.sourceArchiveSha256)} /var/tmp/rudder-fleet-source.tar.gz | sha256sum -c -; mkdir /var/tmp/rudder-fleet-source; tar -xzf /var/tmp/rudder-fleet-source.tar.gz -C /var/tmp/rudder-fleet-source; cd /var/tmp/rudder-fleet-source; sha256sum -c SOURCE-SHA256SUMS; printf 'VERIFIED:${state.nonce}\\n'`;
    assert((await remote(download,'private-source-download-and-sha256')).includes('VERIFIED:'+state.nonce));
    report.fixtureTransport='Private 6PN, exact newly owned source IP plus random nonce route, source archive and each file SHA256 checked; no credentials';
    const launch=`(timeout 180 /bin/bash /var/tmp/rudder-fleet-source/examples/fleet-authority/qualify-guest.sh /var/tmp/rudder-fleet-source ${quote(state.nonce)} ${quote(owned)}; result=$?; printf '%s\\n' "$result" >/var/tmp/rudder-fleet-done) >/var/tmp/rudder-fleet-output 2>&1 </dev/null & printf 'LAUNCHED:${state.nonce}\\n'`;
    assert((await remote(launch,'launch-contained-postgresql-qualification')).includes('LAUNCHED:'+state.nonce));
    const deadline=Date.now()+210000;
    let finished=false,lastNotice=0;
    while(Date.now()<deadline) {
      const output=await remote("if test -f /var/tmp/rudder-fleet-done; then printf FINISHED:; cat /var/tmp/rudder-fleet-done; else printf PENDING; fi",'poll-owned-qualification');
      if(output.includes('FINISHED:')) {report.guestExitCode=Number(/FINISHED:(\d+)/.exec(output)?.[1]);finished=true;break;}
      if(Date.now()-lastNotice>30000) {emit({step:'postgresql-qualification-running',machine:owned});lastNotice=Date.now();}
      await delay(2000);
    }
    assert(finished,'guest deadline exceeded');
    const encoded=(await remote('base64 -w0 /var/tmp/rudder-fleet-output','read-public-fixture-output')).trim();
    assert(encoded.length && /^[A-Za-z0-9+/]+=*$/.test(encoded));
    await fs.writeFile(path.join(state.output,'guest-output.log'),Buffer.from(encoded,'base64'));
    const artifact=(await remote('if test -f /var/tmp/rudder-fleet-evidence.tar.gz; then base64 -w0 /var/tmp/rudder-fleet-evidence.tar.gz; fi','read-owned-evidence-archive')).trim();
    if(artifact) {
      assert(/^[A-Za-z0-9+/]+=*$/.test(artifact));
      const artifactPath=path.join(state.output,'guest-evidence.tar.gz');
      const artifactBytes=Buffer.from(artifact,'base64');
      await fs.writeFile(artifactPath,artifactBytes); report.guestEvidenceSha256=digest(artifactBytes);
      const listing=await exec('tar',['-tzf',artifactPath]);
      assert(listing.stdout.split('\n').filter(Boolean).every(entry=>entry.startsWith('./')&&!entry.split('/').includes('..')));
      const guest=path.join(state.output,'guest'); await fs.mkdir(guest,{recursive:true});
      await exec('tar',['-xzf',artifactPath,'-C',guest,'--no-same-owner','--no-same-permissions']);
      try {report.result=JSON.parse(await fs.readFile(path.join(guest,'result.json'),'utf8'));}
      catch(error) {if(error.code!=='ENOENT') throw error;}
      try {report.guestFailure=JSON.parse(await fs.readFile(path.join(guest,'failure.json'),'utf8'));}
      catch(error) {if(error.code!=='ENOENT') throw error;}
      report.guestCleanup=JSON.parse(await fs.readFile(path.join(guest,'guest-cleanup.json'),'utf8'));
    }
    phase='guest-'+(report.guestFailure?.phase||'completed');
    assert.equal(report.guestExitCode,0,'guest returned failure; see public fixture log');
    assert.equal(report.result?.ok,true); assert(report.result.assertion_count>=30);
    assert.deepEqual(report.guestCleanup,{cluster_removed:true,socket_removed:true,server_stopped:true});
    for(const file of sourceFiles) assert.equal(digest(await fs.readFile(path.join(root,file))),state.sourceHashes[file],`source changed during run: ${file}`);
    report.ok=true;
  } catch(error) {
    report.ok=false;report.failedPhase=phase;report.errorCode=error.code||'assertion';
    report.error='qualification failed at '+phase;
    process.exitCode=1;emit({step:'qualification-failed',phase,code:report.errorCode});
  } finally {
    if(server) {server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
    try {
      for(const machine of (await inventory()).filter(matches)) {
        assert(machine.id!==forbidden);
        await command(['machine','destroy',machine.id,'-a',app,'--force'],'destroy-exact-owned-disposable');
      }
      const remaining=await inventory();
      report.cleanupVerified=!remaining.some(matches)&&!remaining.some(machine=>machine.id===owned);
      report.cleanup={verifiedAt:new Date().toISOString(),ownedId:owned||null,
        matchingNameNonceRoleCount:remaining.filter(matches).length,ownedIdPresent:remaining.some(machine=>machine.id===owned)};
    } catch {report.cleanupVerified=false;process.exitCode=1;}
    report.verifiedAt=new Date().toISOString();
    await fs.writeFile(path.join(state.output,'report.json'),JSON.stringify(report,null,2)+'\n');
    emit({step:'qualification-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,
      report:path.join(state.output,'report.json')});
  }
}

if(process.argv[2]==='--prepare' && process.argv[3]) await prepare(process.argv[3]);
else if(process.argv[2]==='--run' && process.argv[3]) await run(path.resolve(process.argv[3]));
else throw new Error('usage: qualify.mjs --prepare OUTPUT_DIRECTORY | --run STATE_JSON');
