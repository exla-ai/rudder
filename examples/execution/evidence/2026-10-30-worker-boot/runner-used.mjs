import fs from 'node:fs/promises';
import {createReadStream} from 'node:fs';
import path from 'node:path';
import {createHash,randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
const exec=promisify(execFile),fly='/workspace/home/.fly/bin/flyctl',app='rudder-workers-exla';
const base='/workspace/home/.cache/rudder-worker-bootstrap';
const nonce=randomUUID(),name='rudder-worker-boot-'+nonce.slice(0,18),role='worker-boot-qualification';
const directory=path.join(base,'runs',nonce);await fs.mkdir(directory,{recursive:true,mode:0o700});
const machineConfig=path.join(directory,'machine.json');
await fs.writeFile(machineConfig,JSON.stringify({image:'debian:bookworm-slim',init:{exec:['/bin/sleep','1200']},guest:{cpu_kind:'performance',cpus:1,memory_mb:2048},restart:{policy:'no'},auto_destroy:true,services:[],mounts:[],env:{},metadata:{rudder_role:role,qualification:nonce}},null,2),{mode:0o600});
const state={name,nonce,role,directory,machineConfig};await fs.writeFile(path.join(base,'latest-state.json'),JSON.stringify(state,null,2)+'\n',{mode:0o600});
const report={format_version:1,kind:'worker-boot-readiness',provider:'Fly.io',app,name,qualification:nonce,role,limits:{cpu_kind:'performance',cpus:1,memory_mb:2048,maxLifetimeSeconds:1200,services:0,volumes:0,productionEnv:false},steps:[]};
const emit=value=>console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const delay=ms=>new Promise(r=>setTimeout(r,ms));
const q=s=>"'"+s.replaceAll("'","'\\''")+"'";
const parentNamespace=await fs.readlink('/proc/self/ns/mnt');
const safe=e=>String(e?.stderr||e?.message||e).replace(/Bearer\s+\S+/gi,'Bearer [redacted]').replace(/(?:FLY_API_TOKEN|API_KEY|TOKEN)=\S+/g,'[redacted]').replaceAll('/workspace/home/.fly','[private-provider-profile]').slice(-4000);
let owned,server,phase='preflight';
async function command(args,label,timeout=25000){phase=label;const began=Date.now();try{const r=await exec(fly,args,{cwd:directory,timeout,maxBuffer:8*1024*1024});report.steps.push({step:label,ms:Date.now()-began,ok:true});return r.stdout;}catch(error){report.steps.push({step:label,ms:Date.now()-began,ok:false});throw error;}}
async function machines(){const result=JSON.parse(await command(['machine','list','-a',app,'--json'],'machine-inventory'));assert(Array.isArray(result));return result;}
const matches=m=>m.name===name&&m.config?.metadata?.qualification===nonce&&m.config?.metadata?.rudder_role===role;
async function remote(commandText,label){const out=await command(['machine','exec',owned,'-a',app,commandText,'--timeout','15'],label,25000);try{const result=JSON.parse(out);assert(!result?.error,'provider returned a command error');}catch(error){if(error?.code==='ERR_ASSERTION')throw error;}return out;}
function verifyConfiguration(machine){assert(matches(machine));const c=machine.config;assert(c);assert.deepEqual(c.init?.exec,['/bin/sleep','1200']);assert(!c.services?.length&&!c.mounts?.length);assert.equal(Object.keys(c.env||{}).length,0);assert.equal(c.guest?.cpu_kind,'performance');assert.equal(c.guest?.cpus,1);assert.equal(c.guest?.memory_mb,2048);}
async function sha(file){const h=createHash('sha256');for await(const bytes of createReadStream(file))h.update(bytes);return h.digest('hex');}
try{
 assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-no-app-secrets')),[]);
 const manifest=JSON.parse(await fs.readFile(path.join(base,'source-checksums.json'),'utf8'));assert.equal(manifest.kind,'worker-boot-readiness');
 for(const file of ['controller.gz','native.gz','boot-fixture.tar.gz'])assert.equal(await sha(path.join(base,file)),manifest.artifacts[file].sha256);
 report.artifacts=manifest;report.manifestSha256=await sha(path.join(base,'source-checksums.json'));
 try{await command(['machine','run','debian:bookworm-slim','-a',app,'--machine-config',machineConfig,'--name',name,'--region','sjc','--detach','--skip-dns-registration'],'create-disposable-canary',60000);}catch(error){report.createReturnedError=true;report.createDiagnostic=safe(error).slice(-500);}
 let candidates=(await machines()).filter(matches);assert.equal(candidates.length,1,'create did not produce exactly one owned canary');let machine=candidates[0];owned=machine.id;verifyConfiguration(machine);report.machine=owned;emit({step:'owned-disposable-canary',machine:owned,state:machine.state});
 if(['created','stopped'].includes(machine.state)){await command(['machine','start',owned,'-a',app],'explicit-start-created-canary',60000);report.explicitStartOnce=true;}
 const boot=Date.now()+180000;while(Date.now()<boot){machine=(await machines()).find(m=>m.id===owned&&matches(m));assert(machine,'owned canary disappeared before boot');verifyConfiguration(machine);if(machine.state==='started')break;await delay(3000);}assert.equal(machine.state,'started','canary boot deadline');
 report.instance=machine.instance_id;report.region=machine.region;report.image=machine.image_ref;report.guest=machine.config.guest;assert.equal(machine.region,'sjc');
 const address=Object.values(os.networkInterfaces()).flat().find(x=>x?.family==='IPv6'&&x.address.startsWith('fdaa:'))?.address;assert(address,'private Fly address unavailable');assert(typeof machine.private_ip==='string'&&machine.private_ip.startsWith('fdaa:'));
 const routes=new Map(['controller.gz','native.gz','boot-fixture.tar.gz'].map(file=>['/'+nonce+'/'+file,{file:path.join(base,file),size:manifest.artifacts[file].size_bytes}]));
 server=http.createServer((req,res)=>{const route=routes.get(req.url);if(req.method!=='GET'||req.socket.remoteAddress!==machine.private_ip||!route){res.writeHead(404);res.end();return;}res.writeHead(200,{'Content-Length':route.size,'Content-Type':'application/octet-stream','Connection':'close'});const stream=createReadStream(route.file);stream.on('error',()=>res.destroy());res.on('close',()=>stream.destroy());stream.pipe(res);});
 server.requestTimeout=30000;server.headersTimeout=10000;await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,address,resolve);});const port=server.address().port;
 const markerReady=await remote('/bin/sh -c '+q(`set -e; mkdir -p /workspace/boot-artifacts; printf '%s\\n' ${q(nonce+':'+owned)} >/run/rudder-worker-bootstrap-qualification; chmod 0600 /run/rudder-worker-bootstrap-qualification; echo MARKER:${nonce}`),'prepare-owned-marker');assert(markerReady.includes('MARKER:'+nonce));
 for(const [file,dest]of[['controller.gz','/workspace/boot-artifacts/controller.gz'],['native.gz','/workspace/boot-artifacts/native.gz'],['boot-fixture.tar.gz','/workspace/boot-artifacts/boot-fixture.tar.gz']]){
  const marker='FETCHED:'+nonce+':'+file;
  const script=`set -e; exec 3<>/dev/tcp/${address}/${port}; printf 'GET /${nonce}/${file} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >${dest}; exec 3<&-; exec 3>&-; printf '%s\\n' ${q(marker)}`;
  const out=await remote('/bin/bash -c '+q(script),'download-private-'+file);assert(out.includes(marker),'missing download acknowledgement');
 }
 const expected=[['controller.gz','/workspace/boot-artifacts/controller.gz'],['native.gz','/workspace/boot-artifacts/native.gz'],['boot-fixture.tar.gz','/workspace/boot-artifacts/boot-fixture.tar.gz']].map(([f,d])=>manifest.artifacts[f].sha256+'  '+d).join('\n');
 const verified=await remote('/bin/sh -c '+q(`set -e; printf '%s\\n' ${q(expected)} | sha256sum -c -; tar -xOf /workspace/boot-artifacts/boot-fixture.tar.gz guest-canary.sh >/workspace/boot-artifacts/guest-canary.sh; printf '%s  %s\\n' ${q(manifest.artifacts['guest-canary.sh'].sha256)} /workspace/boot-artifacts/guest-canary.sh | sha256sum -c -; printf '%s\\n' ${q('VERIFIED:'+nonce)}`),'verify-three-payload-sha256');assert(verified.includes('VERIFIED:'+nonce)&&['/workspace/boot-artifacts/controller.gz: OK','/workspace/boot-artifacts/native.gz: OK','/workspace/boot-artifacts/boot-fixture.tar.gz: OK','/workspace/boot-artifacts/guest-canary.sh: OK'].every(x=>verified.includes(x)));
 report.fixtureTransport='private Fly6PN HTTP; exact owned VM source+nonce; three SHA256 checks before exec; no credentials';
 const launched=await remote('/bin/sh -c '+q(`(RUDDER_BOOTSTRAP_QUALIFICATION_NONCE=${q(nonce)} RUDDER_BOOTSTRAP_OWNED_MACHINE=${q(owned)} timeout 900 /bin/sh /workspace/boot-artifacts/guest-canary.sh; result=$?; echo "$result" >/tmp/execution-rpc-done) >/tmp/execution-rpc-output 2>&1 </dev/null & echo LAUNCHED:${nonce}`),'launch-contained-qualification');assert(launched.includes('LAUNCHED:'+nonce));
 const deadline=Date.now()+930000;let finished=false,lastNotice=0;
 while(Date.now()<deadline){const out=await remote("/bin/sh -c 'if test -f /tmp/execution-rpc-done; then printf FINISHED:; cat /tmp/execution-rpc-done; else printf PENDING; fi'",'poll-contained-qualification');if(out.includes('FINISHED:')){report.exitCode=Number(/FINISHED:(\d+)/.exec(out)?.[1]);finished=true;break;}if(Date.now()-lastNotice>30000){emit({step:'workspace-qualification-running',machine:owned});lastNotice=Date.now();}await delay(3000);}assert(finished,'qualification deadline exceeded');
 const encoded=(await remote('/usr/bin/base64 -w0 /tmp/execution-rpc-output','read-owned-report')).trim();assert(encoded.length&&/^[A-Za-z0-9+/]+=*$/.test(encoded),'missing/invalid report payload');const output=Buffer.from(encoded,'base64').toString('utf8');await fs.writeFile(path.join(directory,'mounted-output.log'),output,{mode:0o600});report.results=output.split('\n').filter(x=>x.startsWith('{')).map(line=>JSON.parse(line));assert.equal(report.exitCode,0,output.slice(-4000));
 const matrix=report.results.find(x=>x.kind==='worker-bootstrap-matrix');assert(matrix,'missing actual boot matrix');assert(matrix.cases.length>=19);assert.equal(matrix.workspaceMountTopOnly,true);assert.equal(matrix.existingChildInodesPreserved,true);assert.equal(matrix.defaultManagedLaunchChanged,false);
 const tools=report.results.find(x=>x.kind==='worker-bootstrap-tools');assert.equal(tools?.git,'git version 2.56.0');assert(tools?.jj.startsWith('jj 0.40.0'));
 const proof=report.results.find(x=>x.kind==='workspace-observation-rpc');assert(proof&&proof.uid===1500&&proof.gid===1500&&proof.records.length===4);assert.equal(proof.parent_mount_namespace_unchanged,true);assert.equal(proof.records[0].guest_proof.git.jj_fetch_changed_file_remote,true);assert.equal(proof.records[0].guest_proof.git.git,'git version 2.56.0');report.ok=true;
}catch(error){report.ok=false;report.failedPhase=phase;report.error=safe(error);process.exitCode=1;emit({step:'qualification-failed',phase,error:report.error});}
finally{
 if(server){server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
 try{let list=await machines();for(const m of list.filter(matches)){await command(['machine','destroy',m.id,'-a',app,'--force'],'destroy-owned-canary',60000);}const remaining=await machines();report.cleanupVerified=!remaining.some(matches);if(!report.cleanupVerified)process.exitCode=1;}catch(error){report.cleanupVerified=false;report.cleanupError=safe(error);process.exitCode=1;}
 report.parentHostMountNamespaceUnchanged=(await fs.readlink('/proc/self/ns/mnt'))===parentNamespace;assert(report.parentHostMountNamespaceUnchanged);report.verifiedAt=new Date().toISOString();await fs.writeFile(path.join(directory,'report.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});emit({step:'qualification-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,report:path.join(directory,'report.json')});
}
