// Executed only by the disposable-VM fixture after the real worker entrypoint.
import fs from 'node:fs';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import cp from 'node:child_process';
const status=fs.readFileSync('/proc/self/status','utf8');
const field=name=>new RegExp('^'+name+':[ \\t]*([^\\r\\n]*)$','m').exec(status)?.[1];
const uid=Number(process.env.BOOT_EXPECT_UID);
assert.equal(process.getuid(),uid);assert.equal(process.geteuid(),uid);
assert.equal(process.getgid(),uid);assert.equal(process.getegid(),uid);
// Node includes the effective primary GID even when Linux getgroups(2) is
// empty. Check the kernel supplementary list separately so a primary-inclusive
// Node result cannot hide a retained boot group.
const supplemental=field('Groups').trim().split(/\s+/).filter(Boolean).map(Number).sort((a,b)=>a-b);
const expectedGroups=JSON.parse(process.env.BOOT_EXPECT_GROUPS).sort((a,b)=>a-b);
assert.deepEqual(supplemental,expectedGroups);
assert.deepEqual(process.getgroups().sort((a,b)=>a-b),[...new Set([uid,...expectedGroups])].sort((a,b)=>a-b));
assert.equal(field('Uid').trim(),[uid,uid,uid,uid].join('\t'));
assert.equal(field('NoNewPrivs').trim(),process.env.BOOT_EXPECT_NNP);
for(const key of ['CapEff','CapPrm','CapInh','CapAmb'])assert(/^0+$/.test(field(key)));
if(process.env.BOOT_EXPECT_BOUNDS_ZERO==='1')assert(/^0+$/.test(field('CapBnd')));
assert.equal(process.env.HOME,process.env.BOOT_EXPECT_HOME);
assert.equal(process.env.RUDDER_WORKER_FUSE_READY??'unset',process.env.BOOT_EXPECT_FUSE);
assert.equal(fs.statSync(process.env.RUDDER_CACHE_DIR).uid,uid);
if(process.env.BOOT_EXPECT_WORKSPACE_WRITE==='1'){
 const file='/workspace/worker-write-'+process.env.BOOT_CASE;fs.writeFileSync(file,'after permanent drop',{flag:'wx',mode:0o600});assert.equal(fs.statSync(file).uid,uid);
}
assert.equal(process.argv[1].endsWith('/home.mjs')?'home':'sail',process.env.BOOT_EXPECT_MODE);
assert(process.argv.includes('--fixture-argument'),'entrypoint lost arguments');
if(uid===1500&&process.env.BOOT_EXPECT_NNP==='1')assert.throws(()=>process.setuid(0));
const credential=fs.readFileSync(process.env.BOOT_CREDENTIAL);
const file=fs.statSync(process.env.BOOT_CREDENTIAL);assert.equal(file.uid,uid);assert.equal(file.mode&0o777,0o600);
if(process.env.BOOT_EXPECT_FUSE==='1'){
 const device=fs.lstatSync('/dev/fuse');assert(device.isCharacterDevice());assert.equal(device.uid,1500);assert.equal(device.gid,1500);assert.equal(device.mode&0o777,0o600);
 const fd=fs.openSync('/dev/fuse','r+');fs.closeSync(fd);
}
if(process.env.BOOT_EXPECT_UPDATES==='1'){
 const end=Date.now()+3000;while(Date.now()<end){let names=[];try{names=fs.readFileSync(process.env.BOOT_LOG_PATH,'utf8').trim().split('\n').map(x=>JSON.parse(x).tool)}catch{}if(['claude','npm','codex'].every(x=>names.includes(x)))break;await new Promise(r=>setTimeout(r,10));}
 const entries=fs.readFileSync(process.env.BOOT_LOG_PATH,'utf8').trim().split('\n').map(x=>JSON.parse(x));
 for(const name of ['claude','npm','codex'])assert(entries.some(x=>x.tool===name));
 for(const value of entries){assert.equal(value.uid,uid);assert.equal(value.nnp,'1');}
}
console.log(JSON.stringify({kind:'worker-bootstrap-case',case:process.env.BOOT_CASE,uid,gid:process.getgid(),groups:supplemental,nnp:field('NoNewPrivs').trim(),fuseReady:process.env.RUDDER_WORKER_FUSE_READY??'unset',home:process.env.HOME,mode:process.env.BOOT_EXPECT_MODE,credentialSha256:crypto.createHash('sha256').update(credential).digest('hex'),updatesAfterDrop:process.env.BOOT_EXPECT_UPDATES==='1',capsZero:true,cacheOwnedByUser:true}));
if(process.env.BOOT_RUN_HELPER==='1'){
 const result=cp.spawnSync('/workspace/controller',['rpc-qualify','/opt/runtime/rudder-native','/workspace/execution-rpc'],{env:{...process.env,RUDDER_EXECUTION_GIT_QUALIFY:'1'},encoding:'utf8',timeout:180000,maxBuffer:4*1024*1024});
 if(result.stdout)process.stdout.write(result.stdout);
 if(result.status!==0){process.stderr.write((result.stderr||'helper did not exit successfully').slice(-6000));throw Error('post-drop managed helper qualification failed');}
 const proof=result.stdout.split('\n').filter(x=>x.startsWith('{')).map(x=>JSON.parse(x)).find(x=>x.kind==='workspace-observation-rpc');
 assert(proof&&proof.records.length===4);assert.equal(proof.records[0].guest_proof.git.git,'git version 2.56.0');assert.equal(proof.records[0].guest_proof.git.jj_fetch_changed_file_remote,true);
}
