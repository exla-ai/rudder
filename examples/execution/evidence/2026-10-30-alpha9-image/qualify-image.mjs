import fs from 'node:fs/promises';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import assert from 'node:assert/strict';

const exec = promisify(execFile), fly = '/workspace/home/.fly/bin/flyctl';
const app = 'rudder-workers-exla', nonce = randomUUID();
const name = 'rudder-alpha9-boot-' + nonce.slice(0, 8);
const directory = '/workspace/home/.cache/rudder-release-build/alpha9-image-smoke-' + nonce;
const digest = process.argv[2];
assert.match(digest || '', /^sha256:[0-9a-f]{64}$/);
await fs.mkdir(directory, { mode: 0o700 });
const report = { format_version: 1, provider: 'Fly.io', version: '4.0.0-alpha.9',
  revision: '11f1f75e5a18555f6c7e257aacc6b4097f425c5e',
  image: 'registry.fly.io/rudder-workers-exla:4.0.0-alpha.9', expectedDigest: digest,
  nonce, name, steps: [], limits: [
    'The real image bootstrap, cache setup and entrypoint execute. An image-owned-node preload probes and holds before supervisor module evaluation.',
    'No model call, account vault, worker credential, live home or production workspace is used.',
    'Managed execution remains opt-in; this does not prove full backend, fleet or remote-build admission.'
  ] };
const emit = value => console.log(JSON.stringify({ at: new Date().toISOString(), ...value }));
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function command(args, label, timeout = 60000) {
  const start = Date.now();
  const result = await exec(fly, args, { timeout, maxBuffer: 4 * 1024 * 1024 });
  report.steps.push({ step: label, ms: Date.now() - start });
  return result.stdout;
}
async function machines() { return JSON.parse(await command(['machine', 'list', '-a', app, '--json'], 'inventory')); }
const probe = `import fs from 'node:fs';
import cp from 'node:child_process';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
const env = {...process.env}; delete env.NODE_OPTIONS;
const run = (p,a,cwd) => cp.execFileSync(p,a,{env,cwd,encoding:'utf8',timeout:30000,maxBuffer:1024*1024}).trim();
const status=fs.readFileSync('/proc/self/status','utf8');
const field=name=>new RegExp('^'+name+':[ \\t]*([^\\r\\n]*)$','m').exec(status)?.[1]?.trim();
assert.equal(process.getuid(),1500); assert.equal(process.getgid(),1500);
assert.equal(field('Uid'),'1500\\t1500\\t1500\\t1500');
assert.equal(field('Gid'),'1500\\t1500\\t1500\\t1500');
assert.equal(field('Groups'),''); assert.equal(field('NoNewPrivs'),'1');
for(const f of ['CapEff','CapPrm','CapInh','CapAmb','CapBnd']) assert.match(field(f),/^0+$/);
assert.equal(process.env.HOME,'/home/rudder'); assert.equal(process.env.RUDDER_WORKER_FUSE_READY,'1');
assert.throws(()=>process.setuid(0));
assert.equal(fs.statSync('/workspace').uid,1500);
assert.equal(fs.statSync(process.env.RUDDER_CACHE_DIR).uid,1500);
const dev=fs.lstatSync('/dev/fuse'); assert(dev.isCharacterDevice());
assert.equal(dev.uid,1500); assert.equal(dev.gid,1500); assert.equal(dev.mode&0o777,0o600);
const fd=fs.openSync('/dev/fuse','r+');fs.closeSync(fd);
const packageVersion=JSON.parse(fs.readFileSync('/opt/rudder/package.json')).version;
assert.equal(packageVersion,'4.0.0-alpha.9');
const version=run('/usr/local/bin/rudder',['--version']); assert.equal(version,'4.0.0-alpha.9');
const native='/opt/rudder/dist/native/rudder-native', buddy='/opt/rudder/dist/native/rudder-jj';
assert.deepEqual(fs.readdirSync('/opt/rudder/dist/native').sort(),['rudder-jj','rudder-native']);
const capabilities=JSON.parse(run(native,['fabric','capabilities']));
assert.equal(capabilities.authority,'local-only'); assert.equal(capabilities.scheduler,false);
const bridge=JSON.parse(run(buddy,['rudder-capabilities']));
assert.equal(bridge.jj_version,'0.40.0'); assert.equal(bridge.vfs_default,false);
assert.equal(bridge.local_development,false);
assert.equal(run('/usr/local/bin/git',['--version']),'git version 2.56.0');
const root='/workspace/git-smoke'; fs.mkdirSync(root);
const bare=root+'/remote.git',seed=root+'/seed',checkout=root+'/checkout';
run('/usr/local/bin/git',['init','--bare',bare]);run('/usr/local/bin/git',['init',seed]);
run('/usr/local/bin/git',['config','user.name','Rudder disposable image fixture'],seed);
run('/usr/local/bin/git',['config','user.email','fixture@invalid.example'],seed);
fs.writeFileSync(seed+'/file','before');run('/usr/local/bin/git',['add','file'],seed);
run('/usr/local/bin/git',['commit','-m','before'],seed);
run('/usr/local/bin/git',['remote','add','origin',bare],seed);
run('/usr/local/bin/git',['push','origin','HEAD:refs/heads/main'],seed);
run('/usr/local/bin/git',['symbolic-ref','HEAD','refs/heads/main'],bare);
run('/usr/local/bin/jj',['git','clone','--colocate',bare,checkout]);
fs.writeFileSync(seed+'/file','after');run('/usr/local/bin/git',['add','file'],seed);
run('/usr/local/bin/git',['commit','-m','after'],seed);
run('/usr/local/bin/git',['push','origin','HEAD:refs/heads/main'],seed);
run('/usr/local/bin/jj',['git','fetch'],checkout);
assert.equal(run('/usr/local/bin/git',['show','refs/remotes/origin/main:file'],checkout),'after');
const sha=file=>crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const result={kind:'alpha9-full-image-boot-smoke',ok:true,uid:process.getuid(),gid:process.getgid(),
  nnp:field('NoNewPrivs'),zeroCapabilities:true,supplementaryGroups:field('Groups'),
  fuseGranted:true,fuseOpen:true,cacheOwnedByUser:true,rootReentryDenied:true,
  realBootstrapAndEntrypoint:true,supervisorModuleEvaluated:false,packageVersion,version,
  node:process.version,kernel:run('/bin/uname',['-r']),
  claude:run('/home/rudder/.local/bin/claude',['--version']),
  codex:run('/home/rudder/.npm-global/bin/codex',['--version']),
  tailscale:run('/usr/bin/tailscale',['version']).split('\\n')[0],
  jj:run('/usr/local/bin/jj',['--version']),git:run('/usr/local/bin/git',['--version']),
  jjFetchChangedFileRemote:true,capabilities,bridge,
  nativeSha256:sha(native),buddySha256:sha(buddy),verifiedAt:new Date().toISOString()};
fs.writeFileSync('/workspace/alpha9-proof.json',JSON.stringify(result),{mode:0o600,flag:'wx'});
console.log(JSON.stringify(result));
setTimeout(()=>process.exit(0),480000);await new Promise(()=>{});
`;
let machine;
try {
  const secrets = JSON.parse(await command(['secrets','list','-a',app,'--json'],'assert-no-app-secrets'));
  assert.equal(secrets.length,0,'disposable image smoke requires no app secrets');
  const config=path.join(directory,'machine.json');
  await fs.writeFile(config,JSON.stringify({ image: report.image,
    guest:{cpu_kind:'shared',cpus:2,memory_mb:2048},restart:{policy:'no'},auto_destroy:false,
    services:[],env:{RUDDER_WORKER_MANAGED_EXECUTION:'1',RUDDER_HOME:'1',
      RUDDER_WORKER_SKIP_AGENT_UPDATE:'1',NODE_OPTIONS:'--import=data:text/javascript;base64,'+Buffer.from(probe).toString('base64')},
    metadata:{rudder_role:'alpha9-image-boot-smoke',qualification:nonce}
  }),{mode:0o600});
  try { await command(['machine','run',report.image,'-a',app,'--machine-config',config,
    '--name',name,'--region','sjc','--detach','--skip-dns-registration'],'create-owned-smoke',120000); }
  catch(error) {report.createError=String(error.stderr||error.message).slice(-2000);
    emit({step:'create-needs-reconciliation',error:report.createError});
    if(report.createError.includes('MANIFEST_UNKNOWN'))throw new Error('provider rejected image manifest');}
  const deadline=Date.now()+180000;let requestedStart=false;
  while(Date.now()<deadline){
    const owned=(await machines()).filter(m=>m.name===name&&m.config?.metadata?.qualification===nonce);
    assert(owned.length<=1);machine=owned[0];
    if(machine?.state==='started')break;
    if(machine?.state==='created'&&!requestedStart){requestedStart=true;
      try{await command(['machine','start',machine.id,'-a',app],'start-reconciled-owned-smoke',90000);}
      catch(error){report.startError=String(error.stderr||error.message).slice(-2000);}}
    await delay(3000);
  }
  assert.equal(machine?.state,'started');assert.equal(machine.image_ref.digest,digest);
  assert(!machine.config?.mounts?.length&&!machine.config?.services?.length);
  report.machine=machine.id;report.instance=machine.instance_id;report.region=machine.region;
  emit({step:'real-bootstrap-entered',machine:machine.id});
  let result;
  for(let i=0;i<12;i++){
    try {const output=await command(['machine','exec',machine.id,'-a',app,
      '/bin/cat /workspace/alpha9-proof.json','--timeout','10'],'read-bounded-proof',20000);
      result=JSON.parse(output.trim());if(result.ok)break;}
    catch(error){report.lastProbeError=String(error.stderr||error.message).slice(-1500);}
    await delay(3000);
  }
  assert.equal(result?.ok,true,'image boot probe did not return proof');
  report.checks=result;report.ok=true;delete report.lastProbeError;
}catch(failure){report.ok=false;report.error=String(failure.message).slice(0,2000);process.exitCode=1;
  if(machine){try{report.diagnostic=await command(['machine','exec',machine.id,'-a',app,
    '/bin/sh -c "cat /proc/1/status; ls -l /dev/fuse /workspace/alpha9-proof.json"','--timeout','10'],
    'bounded-nonsecret-diagnostic',20000);}catch{}}
}finally{
  try{for(const m of await machines())if(m.name===name&&m.config?.metadata?.qualification===nonce)
    await command(['machine','destroy',m.id,'-a',app,'--force'],'destroy-owned-smoke');
    report.cleanupVerified=!(await machines()).some(m=>m.name===name&&m.config?.metadata?.qualification===nonce);
  }catch{report.cleanupVerified=false;process.exitCode=1;}
  report.verifiedAt=new Date().toISOString();
  await fs.writeFile(path.join(directory,'report.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});
  emit({step:'image-smoke-finished',ok:report.ok,cleanupVerified:report.cleanupVerified,
    report:path.join(directory,'report.json')});
}
