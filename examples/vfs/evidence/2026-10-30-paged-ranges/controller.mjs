import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import assert from 'node:assert/strict';
import http from 'node:http';
import os from 'node:os';
const exec=promisify(execFile),fly='/workspace/home/.fly/bin/flyctl',app='rudder-workers-exla';
const state=JSON.parse(await fs.readFile('/workspace/home/.cache/rudder-vfs-paged-qualification/current.json','utf8'));
const report={format_version:1,provider:'Fly.io',data_plane:'paged_ranges_v2',name:state.name,qualification:state.nonce,steps:[]};
const emit=value=>console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const delay=ms=>new Promise(r=>setTimeout(r,ms));
const q=s=>"'"+s.replaceAll("'","'\\''")+"'";
let owned,server,phase='preflight';
async function command(args,label,timeout=60000){phase=label;const started=Date.now();const r=await exec(fly,args,{timeout,maxBuffer:8*1024*1024});report.steps.push({step:label,ms:Date.now()-started});return r.stdout;}
async function machines(){return JSON.parse(await command(['machine','list','-a',app,'--json'],'machine-inventory'));}
async function remote(cmd,label){return command(['machine','exec',owned,'-a',app,cmd,'--timeout','15'],label,25000);}
try{
 assert.deepEqual(JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-no-app-secrets')),[]);
 try{
  const out=await command(['machine','run','debian:bookworm-slim','-a',app,'--machine-config',state.machineConfig,'--name',state.name,'--region','iad','--detach','--skip-dns-registration'],'create-disposable-canary',120000);
  owned=/Machine ID:\s*([0-9a-f]+)/.exec(out)?.[1];
 }catch(failure){const matches=(await machines()).filter(m=>m.name===state.name&&m.config?.metadata?.qualification===state.nonce);if(matches.length!==1)throw failure;owned=matches[0].id;}
 assert(owned,'no owned machine identity');report.machine=owned;emit({step:'created-disposable-canary',machine:owned});
 let machine;const bootDeadline=Date.now()+180000;
 while(Date.now()<bootDeadline){machine=(await machines()).find(m=>m.id===owned&&m.name===state.name&&m.config?.metadata?.qualification===state.nonce);assert(machine);if(machine.state==='started')break;await delay(3000);}
 assert.equal(machine.state,'started');assert(!machine.config?.services?.length&&!machine.config?.mounts?.length);assert(Object.keys(machine.config?.env||{}).every(k=>['PRIMARY_REGION','FLY_PRIMARY_REGION'].includes(k)));
 report.instance=machine.instance_id;report.region=machine.region;report.image=machine.image_ref;report.guest=machine.config.guest;
 emit({step:'awaiting-frozen-driver',machine:owned});
 let ready;const driverDeadline=Date.now()+180000;
 while(Date.now()<driverDeadline){try{ready=JSON.parse(await fs.readFile(path.join(state.directory,'ready-driver.json'),'utf8'));break;}catch(e){if(e.code!=='ENOENT')throw e;}await delay(1000);}
 assert(ready,'driver handoff deadline exceeded');
 const gzip=await fs.readFile(ready.gzipPath);assert.equal(createHash('sha256').update(gzip).digest('hex'),ready.gzipSha256);report.artifacts=ready;
 // The SSH credential cannot create a new org tunnel. Serve ONLY these two
 // non-secret fixtures on this host's private Fly 6PN address, gated to the
 // exact newly-created VM and nonce. Nothing is exposed on a public service.
 const address=Object.values(os.networkInterfaces()).flat().find(x=>x?.family==='IPv6'&&x.address.startsWith('fdaa:'))?.address;
 assert(address,'private Fly network address unavailable');
 const guest=await fs.readFile(path.join(state.directory,'guest-canary.sh'));
 const routes=new Map([['/'+state.nonce+'/driver.gz',gzip],['/'+state.nonce+'/guest.sh',guest]]);
 server=http.createServer((req,res)=>{const bytes=routes.get(req.url);if(req.method!=='GET'||req.socket.remoteAddress!==machine.private_ip||!bytes){res.writeHead(404);res.end();return;}res.writeHead(200,{'Content-Length':bytes.length,'Content-Type':'application/octet-stream','Connection':'close'});res.end(bytes);});
 server.requestTimeout=30000;await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,address,resolve);});const port=server.address().port;
 const download=(route,destination)=>`exec 3<>/dev/tcp/${address}/${port}; printf 'GET ${route} HTTP/1.0\\r\\nHost: fixture\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; [[ "$status" == 'HTTP/1.1 200 '* || "$status" == 'HTTP/1.0 200 '* ]] || exit 24; while IFS= read -r line <&3; do [[ "$line" == $'\\r' ]] && break; done; cat <&3 >${destination}; exec 3<&-; exec 3>&-`;
 for(const [route,destination]of[['/'+state.nonce+'/driver.gz','/tmp/vfs-driver.gz'],['/'+state.nonce+'/guest.sh','/tmp/vfs-canary.sh']]){const result=await remote('/bin/bash -c '+q(download(route,destination)),'private-network-fixture-download');void result;}
 const verification=await remote('/bin/sh -c '+q("printf '%s  %s\\n' '"+ready.gzipSha256+"' /tmp/vfs-driver.gz | sha256sum -c -"),'verify-downloaded-fixture');assert(verification.includes('OK'));
 report.fixtureTransport='private Fly6PN HTTP; exact VM source/nonce; compressed artifact SHA256 verified before execution; no credentials';
 const script='(timeout 600 /bin/sh /tmp/vfs-canary.sh; result=$?; echo "$result" >/tmp/vfs-done) >/tmp/vfs-output 2>&1 </dev/null & echo LAUNCHED';
 const acknowledged=await remote('/bin/sh -c '+q(script),'launch-contained-qualification');assert(acknowledged.includes('LAUNCHED'));
 const deadline=Date.now()+630000;let finished=false,lastNotice=0;
 while(Date.now()<deadline){const out=await remote("/bin/sh -c 'if test -f /tmp/vfs-done; then printf FINISHED:; cat /tmp/vfs-done; else printf PENDING; fi'",'poll-contained-qualification');if(out.includes('FINISHED:')){report.exitCode=Number(/FINISHED:(\d+)/.exec(out)?.[1]);finished=true;break;}if(Date.now()-lastNotice>30000){emit({step:'mounted-qualification-running',machine:owned});lastNotice=Date.now();}await delay(3000);}
 assert(finished,'qualification deadline exceeded');
 const encoded=(await remote('/usr/bin/base64 -w0 /tmp/vfs-output','read-owned-report')).trim();assert(encoded.length&&/^[A-Za-z0-9+/]+=*$/.test(encoded));const output=Buffer.from(encoded,'base64').toString('utf8');await fs.writeFile(path.join(state.directory,'mounted-output.log'),output);report.results=output.split('\n').filter(line=>line.startsWith('{')).map(line=>JSON.parse(line));assert.equal(report.exitCode,0,output.slice(-5000));assert(report.results.length>=3);report.ok=true;
}catch(error){report.ok=false;report.failedPhase=phase;report.error=String(error.stderr||error.message).slice(-5000);process.exitCode=1;emit({step:'qualification-failed',phase});}
finally{
 if(server){server.closeAllConnections();await new Promise(r=>server.close(r));}
 try{const list=await machines();for(const m of list)if(m.name===state.name&&m.config?.metadata?.qualification===state.nonce){owned=m.id;await command(['machine','destroy',m.id,'-a',app,'--force'],'destroy-owned-canary',60000);}const remaining=await machines();report.cleanupVerified=!remaining.some(m=>m.name===state.name&&m.config?.metadata?.qualification===state.nonce);}catch{report.cleanupVerified=false;process.exitCode=1;}
 report.verifiedAt=new Date().toISOString();await fs.writeFile(path.join(state.directory,'report.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});emit({step:'qualification-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,report:path.join(state.directory,'report.json')});
}
