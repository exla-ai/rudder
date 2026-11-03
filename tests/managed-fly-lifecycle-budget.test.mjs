// UNRUN. Actual production classes + controlled local clock/timers + intercepted
// HTTP only. Fixture Fly/clock metadata is not physical provider/Root readiness.
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {createFlyMachineLeaseSession,flyMachineConfigDigest,FlyCustodyError,FLY_MANAGED_HOME_LIFECYCLE as PROFILE,flyProviderHttpTimeoutLimit} from '../cloud/dist/fly-custody.js';
import {FlyHttpTransport,FlyManagedHomeProvider,machineConfig,machineAck} from '../cloud/dist/managed-home-provisioning/fly.js';
const TOKEN='TEST_ONLY_private_lifecycle_transport_token',NONCE='TEST_ONLY_private_provider_nonce';
const APP='rudder-lifecycle-fixture',BASE='/v1/apps/'+APP+'/machines/machine-original';
const CONFIG={image:'registry.example/fixture@sha256:'+'1'.repeat(64),env:{},services:[]};
const expected={app:APP,machineId:'machine-original',instanceId:'instance-original',configSha256:flyMachineConfigDigest(CONFIG),challenge:'a'.repeat(64)};
const isCode=code=>e=>e instanceof FlyCustodyError&&e.code===code;
function clock(){let elapsed=0;return {elapsed:()=>elapsed,advance(ms){elapsed+=ms;},sample:()=>({wallMs:1_700_000_000_000+elapsed,boottimeMs:10_000+elapsed}),time(total=300_000){const end=elapsed+total;return {end,remaining(){const n=end-elapsed;if(n<=0)throw Error('TEST_ONLY_ORIGINAL_CUTOFF');return n;}};}};}
function timers(t,c){
 const set=globalThis.setTimeout,clear=globalThis.clearTimeout,pending=new Set(),owned=new WeakSet(),scheduled=[];
 globalThis.setTimeout=(callback,ms,...args)=>{const value={at:c.elapsed()+Number(ms),callback,args,unref(){return this;},ref(){return this;}};pending.add(value);owned.add(value);scheduled.push(Number(ms));return value;};
 globalThis.clearTimeout=value=>{if(owned.has(value))pending.delete(value);else clear(value);};
 t.after(()=>{globalThis.setTimeout=set;globalThis.clearTimeout=clear;});
 return {scheduled,advance(ms){c.advance(ms);for(;;){const next=[...pending].filter(x=>x.at<=c.elapsed()).sort((a,b)=>a.at-b.at)[0];if(!next)break;pending.delete(next);next.callback(...next.args);}}};
}
function wire(t,{actualWall=false}={}){
 const c=clock(),time=c.time(),http=new FlyHttpTransport(TOKEN,128),previous=globalThis.fetch,calls=[],contexts=[];let lease=null,hold=null;
 const f={c,time,http,calls,contexts,machine:{id:expected.machineId,name:'fixture-home',instance_id:expected.instanceId,version:expected.instanceId,state:'stopped',region:'sjc',host_status:'ok',config:structuredClone(CONFIG)},app:null};
 f.hold=(method,path)=>{let enter,finish;const entered=new Promise(r=>{enter=r;}),waiting=new Promise(r=>{finish=r;});hold={method,path,enter,waiting};return {entered,finish};};
 globalThis.fetch=async(url,init)=>{
  const parsed=new URL(String(url)),path=parsed.pathname;calls.push({path,query:parsed.search,method:init.method,body:init.body,signal:init.signal});let response;
  if(f.app&&path==='/v1/apps/'+APP&&init.method==='GET')response=Response.json(f.app);
  else if(path===BASE&&init.method==='GET')response=Response.json(f.machine);
  else if(path===BASE+'/wait'&&init.method==='GET'){
   assert.equal(parsed.searchParams.get('state'),'settled');assert.equal(parsed.searchParams.get('version'),f.machine.instance_id);
   f.onWait?.();
   f.machine.state='stopped';response=f.waitResponse?f.waitResponse():Response.json({});
  }
  else if(path===BASE+'/lease'&&init.method==='POST'){
   const body=JSON.parse(init.body);assert.equal([...body.description].length,64);lease={nonce:NONCE,expires_at:Math.ceil(((actualWall?Date.now():c.sample().wallMs)+body.ttl*1000)/1000),owner:'TEST_ONLY_provider',version:f.machine.instance_id,description:body.description};response=Response.json({status:'success',data:lease},{status:201});
  }else if(path===BASE+'/lease'&&init.method==='GET')response=Response.json({status:'success',data:lease});
  else if(path===BASE+'/lease'&&init.method==='DELETE'){assert.equal(init.headers['fly-machine-lease-nonce'],NONCE);lease=null;response=Response.json({status:'success',data:{ok:true}});}
  else if(path===BASE&&init.method==='POST'){
   assert.equal(init.headers['fly-machine-lease-nonce'],NONCE);const body=JSON.parse(init.body);assert.equal(body.current_version,f.machine.instance_id);f.machine={...f.machine,config:body.config,instance_id:'instance-configured',version:'instance-configured',state:f.updateState??'stopped'};response=Response.json(f.machine);
  }else if(path===BASE+'/start'&&init.method==='POST'){assert.equal(init.headers['fly-machine-lease-nonce'],NONCE);f.machine.state='started';response=Response.json({status:'started'});}
  else assert.fail('unexpected inert wire route');
  if(hold&&hold.method===init.method&&hold.path===path){const original=hold;hold=null;original.enter();await original.waiting;} // deliberately ignores abort until actual settlement
  return response;
 };
 t.after(()=>{globalThis.fetch=previous;});
 f.adapter={request:async(method,path,body,original,headers,context)=>{contexts.push({method,path,context});return http.request(method,path,body,original,headers,context);}};
 f.session=({profile=true,remaining=120_000}={})=>createFlyMachineLeaseSession({expected,clock:c,transport:{request:r=>f.adapter.request(r.method,r.path,r.body===undefined?undefined:JSON.parse(r.body),time,{...r.headers},{signal:r.signal,lifecycleProfile:r.lifecycleProfile})},ttlSeconds:profile?120:15,...(profile?{lifecycleProfile:PROFILE,sessionLifetimeMs:remaining}:{})});
 return f;
}
function privateData(f){const data=JSON.stringify(f.http.originalResponseCustodyData(APP));assert(!data.includes(TOKEN)&&!data.includes(NONCE));return f.http.originalResponseCustodyData(APP);}

test('original lifecycle update ACK after26.7s succeeds with120s lease and unchanged global300 clock',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session();await s.acquire();const held=f.hold('POST',BASE),mutation=s.mutate({action:'update',config:{...CONFIG,env:{FIXTURE:'changed'}}});await held.entered;
 assert.equal(JSON.parse(f.calls.find(x=>x.method==='POST'&&x.path.endsWith('/lease')).body).ttl,120);
 run.advance(26_750);assert.equal(f.calls.at(-1).signal.aborted,false);assert.equal(f.time.remaining(),273_250);held.finish();assert.deepEqual(await mutation,{kind:'machine-mutation-http-ack',action:'update'});
 assert(run.scheduled.includes(60_000));assert(run.scheduled.includes(5_000));assert(!run.scheduled.includes(15_000));await s.release();privateData(f);
});
test('60s original mutation limit retains ignored-abort fetch and rejects late200 without replay or restored custody',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session();await s.acquire();const preflight=f.hold('GET',BASE+'/lease');let settled=false;
 const p=s.mutate({action:'update',config:{...CONFIG,env:{FIXTURE:'changed'}}});const result=p.then(()=>{settled=true;return null;},e=>{settled=true;return e;});
 await preflight.entered;const held=f.hold('POST',BASE);run.advance(250);preflight.finish();await held.entered;run.advance(59_751);
 assert.equal(f.calls.at(-1).signal.aborted,true);assert.equal(settled,false);assert.equal(privateData(f).at(-1).settlement,'pending');assert.throws(()=>f.http.archiveKnownCompleted(APP),{code:'effect_unknown'});
 held.finish();assert(await result instanceof FlyCustodyError);assert.equal(s.status().state,'uncertain');assert.equal(privateData(f).at(-1).status,200);assert.equal(privateData(f).at(-1).classification,'original_cancelled');
 await assert.rejects(s.mutate({action:'update',config:CONFIG}),isCode('not_current'));const read=await s.reconcile();assert.equal(read.custodyRestored,false);assert.equal(s.status().state,'uncertain');assert.equal(f.calls.filter(x=>x.method==='POST'&&x.path===BASE).length,1);
});
test('captured original300s remainder wins over60s profile; no new global clock is manufactured',async t=>{
 const f=wire(t),run=timers(t,f.c),originalEnd=f.time.end;run.advance(275_000);
 const s=f.session({remaining:f.time.remaining()});await s.acquire();const held=f.hold('POST',BASE);const p=s.mutate({action:'update',config:CONFIG}).then(()=>null,e=>e);await held.entered;run.advance(26_750);
 assert.equal(f.calls.at(-1).signal.aborted,true);held.finish();assert(await p instanceof FlyCustodyError);assert.equal(s.status().state,'uncertain');
 assert.equal(f.time.end,originalEnd);assert.equal(f.c.elapsed(),301_750);assert.throws(()=>f.time.remaining(),/TEST_ONLY_ORIGINAL_CUTOFF/);
});
test('default operation remains1500ms and unrelated HTTP cap15s; profile checks remain5s',async t=>{
 assert.equal(flyProviderHttpTimeoutLimit(undefined,'POST',BASE),15_000);assert.equal(flyProviderHttpTimeoutLimit(PROFILE,'GET',BASE),5_000);assert.equal(flyProviderHttpTimeoutLimit(PROFILE,'POST',BASE+'/lease'),5_000);assert.equal(flyProviderHttpTimeoutLimit(PROFILE,'POST',BASE+'/start'),60_000);
 const f=wire(t),run=timers(t,f.c),s=f.session({profile:false});await s.acquire();const held=f.hold('POST',BASE);const p=s.mutate({action:'update',config:CONFIG}).then(()=>null,e=>e);await held.entered;run.advance(1501);assert.equal(f.calls.at(-1).signal.aborted,true);held.finish();assert(await p instanceof FlyCustodyError);assert.equal(s.status().state,'uncertain');assert(run.scheduled.includes(1500));assert(run.scheduled.includes(15_000));
});
test('outer original abort is propagated while actual fetch remains pending and preaborted dispatch is refused',async t=>{
 const f=wire(t),abort=new AbortController(),held=f.hold('POST',BASE);const p=f.http.request('POST',BASE,{current_version:expected.instanceId,config:CONFIG},f.time,{'fly-machine-lease-nonce':NONCE}, {signal:abort.signal,lifecycleProfile:PROFILE}).then(()=>null,e=>e);await held.entered;abort.abort();assert.equal(f.calls.at(-1).signal.aborted,true);assert.equal(privateData(f).at(-1).settlement,'pending');held.finish();assert.equal((await p).code,'effect_unknown');assert.equal(privateData(f).at(-1).classification,'original_cancelled');
 const count=f.calls.length;await assert.rejects(f.http.request('POST',BASE,{config:CONFIG},f.time,{}, {signal:abort.signal,lifecycleProfile:PROFILE}),{code:'effect_unknown'});assert.equal(f.calls.length,count);
});
test('lifecycle acquisition/check is capped at5s and cannot mint a timely lease from a late response',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session(),held=f.hold('POST',BASE+'/lease');let settled=false;
 const p=s.acquire().then(()=>{settled=true;return null;},e=>{settled=true;return e;});await held.entered;run.advance(5001);assert.equal(f.calls.at(-1).signal.aborted,true);assert.equal(settled,false);assert.equal(privateData(f).at(-1).settlement,'pending');
 held.finish();assert(await p instanceof FlyCustodyError);assert.equal(s.status().state,'uncertain');assert.equal(f.calls.length,1);await assert.rejects(s.acquire(),isCode('not_current'));
});
test('closed startup profile refuses serialized tags, custom bounds, default120TTL and nonstartup mutations',async t=>{
 const f=wire(t),transport={request:async()=>assert.fail('invalid policy may not dispatch')},options={transport,clock:f.c,expected,ttlSeconds:120,lifecycleProfile:PROFILE};
 for(const change of [{lifecycleProfile:'managed-home-lifecycle-v1'},{lifecycleProfile:{}},{lifecycleProfile:undefined},{ttlSeconds:15},{requestTimeoutMs:5000},{sessionLifetimeMs:120001}])assert.throws(()=>createFlyMachineLeaseSession({...options,...change}),isCode('invalid_input'));
 const s=createFlyMachineLeaseSession(options);await assert.rejects(s.mutate({action:'destroy',force:true}),isCode('invalid_input'));await assert.rejects(s.mutate({action:'stop'}),isCode('invalid_input'));assert.equal(f.calls.length,0);
});
test('actual managed provider selects lifecycle profile forconfigure/start while no secret or clock profile enters body',{skip:process.platform!=='linux'&&'actual provider clock requires Linux /proc/uptime'},async t=>{
 const f=wire(t,{actualWall:true}),i={nonce:randomUUID(),homeId:'fixture-home',storeId:randomUUID(),appName:APP,networkName:'fixture-network',policy:{organization:'fixture-org',region:'sjc',image:CONFIG.image,volumeGb:1,cpus:1,memoryMb:512},runtimeQualification:{fixture_data_only:true}};
 const volume={id:'vol_original',name:'rudder_'+i.nonce.replaceAll('-','').slice(0,23),region:'sjc',sizeGb:1};f.app={id:'app-original',name:APP,network:i.networkName,network_cidr:'fdaa:1::/48',organization:{slug:i.policy.organization}};f.machine.config=machineConfig(i,volume,false);
 const provider=new FlyManagedHomeProvider(f.adapter),machine=machineAck(f.machine,i,f.machine.config,volume),acks={app:{id:f.app.id,name:APP,network:i.networkName,networkCidr:f.app.network_cidr,organization:i.policy.organization},volume,machine,config_secret:{version:'3'},token_secret:{version:'4'},registration_secret:{version:'5'}};
 assert.equal(provider.describe('configure',i,acks).lease.ttl_seconds,120);const configured=await provider.execute('configure',i,acks,undefined,f.time);assert.equal(configured.instanceId,'instance-configured');acks.configure=configured;
 const started=await provider.execute('start',i,acks,undefined,f.time);assert.equal(started.state,'started');
 assert(f.contexts.filter(x=>x.context).every(x=>x.context.signal instanceof AbortSignal&&x.context.lifecycleProfile===PROFILE));assert(f.calls.filter(x=>x.path===BASE+'/lease'&&x.method==='POST').every(x=>JSON.parse(x.body).ttl===120));
 for(const call of f.calls)if(call.body)assert(!call.body.includes('lifecycleProfile'));privateData(f);
});

test('known update ACK waits for its exact version under the original mutation window',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session();await s.acquire();const held=f.hold('GET',BASE+'/wait');
 f.updateState='created';let observedCreated=false,completed=false;
 f.onWait=()=>{assert.equal(f.machine.state,'created');observedCreated=true;};
 const pending=s.mutate({action:'update',config:CONFIG}).then(value=>{completed=true;return value;});await held.entered;
 assert.equal(observedCreated,true);assert.equal(completed,false);
 const wait=f.calls.at(-1);assert.match(wait.query,/version=instance-configured/);assert.equal(flyProviderHttpTimeoutLimit(PROFILE,'GET',BASE+wait.query.replace(/^\?/, '/wait?')),60000);
 run.advance(40000);assert.equal(wait.signal.aborted,false);held.finish();await pending;
 assert.equal(completed,true);assert.equal(f.machine.state,'stopped');assert.equal(f.time.remaining(),260000);assert.equal(f.calls.at(-1).path,BASE);await s.release();
});

test('settlement cannot extend an update that consumed its original mutation budget',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session();await s.acquire();const update=f.hold('POST',BASE);
 const pending=s.mutate({action:'update',config:CONFIG}).then(()=>null,e=>e);await update.entered;run.advance(30000);
 const wait=f.hold('GET',BASE+'/wait');update.finish();await wait.entered;run.advance(30001);
 assert.equal(f.calls.at(-1).signal.aborted,true);wait.finish();assert(await pending instanceof FlyCustodyError);
 assert.equal(s.status().state,'uncertain');assert.equal(f.calls.filter(x=>x.method==='POST'&&x.path===BASE).length,1);
});

for(const failure of ['http','version','config','state'])test('post-ACK settlement refuses '+failure+' without replay',async t=>{
 const f=wire(t),run=timers(t,f.c),s=f.session();await s.acquire();
 f.waitResponse=()=>{
  if(failure==='http')return Response.json({error:'TEST_ONLY wait refusal'},{status:400});
  if(failure==='version')f.machine.instance_id=f.machine.version='foreign-version';
  if(failure==='config')f.machine.config={...f.machine.config,env:{CHANGED:'yes'}};
  if(failure==='state')f.machine.state='started';
  return Response.json({});
 };
 await assert.rejects(s.mutate({action:'update',config:CONFIG}),e=>e instanceof FlyCustodyError);
 assert.equal(s.status().state,'uncertain');assert.equal(f.calls.filter(x=>x.method==='POST'&&x.path===BASE).length,1);
});
