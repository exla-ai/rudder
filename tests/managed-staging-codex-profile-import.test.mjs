import test from 'node:test';
import assert from 'node:assert/strict';
import {EventEmitter} from 'node:events';
import {PassThrough} from 'node:stream';
import {setImmediate as turn} from 'node:timers/promises';
import {inspect} from 'node:util';
import {OriginalStagingCodexProfileImport,validateStagingCodexAccessSnapshot} from '../cloud/dist/managed-home-provisioning/staging-codex-profile-import.js';
const profile='openai-codex:default';
const target={app:'rudder-mh-0123456789ab4cde81230123456789ab',machine:'1855366c049e08',instanceId:'01M670MG4A8E0DWE9GA1C0PQ92',projectId:'01234567-89ab-4cde-8123-0123456789ab',home:'/workspace/fresh-v4/01234567-89ab-4cde-8123-0123456789ab/home'};
const ack='CODEX_OAUTH_ACCESS_SNAPSHOT_IMPORTED_V1\n';
function wire(changes={},claimsChanges={}){const exp=Math.floor(Date.now()/1000)+3600,claims={exp,'https://api.openai.com/auth':{chatgpt_account_id:'account',chatgpt_plan_type:'plus'},...claimsChanges};return Buffer.from(JSON.stringify({formatVersion:1,kind:'CodexOAuthAccessSnapshotV1',profileId:profile,backend:'codex',provider:'openai-codex',accessToken:'e30.'+Buffer.from(JSON.stringify(claims)).toString('base64url')+'.c2ln',chatgptAccountId:'account',expiresAtMs:exp*1000,chatgptPlanType:'plus',...changes}));}
class Fake extends EventEmitter{
 stdin=new PassThrough();stdout=new PassThrough();stderr=new PassThrough();kills=[];options;input=[];
 constructor(){super();this.stdin.on('data',b=>this.input.push(Buffer.from(b)));}
 spawn(o){this.options=o;}kill(s){this.kills.push(s);return true;}
 settle(code=0,text=ack){this.stdout.end(text);this.stderr.end();this.emit('exit',code,null);this.emit('close',code,null);}
}
function fixture(options={}){const child=options.child??new Fake(),socket={},route={homeId:'home'},session={homeId:'home',socket},owner={homeId:'home',socket,assertLocalCurrent(){if(options.local)options.local();},async assertCurrent(){if(options.owner)await options.owner();}};
 let allocations=0,targets=0;const actual={route,session,socket,owner,sessions:{isCurrent:()=>options.current!==false},registry:{async assertManagedHomeSocketMatchesRoute(s,r){assert.equal(s,socket);assert.equal(r,route);if(options.registry)await options.registry();}},setup:{remaining:()=>options.remaining??10000},driver:{end:Infinity,remaining:()=>options.remaining??10000},async assertTargetCurrent(t){assert.deepEqual(t,target);targets++;if(options.target)await options.target();}};
 const raw=wire(),read=new OriginalStagingCodexProfileImport(actual,raw,profile,target,'private-fly-token','/explicit/flyctl',()=>{allocations++;return child;});
 return {read,raw,child,allocations:()=>allocations,targets:()=>targets};}
async function spawned(f){const run=f.read.startRetaining();await turn();assert(f.child.options);return {run};}

test('strict closed snapshot independently binds JWT expiration/account/plan and exact wire',()=>{
 const b=wire(),out=validateStagingCodexAccessSnapshot(b,profile);assert.deepEqual(b,out);assert.notEqual(b,out);
 assert.deepEqual(validateStagingCodexAccessSnapshot(JSON.parse(b),profile),b);
 for(const input of [wire({extra:1}),wire({profileId:'foreign'}),wire({backend:'claude'}),wire({expiresAtMs:0}),wire({chatgptAccountId:'foreign'}),wire({chatgptPlanType:'free'}),wire({}, {exp:1}),wire({}, {exp:Math.floor(Date.now()/1000)+90000}),Buffer.concat([b,Buffer.from('\n')]),Buffer.from(b.toString().replace('"formatVersion":1','"formatVersion":1,"formatVersion":1')),Buffer.alloc(65533)])assert.throws(()=>validateStagingCodexAccessSnapshot(input,profile));
 const dup=Buffer.from('{"exp":1,"exp":'+(Math.floor(Date.now()/1000)+1000)+',"https://api.openai.com/auth":{"chatgpt_account_id":"account","chatgpt_plan_type":"plus"}}').toString('base64url');assert.throws(()=>validateStagingCodexAccessSnapshot(wire({accessToken:'e30.'+dup+'.c2ln'}),profile));
});
test('original one-shot sends only stdin to exact home; env sanitizes Fly alternate token',async()=>{
 const old=process.env.FLY_ACCESS_TOKEN;process.env.FLY_ACCESS_TOKEN='inherited';try{
 const f=fixture(),expected=Buffer.from(f.raw),{run}=await spawned(f);f.raw.fill(120);const o=f.child.options;
 assert.deepEqual(o.args,['/explicit/flyctl','ssh','console','--app',target.app,'--machine',target.machine,'--user','rudder','--quiet','-C',`/usr/bin/env RUDDER_HOME=${target.home} /usr/local/bin/node /opt/rudder/dist/managed-task-profile-cli.js import-codex-oauth-access-snapshot`]);
 assert.deepEqual(Buffer.concat(f.child.input),expected);assert(!JSON.stringify(o).includes(JSON.parse(expected).accessToken));assert(o.envPairs.includes('FLY_API_TOKEN=private-fly-token'));assert(!o.envPairs.some(x=>x.startsWith('FLY_ACCESS_TOKEN=')));
 assert(!inspect(f.read).includes('private-fly-token'));assert.throws(()=>JSON.stringify(f.read));assert.throws(()=>f.read.startRetaining());f.child.settle();await run;assert(f.read.statusData().imported);assert.equal(f.allocations(),1);assert.equal(f.targets(),2);
 }finally{if(old===undefined)delete process.env.FLY_ACCESS_TOKEN;else process.env.FLY_ACCESS_TOKEN=old;}
});
test('actual exit and close are not pipe EOF and no task completion is invented',async()=>{
 const f=fixture(),{run}=await spawned(f);let done=false;run.then(()=>done=true);f.child.stdout.write(ack);f.child.emit('exit',0,null);f.child.emit('close',0,null);await turn();assert(!done);f.child.stdout.end();await turn();assert(!done);f.child.stderr.end();await run;assert.equal(f.read.statusData().task_started,false);
});
test('same cap timeout kills only original local child and retains promise until terminal pipes',async t=>{
 t.mock.timers.enable({apis:['setTimeout']});
 const f=fixture(),{run}=await spawned(f);let done=false;run.catch(()=>done=true);t.mock.timers.tick(10001);assert.deepEqual(f.child.kills,['SIGKILL']);assert(!done);f.child.settle(1,'');await assert.rejects(run,/unconfirmed/);assert.equal(f.allocations(),1);assert(f.read.statusData().errors.includes('original_import_deadline'));
});
test('async currentness refusal before effect never spawns; checks after result fail closed',async()=>{
 const f=fixture({owner:async()=>{throw Error('secret');}});await assert.rejects(f.read.startRetaining(),/unavailable/);assert.equal(f.allocations(),0);
 let good=true;const g=fixture({target:async()=>{if(!good)throw Error('private target');}}),{run}=await spawned(g);good=false;g.child.settle();await assert.rejects(run,/unconfirmed/);assert(!g.read.statusData().imported);
});
test('reordered async route check cannot dispatch after local ownership changes',async()=>{
 let local=true;const f=fixture({local(){if(!local)throw Error('private');},registry:async()=>{local=false;}});await assert.rejects(f.read.startRetaining());assert.equal(f.allocations(),0);
});
test('unknown spawn retains same carrier and promise, never retries',async t=>{
 t.mock.timers.enable({apis:['setTimeout']});
 const child=new Fake();child.spawn=()=>{throw Error('private spawn');};const f=fixture({child}),run=f.read.startRetaining();let settled=false;run.catch(()=>settled=true);await turn();t.mock.timers.tick(10001);assert(!settled);assert.equal(f.allocations(),1);assert.deepEqual(child.kills,['SIGKILL']);assert(!f.read.statusData().constructor_completed);
});
test('fixed ACK/empty stderr required; errors/output bounds discard secret text',async()=>{
 for(const bad of ['wrong',ack+ack]){const f=fixture(),{run}=await spawned(f);f.child.settle(0,bad);await assert.rejects(run);assert(!f.read.statusData().imported);}
 const f=fixture(),{run}=await spawned(f);f.child.stderr.write('private diagnostic');f.child.settle();await assert.rejects(run);assert(!JSON.stringify(f.read.statusData()).includes('private diagnostic'));
});
test('stdin failure is fixed and same child remains held until actual terminal',async()=>{
 const f=fixture(),{run}=await spawned(f);f.child.stdin.emit('error',Error('private credential'));assert.deepEqual(f.child.kills,['SIGKILL']);f.child.settle(1,'');await assert.rejects(run);assert(!JSON.stringify(f.read.statusData()).includes('private credential'));
});
