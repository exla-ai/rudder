import fs from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import assert from 'node:assert/strict';
const exec=promisify(execFile),fly='/workspace/home/.fly/bin/flyctl';
const dir='/workspace/home/.cache/rudder-release-build/alpha10-checks', frozen='/workspace/home/.cache/rudder-release-build/alpha10-56f825e';
const mode=process.argv[2];assert(['baseline','verify'].includes(mode));
const controlId='8911d61a434978',homeId='8edd225a7d0378';
const expectedDigest='sha256:7a51c3199478706a68b234d32ff7d1b9b0342fc6c81317a203a5c660888bec5c';
const sha=b=>createHash('sha256').update(b).digest('hex'),quote=s=>"'"+s.replaceAll("'","'\\''")+"'";
async function command(args){return (await exec(fly,args,{timeout:30000,maxBuffer:4*1024*1024})).stdout;}
function machine(m){assert(m);return {id:m.id,instance:m.instance_id,state:m.state,image:m.config?.image,digest:m.image_ref?.digest,
 revision:m.image_ref?.labels?.['org.opencontainers.image.revision']};}
async function child(code,esm=false){let last;for(let i=0;i<3;i++){try{const raw=await command(['machine','exec',controlId,'-a','rudder-cloud-control',(esm?'node --input-type=module -e ':'node -e ')+quote(code),'--timeout','20']);if(raw.trim())return JSON.parse(raw.trim());}catch(error){last=error;}}throw Error('Control proof returned no valid JSON');}
const [controls,workers]=await Promise.all([command(['machine','list','-a','rudder-cloud-control','--json']).then(JSON.parse),command(['machine','list','-a','rudder-workers-exla','--json']).then(JSON.parse)]);
const raw=controls.find(m=>m.id===controlId),control=machine(raw),home=machine(workers.find(m=>m.id===homeId));
const database=await child(`const fs=require('node:fs'),crypto=require('node:crypto'),DB=require('/app/node_modules/better-sqlite3');const db=new DB('/tmp/rudder-cloud/rudder-cloud.sqlite',{readonly:true,fileMustExist:true});const result={integrity:db.pragma('quick_check',{simple:true}),counts:{workspaces:db.prepare('select count(*) as n from rudder_workspaces').get().n,sails:db.prepare('select count(*) as n from rudder_sails').get().n},homeRetained:!!db.prepare('select 1 from rudder_workspaces where machine_id=?').get(${JSON.stringify(homeId)}),serverSha256:crypto.createHash('sha256').update(fs.readFileSync('/app/src/server.ts')).digest('hex')};db.close();console.log(JSON.stringify(result));`);
const report={formatVersion:1,version:'4.0.0-alpha.10',revision:'56f825ea3c1d6136b1a20b3031f835ef33f870cd',mode,control,home,database,verifiedAt:new Date().toISOString()};
assert.equal(database.integrity,'ok');assert(database.homeRetained);
if(mode==='verify'){
 assert.equal(control.digest,expectedDigest);assert.equal(raw.config?.env?.RUDDER_WORKER_IMAGE,'registry.fly.io/rudder-workers-exla:4.0.0-alpha.10');
 assert.equal(database.serverSha256,sha(await fs.readFile(frozen+'/cloud/src/server.ts')));
 const before=JSON.parse(await fs.readFile(dir+'/control-before.json'));
 report.homeUnchangedAcrossDeploy=before.home.instance===home.instance&&before.home.state===home.state;
 const pg=await child(`import fs from 'node:fs';import{createHash}from'node:crypto';import{createAuthority}from'/app/dist/project-authority/index.js';const api=createAuthority();const stats=api.stats();await api.close();console.log(JSON.stringify({inactiveStats:stats,pgVersion:JSON.parse(fs.readFileSync('/app/node_modules/pg/package.json')).version,indexSha256:createHash('sha256').update(fs.readFileSync('/app/src/project-authority/index.ts')).digest('hex')}));`,true);
 assert.deepEqual(pg.inactiveStats,{active:0,queued:0});assert.equal(pg.pgVersion,'8.23.1');assert.equal(pg.indexSha256,sha(await fs.readFile(frozen+'/cloud/src/project-authority/index.ts')));report.inactiveAuthority=pg;
 report.checks=await Promise.all([
 ['https://rudder.exla.ai/app',200],['https://rudder-cloud-control.fly.dev/health',200],
 ['https://rudder-cloud-control.fly.dev/api/app/state',401],['https://rudder-cloud-control.fly.dev/api/rudder/fabric/roots/unauthenticated-alpha10-smoke',401]
 ].map(async([url,expected])=>{const r=await fetch(url,{signal:AbortSignal.timeout(20000)});const out={url,status:r.status,expected,finalUrl:r.url,ok:r.status===expected};if(url.endsWith('/health')){out.health=await r.json();assert.equal(out.health.ok,true);}assert(out.ok);return out;}));
 const releases=JSON.parse(await command(['releases','-a','rudder-cloud-control','--json']));report.releases=releases.slice(0,2).map(x=>({id:x.ID??x.id,version:x.Version??x.version,status:x.Status??x.status,createdAt:x.CreatedAt??x.created_at}));
 report.verifiedAt=new Date().toISOString();
}
await fs.writeFile(dir+(mode==='baseline'?'/control-before.json':'/control-deployed-proof.json'),JSON.stringify(report,null,2)+'\n',{mode:0o600});
console.log(JSON.stringify(report,null,2));
