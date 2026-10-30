import fs from 'node:fs/promises';
import {createReadStream} from 'node:fs';
import path from 'node:path';
import {createHash,randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
const exec=promisify(execFile),fly=process.env.RUDDER_FLYCTL||'flyctl',app=process.env.RUDDER_NATIVE_VCS_FLY_APP;
assert.equal(app,'rudder-workers-exla','explicit authorized disposable app required');
const base=process.env.RUDDER_NATIVE_VCS_CANARY_DIR;
assert(base,'explicit frozen artifact directory required');
const nonce=randomUUID(),name='rudder-native-vcs-'+nonce.slice(0,18),role='native-vcs-rpc-qualification';
const directory=path.join(base,'runs',nonce);await fs.mkdir(directory,{recursive:true,mode:0o700});
const machineConfig=path.join(directory,'machine.json');
await fs.writeFile(machineConfig,JSON.stringify({image:'debian:bookworm-slim',init:{exec:['/bin/sleep','1200']},guest:{cpu_kind:'shared',cpus:1,memory_mb:2048},restart:{policy:'no'},auto_destroy:true,services:[],mounts:[],env:{},metadata:{rudder_role:role,qualification:nonce}},null,2),{mode:0o600});
const state={name,nonce,role,directory,machineConfig};await fs.writeFile(path.join(base,'latest-state.json'),JSON.stringify(state,null,2)+'\n',{mode:0o600});
const report={format_version:1,kind:'mounted-native-jj-rpc-consumer',provider:'Fly.io',app,name,qualification:nonce,role,limits:{cpus:1,memory_mb:2048,maxLifetimeSeconds:1200,services:0,volumes:0,productionEnv:false},steps:[]};
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
function verifyConfiguration(machine){assert(matches(machine));const c=machine.config;assert(c);assert.deepEqual(c.init?.exec,['/bin/sleep','1200']);assert(!c.services?.length&&!c.mounts?.length);assert.equal(Object.keys(c.env||{}).length,0);assert.equal(c.guest?.cpu_kind,'shared');assert.equal(c.guest?.cpus,1);assert.equal(c.guest?.memory_mb,2048);}
async function sha(file){const h=createHash('sha256');for await(const bytes of createReadStream(file))h.update(bytes);return h.digest('hex');}
try{
 assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-no-app-secrets')),[]);
 const manifest=JSON.parse(await fs.readFile(path.join(base,'native-vcs-source-checksums.json'),'utf8'));assert.equal(manifest.kind,'mounted-native-jj-rpc-consumer');
 for(const file of ['controller.gz','jj.gz','native.gz','guest-canary.sh'])assert.equal(await sha(path.join(base,file)),manifest.artifacts[file].sha256);
 report.artifacts=manifest;report.manifestSha256=await sha(path.join(base,'native-vcs-source-checksums.json'));
 try{await command(['machine','run','debian:bookworm-slim','-a',app,'--machine-config',machineConfig,'--name',name,'--region','sjc','--detach','--skip-dns-registration'],'create-disposable-canary',60000);}catch(error){report.createReturnedError=true;report.createDiagnostic=safe(error).slice(-500);}
 let candidates=(await machines()).filter(matches);assert.equal(candidates.length,1,'create did not produce exactly one owned canary');let machine=candidates[0];owned=machine.id;verifyConfiguration(machine);report.machine=owned;emit({step:'owned-disposable-canary',machine:owned,state:machine.state});
 if(['created','stopped'].includes(machine.state)){await command(['machine','start',owned,'-a',app],'explicit-start-created-canary',60000);report.explicitStartOnce=true;}
 const boot=Date.now()+180000;while(Date.now()<boot){machine=(await machines()).find(m=>m.id===owned&&matches(m));assert(machine,'owned canary disappeared before boot');verifyConfiguration(machine);if(machine.state==='started')break;await delay(3000);}assert.equal(machine.state,'started','canary boot deadline');
 report.instance=machine.instance_id;report.region=machine.region;report.image=machine.image_ref;report.guest=machine.config.guest;assert.equal(machine.region,'sjc');
 const address=Object.values(os.networkInterfaces()).flat().find(x=>x?.family==='IPv6'&&x.address.startsWith('fdaa:'))?.address;assert(address,'private Fly address unavailable');assert(typeof machine.private_ip==='string'&&machine.private_ip.startsWith('fdaa:'));
 const routes=new Map(['controller.gz','jj.gz','native.gz','guest-canary.sh'].map(file=>['/'+nonce+'/'+file,{file:path.join(base,file),size:manifest.artifacts[file].size_bytes}]));
 server=http.createServer((req,res)=>{const route=routes.get(req.url);if(req.method!=='GET'||req.socket.remoteAddress!==machine.private_ip||!route){res.writeHead(404);res.end();return;}res.writeHead(200,{'Content-Length':route.size,'Content-Type':'application/octet-stream','Connection':'close'});const stream=createReadStream(route.file);stream.on('error',()=>res.destroy());res.on('close',()=>stream.destroy());stream.pipe(res);});
 server.requestTimeout=30000;server.headersTimeout=10000;await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,address,resolve);});const port=server.address().port;
 for(const [file,dest]of[['controller.gz','/tmp/controller.gz'],['jj.gz','/tmp/jj.gz'],['native.gz','/tmp/native.gz'],['guest-canary.sh','/tmp/vcs-rpc-canary.sh']]){
  const marker='FETCHED:'+nonce+':'+file;
  const script=`set -e; exec 3<>/dev/tcp/${address}/${port}; printf 'GET /${nonce}/${file} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >${dest}; exec 3<&-; exec 3>&-; printf '%s\\n' ${q(marker)}`;
  const out=await remote('/bin/bash -c '+q(script),'download-private-'+file);assert(out.includes(marker),'missing download acknowledgement');
 }
 const expected=[['controller.gz','/tmp/controller.gz'],['jj.gz','/tmp/jj.gz'],['native.gz','/tmp/native.gz'],['guest-canary.sh','/tmp/vcs-rpc-canary.sh']].map(([f,d])=>manifest.artifacts[f].sha256+'  '+d).join('\n');
 const verified=await remote('/bin/sh -c '+q(`set -e; printf '%s\\n' ${q(expected)} | sha256sum -c -; printf '%s\\n' ${q('VERIFIED:'+nonce)}`),'verify-four-payload-sha256');assert(verified.includes('VERIFIED:'+nonce)&&['/tmp/controller.gz: OK','/tmp/jj.gz: OK','/tmp/native.gz: OK','/tmp/vcs-rpc-canary.sh: OK'].every(x=>verified.includes(x)));
 report.fixtureTransport='private Fly6PN HTTP; exact owned VM source+nonce; four SHA256 checks before exec; no credentials';
 const launched=await remote('/bin/sh -c '+q(`(timeout 600 /bin/sh /tmp/vcs-rpc-canary.sh; result=$?; echo "$result" >/tmp/vcs-rpc-done) >/tmp/vcs-rpc-output 2>&1 </dev/null & echo LAUNCHED:${nonce}`),'launch-contained-qualification');assert(launched.includes('LAUNCHED:'+nonce));
 const deadline=Date.now()+630000;let finished=false,lastNotice=0;
 while(Date.now()<deadline){const out=await remote("/bin/sh -c 'if test -f /tmp/vcs-rpc-done; then printf FINISHED:; cat /tmp/vcs-rpc-done; else printf PENDING; fi'",'poll-contained-qualification');if(out.includes('FINISHED:')){report.exitCode=Number(/FINISHED:(\d+)/.exec(out)?.[1]);finished=true;break;}if(Date.now()-lastNotice>30000){emit({step:'workspace-qualification-running',machine:owned});lastNotice=Date.now();}await delay(3000);}assert(finished,'qualification deadline exceeded');
 const encoded=(await remote('/usr/bin/base64 -w0 /tmp/vcs-rpc-output','read-owned-report')).trim();assert(encoded.length&&/^[A-Za-z0-9+/]+=*$/.test(encoded),'missing/invalid report payload');const output=Buffer.from(encoded,'base64').toString('utf8');await fs.writeFile(path.join(directory,'mounted-output.log'),output,{mode:0o600});report.results=output.split('\n').filter(x=>x.startsWith('{')).map(line=>JSON.parse(line));assert.equal(report.exitCode,0,output.slice(-4000));
 const proof=report.results.find(x=>x.kind==='mounted-native-jj-rpc-consumer');assert(proof,'missing actual native v3 consumer proof');assert.equal(proof.uid,1500);assert.equal(proof.gid,1500);assert.equal(proof.parent_mount_namespace_unchanged,true);assert.equal(proof.production_default_changed,false);assert.equal(proof.native_broker_qualified,true);assert.equal(proof.explicit_protected_v3_factory,true);assert.deepEqual(proof.records.map(r=>r.mode),['live','epoch']);const [live,stale]=proof.records;assert.equal(live.phase,'checkpointed');assert.equal(live.guest.same_tree_edit,true);assert.equal(live.guest.same_open_fd_across_observations,true);assert.equal(live.guest.offline_version_help,true);assert.equal(live.latest_jj_source_number,4);assert.equal(live.terminal_checkpoint_source_number,5);assert.equal(live.native_shutdown_guard_then_checkpoint,true);assert.equal(stale.phase,'orphaned');assert.equal(stale.no_checkpoint_on_actual_journal_epoch_change,true);for(const record of proof.records){assert.equal(record.all_pidfds_exited,true);assert.equal(record.fixture_authority_used,false);assert.equal(record.actual_native_factory,true);assert.equal(record.execution_spec_version,3);assert.equal(record.manual_fixture_command_shutdown,false);}report.ok=true;
}catch(error){report.ok=false;report.failedPhase=phase;report.error=safe(error);process.exitCode=1;emit({step:'qualification-failed',phase,error:report.error});}
finally{
 if(server){server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
 try{let list=await machines();for(const m of list.filter(matches)){await command(['machine','destroy',m.id,'-a',app,'--force'],'destroy-owned-canary',60000);}const remaining=await machines();report.cleanupVerified=!remaining.some(matches);if(!report.cleanupVerified)process.exitCode=1;}catch(error){report.cleanupVerified=false;report.cleanupError=safe(error);process.exitCode=1;}
 report.parentHostMountNamespaceUnchanged=(await fs.readlink('/proc/self/ns/mnt'))===parentNamespace;assert(report.parentHostMountNamespaceUnchanged);report.verifiedAt=new Date().toISOString();await fs.writeFile(path.join(directory,'report.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});emit({step:'qualification-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,report:path.join(directory,'report.json')});
}
