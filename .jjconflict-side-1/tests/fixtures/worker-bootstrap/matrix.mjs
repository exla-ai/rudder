// This deliberately changes device metadata ONLY on the nonce-marked disposable
// VM selected by the guarded provider runner. Never run it on a workstation.
import fs from 'node:fs';import cp from 'node:child_process';import assert from 'node:assert/strict';
assert.equal(process.getuid(),0);
const marker=fs.readFileSync('/run/rudder-worker-bootstrap-qualification','utf8').trim();
assert.equal(marker,process.env.RUDDER_BOOTSTRAP_QUALIFICATION_NONCE+':'+process.env.RUDDER_BOOTSTRAP_OWNED_MACHINE);
assert(/^[0-9a-f-]{36}:[0-9a-f]{14,20}$/.test(marker));
const root='/workspace/bootstrap-cases';fs.mkdirSync(root,{recursive:true});fs.chownSync(root,1500,1500);
const run=(program,args)=>{const r=cp.spawnSync(program,args,{encoding:'utf8',timeout:20000});assert.equal(r.status,0,(r.stderr||r.stdout).slice(-2000));return r.stdout;};
const script=(file,text)=>{fs.writeFileSync(file,text,{mode:0o755});fs.chmodSync(file,0o755);};
for(const [uid,name]of[[1500,'rudder'],[2000,'byovm']]){
 try{run('/usr/bin/getent',['passwd',name]);}catch{run('/usr/sbin/groupadd',['--gid',String(uid),name]);run('/usr/sbin/useradd',['--uid',String(uid),'--gid',String(uid),'--create-home','--shell','/bin/bash',name]);}
 fs.mkdirSync('/home/'+name+'/.local/bin',{recursive:true});run('/usr/bin/chown',['-R',`${uid}:${uid}`,'/home/'+name]);
 const bin='/home/'+name+'/.local/bin';
 for(const tool of ['claude','npm','codex'])script(bin+'/'+tool,'#!/bin/sh\nexec /usr/bin/node /opt/rudder-worker/update-probe.mjs '+tool+' "$@"\n');
 script(bin+'/node','#!/bin/sh\nexec /usr/bin/node "$@"\n');
 for(const tool of ['id','stat','chown','chmod','setpriv'])script(bin+'/'+tool,'#!/bin/sh\nprintf bad-utility:'+tool+' >>/workspace/bootstrap-path-misuse\nexit 97\n');
 const credential='/home/'+name+'/fixture-credential';fs.writeFileSync(credential,'public disposable fixture '+uid,{mode:0o600});fs.chownSync(credential,uid,uid);
}
const records=[];
const sentinels='/workspace/ownership-fixture';fs.mkdirSync(sentinels);fs.mkdirSync(sentinels+'/nested');fs.writeFileSync(sentinels+'/nested/data','existing child bytes',{mode:0o440});fs.chownSync(sentinels+'/nested/data',2222,2222);fs.symlinkSync('nested/data',sentinels+'/link');
const snapshot=path=>{const value=fs.lstatSync(path);return {inode:value.ino,uid:value.uid,gid:value.gid,mode:value.mode,link:value.isSymbolicLink()?fs.readlinkSync(path):null,contents:value.isFile()?fs.readFileSync(path,'utf8'):null,children:value.isDirectory()?fs.readdirSync(path).sort().map(name=>[name,snapshot(path+'/'+name)]):null};};
const childrenBefore=snapshot(sentinels);
run('/usr/bin/mount',['--bind','/workspace','/workspace']);fs.chownSync('/workspace',0,0);fs.chmodSync('/workspace',0o2755);const workspaceMode=fs.statSync('/workspace').mode;
run('/usr/sbin/groupadd',['--gid','2001','byovm-extra']);
function one(name,{rootBoot=true,managed='1',home=true,uid=1500,nnp='1',groups=[],fuse='1',updates=false,exit=0,direct=false,extra={}}={}){
 const dir=root+'/'+name;fs.mkdirSync(dir);fs.chownSync(dir,uid,uid);
 const user=uid===1500?'rudder':'byovm',userHome='/home/'+user;
 const env={PATH:userHome+'/.local/bin:/usr/local/bin:/usr/bin:/bin',HOME:rootBoot?'/root':userHome,USER:rootBoot?'root':user,RUDDER_HOME:home?'1':'0',RUDDER_CACHE_DIR:dir+'/cache',RUDDER_WORKER_MANAGED_EXECUTION:managed,RUDDER_WORKER_SKIP_AGENT_UPDATE:updates?'0':'1',BOOT_CASE:name,BOOT_EXPECT_UID:String(uid),BOOT_EXPECT_HOME:userHome,BOOT_EXPECT_MODE:home?'home':'sail',BOOT_EXPECT_NNP:nnp,BOOT_EXPECT_GROUPS:JSON.stringify(groups),BOOT_EXPECT_FUSE:fuse,BOOT_EXPECT_UPDATES:updates?'1':'0',BOOT_EXPECT_BOUNDS_ZERO:rootBoot?'1':'0',BOOT_LOG_PATH:dir+'/updates.jsonl',BOOT_CREDENTIAL:userHome+'/fixture-credential',...extra};
 const target=direct?'/usr/local/bin/rudder-worker':'/usr/local/bin/rudder-worker-root-bootstrap';
 const args=rootBoot?[target,'--fixture-argument']:['/usr/bin/setpriv','--reuid='+uid,'--regid='+uid,groups.length?'--groups='+groups.join(','):'--clear-groups',target,'--fixture-argument'];
 const result=cp.spawnSync(args[0],args.slice(1),{cwd:'/workspace',env,encoding:'utf8',timeout:extra.BOOT_RUN_HELPER==='1'?200000:15000,maxBuffer:8*1024*1024});
 assert.equal(result.status,exit,name+': '+(result.stderr||result.stdout).slice(-6000));
 assert(!fs.existsSync('/workspace/bootstrap-path-misuse'),'root bootstrap consulted user utility PATH');
 if(exit===0){const values=result.stdout.split('\n').filter(x=>x.startsWith('{')).map(x=>JSON.parse(x));assert(values.some(x=>x.kind==='worker-bootstrap-case'&&x.case===name));for(const value of values)console.log(JSON.stringify(value));records.push({case:name,exit:0});}
 else{assert(!fs.existsSync(env.RUDDER_CACHE_DIR),'failed boot sourced cache or ran user code');assert(!fs.existsSync(env.BOOT_LOG_PATH),'failed boot ran updates');records.push({case:name,exit,deniedBeforeScripts:true});}
}
const original='/dev/fuse.bootstrap-original';assert(fs.lstatSync('/dev/fuse').isCharacterDevice());
try{
 one('root-managed-home-updates',{updates:true,extra:{BOOT_EXPECT_WORKSPACE_WRITE:'1'}});
 assert.equal(fs.statSync('/workspace').uid,1500);assert.equal(fs.statSync('/workspace').gid,1500);assert.equal(fs.statSync('/workspace').mode,workspaceMode);assert.deepEqual(snapshot(sentinels),childrenBefore);
 const device=fs.lstatSync('/dev/fuse');assert.equal(device.uid,1500);assert.equal(device.gid,1500);assert.equal(device.mode&0o777,0o600);
 one('root-managed-sail',{home:false});
 one('nonroot-managed-via-bootstrap',{rootBoot:false});
 one('nonroot-managed-direct-denied',{rootBoot:false,direct:true,exit:78});
 fs.chmodSync('/workspace',0o555);one('nonroot-readonly-workspace-denied',{rootBoot:false,exit:78});fs.chmodSync('/workspace',workspaceMode&0o7777);
 fs.chownSync('/workspace',2000,2000);one('nonroot-foreign-workspace-denied',{rootBoot:false,exit:78});fs.chownSync('/workspace',1500,1500);fs.chmodSync('/workspace',workspaceMode&0o7777);
 one('nonroot-byovm-legacy',{rootBoot:false,managed:'0',uid:2000,nnp:'0',groups:[2001],fuse:'0'});
 one('nonroot-direct-legacy',{rootBoot:false,direct:true,managed:'0',uid:2000,nnp:'0',groups:[2001],fuse:'unset'});
 fs.chownSync('/workspace',2000,2000);one('foreign-workspace-managed-denied',{exit:78});assert.equal(fs.statSync('/workspace').uid,2000);fs.chownSync('/workspace',1500,1500);
 run('/usr/bin/umount',['/workspace']);fs.chownSync('/workspace',0,0);one('root-owned-unmounted-workspace-denied',{exit:78});assert.equal(fs.statSync('/workspace').uid,0);run('/usr/bin/mount',['--bind','/workspace','/workspace']);
 fs.renameSync('/dev/fuse',original);
 one('missing-managed-denied',{exit:78});
 one('missing-legacy-continues',{managed:'0',fuse:'0'});
 fs.writeFileSync('/workspace/fuse-unsafe-target','must stay unchanged',{mode:0o644});fs.symlinkSync('/workspace/fuse-unsafe-target','/dev/fuse');
 one('symlink-managed-denied',{exit:78});one('symlink-legacy-continues',{managed:'0',fuse:'0'});
 assert.equal(fs.readFileSync('/workspace/fuse-unsafe-target','utf8'),'must stay unchanged');assert.equal(fs.statSync('/workspace/fuse-unsafe-target').uid,0);assert.equal(fs.statSync('/workspace/fuse-unsafe-target').mode&0o777,0o644);
 fs.unlinkSync('/dev/fuse');run('/usr/bin/mknod',['/dev/fuse','c','1','3']);fs.chmodSync('/dev/fuse',0o644);
 one('wrong-device-managed-denied',{exit:78});assert.equal(fs.lstatSync('/dev/fuse').uid,0);assert.equal(fs.lstatSync('/dev/fuse').mode&0o777,0o644);
 fs.unlinkSync('/dev/fuse');fs.linkSync(original,'/dev/fuse');
 one('hardlink-managed-denied',{exit:78});assert.equal(fs.lstatSync(original).nlink,2);fs.unlinkSync('/dev/fuse');
 run('/usr/bin/mknod',['/dev/fuse','c','10','229']);fs.chownSync('/dev/fuse',2222,2222);fs.chmodSync('/dev/fuse',0o600);
 one('foreign-owner-managed-denied',{exit:78});assert.equal(fs.lstatSync('/dev/fuse').uid,2222);fs.unlinkSync('/dev/fuse');
 fs.writeFileSync('/dev/fuse','ordinary-file',{mode:0o644});
 one('regular-file-managed-denied',{exit:78});assert.equal(fs.readFileSync('/dev/fuse','utf8'),'ordinary-file');fs.unlinkSync('/dev/fuse');
 fs.renameSync(original,'/dev/fuse');
 one('managed-helper-after-drop',{extra:{BOOT_RUN_HELPER:'1'}});
 assert.deepEqual(snapshot(sentinels),childrenBefore);
 console.log(JSON.stringify({kind:'worker-bootstrap-matrix',cases:records,deviceType:'character10:229',deviceGrant:'1500:1500/0600',safeDeviceOnly:true,workspaceMountTopOnly:true,existingChildInodesPreserved:true,noSetuidHelper:true,defaultManagedLaunchChanged:false}));
}finally{if(fs.existsSync(original)){try{fs.unlinkSync('/dev/fuse')}catch{}fs.renameSync(original,'/dev/fuse');}}
