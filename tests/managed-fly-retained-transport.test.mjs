import test from 'node:test';
import assert from 'node:assert/strict';
import {inspect} from 'node:util';
import {randomUUID} from 'node:crypto';
import {FlyHttpTransport,FlyManagedHomeProvider,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {hash} from '../cloud/dist/managed-home-provisioning/types.js';
const token='private-synthetic-fly-token-canary',echo='private-synthetic-secret-echo-canary';
const app='rudder-retained-http-fixture';
const mocked=new WeakSet();
function mock(t,fetch){if(!mocked.has(t)){const previous=globalThis.fetch;mocked.add(t);t.after(()=>{globalThis.fetch=previous;});}globalThis.fetch=fetch;}
function intent(){return {nonce:randomUUID(),appName:app,networkName:'fixture-network',policy:{region:'sjc',organization:'fixture-org',volumeGb:1},runtimeQualification:{fixture_data_only:true}};}
function appReply(i){return {id:'app-id',name:i.appName,network:i.networkName,network_cidr:'fdaa:1::/48',organization:{slug:i.policy.organization}};}
function transport(){return new FlyHttpTransport(token,128);}
function assertPrivate(data,...privateValues){const text=JSON.stringify(data);for(const v of [token,echo,...privateValues])assert(!text.includes(v));}

test('non-2xx original response survives #call refusal without exposing echoed secrets',async t=>{
 const body=JSON.stringify({error:echo,value:echo}),http=transport(),i=intent();let calls=0;
 mock(t,async(url,init)=>{calls++;assert.equal(String(url),'https://api.machines.dev/v1/apps?wait=true');assert.equal(init.method,'POST');assert.equal(init.headers.Authorization,'Bearer '+token);return new Response(body,{status:400});});
 const provider=new FlyManagedHomeProvider(http);await assert.rejects(provider.execute('app',i,{},undefined,originalTime(1000)),{code:'effect_unknown'});
 const data=provider.originalDisposableResponseData(i),r=data.transport_requests[0];assert.equal(calls,1);assert.equal(r.status,400);assert.equal(r.classification,'provider_non_2xx');assert.equal(r.body_sha256,hash(body));assert.equal(r.body_complete,true);assert.equal(r.settlement,'settled');assert.equal(r.path,'/v1/apps');assertPrivate(data);assertPrivate(inspect(http));assert.throws(()=>JSON.stringify(http),{code:'invalid_request'});
 await assert.rejects(provider.execute('app',{...i,appName:'foreign-app'}, {},undefined,{remaining(){throw Error('original unavailable');}}));assert.equal(calls,1,'expired original cannot dispatch or replay');
});

test('original volume ID remains retained when later name/region/size metadata refuses',async t=>{
 for(const bad of ['name','region','size_gb']){
  const http=transport(),provider=new FlyManagedHomeProvider(http),i=intent(),a=appReply(i),acks={app:{id:a.id,name:a.name,network:a.network,networkCidr:a.network_cidr,organization:i.policy.organization}};
  const planned=provider.describe('volume',i,acks);let creation;
  mock(t,async(url,init)=>{if(init.method==='GET')return Response.json(a);creation=JSON.parse(init.body);const b={id:'vol_original_retained',name:creation.name,region:creation.region,size_gb:creation.size_gb,[bad]:bad==='size_gb'?2:'wrong-metadata'};return Response.json(b,{status:201});});
  await assert.rejects(provider.execute('volume',i,acks,undefined,originalTime(1000)),{code:'identity_changed'});
  const data=provider.originalDisposableResponseData(i);assert.deepEqual(data.original_responses.volume,{id:'vol_original_retained'});assert.equal(data.transport_requests.at(-1).status,201);assert.equal(data.transport_requests.at(-1).body_complete,true);assert.equal(data.transport_requests.at(-1).classification,'ok');assert.deepEqual(creation,planned.body);
 }
});

test('canonical <=30 byte volume name is identical in description, POST and ACK validation',async t=>{
 const http=transport(),provider=new FlyManagedHomeProvider(http),i=intent(),a=appReply(i),acks={app:{id:a.id,name:a.name,network:a.network,networkCidr:a.network_cidr,organization:i.policy.organization}};
 const planned=provider.describe('volume',i,acks);assert.match(planned.body.name,/^[a-z0-9_]{1,30}$/);assert.equal(planned.body.name,'rudder_'+i.nonce.replaceAll('-','').slice(0,23));assert.equal(Buffer.byteLength(planned.body.name),30);
 let post;mock(t,async(url,init)=>{if(init.method==='GET')return Response.json(a);post=JSON.parse(init.body);return Response.json({id:'vol_original_valid',name:post.name,region:post.region,size_gb:post.size_gb},{status:201});});
 const ack=await provider.execute('volume',i,acks,undefined,originalTime(1000));assert.deepEqual(post,planned.body);assert.equal(ack.name,planned.body.name);assert.equal(ack.id,'vol_original_valid');
});

test('real response reader retains partial bytes and error privately through rejected read',async t=>{
 const http=transport(),prefix=Buffer.from('{"secret":"'+echo);
 let rejectRead;const stream=new ReadableStream({start(c){c.enqueue(prefix);rejectRead=()=>c.error(Error(echo));}});
 mock(t,async()=>new Response(stream,{status:502}));const pending=http.request('POST','/v1/apps/'+app+'/secrets/PRIVATE_NAME',{value:echo},originalTime(2000),{'fly-machine-lease-nonce':echo});
 await new Promise(setImmediate);const before=http.originalResponseCustodyData(app)[0];assert.equal(before.status,502);assert.equal(before.classification,'pending');assert.equal(before.settlement,'pending');assert.equal(before.body_complete,false);assert.equal(before.body_sha256,hash(prefix));assertPrivate(before,'PRIVATE_QUERY_VALUE');
 rejectRead();await assert.rejects(pending,{code:'effect_unknown'});const after=http.originalResponseCustodyData(app)[0];assert.equal(after.classification,'reply_read_error');assert.equal(after.body_sha256,hash(prefix));assert.equal(after.body_complete,false);assertPrivate(after);assert.throws(()=>http.archiveKnownCompleted(app),{code:'effect_unknown'});
});

test('actual abort timeout retains in-flight fetch and refuses its late successful response',async t=>{
 const http=transport();let finish,signal;const aborted=new Promise(resolve=>{mock(t,(_url,init)=>{signal=init.signal;signal.addEventListener('abort',resolve,{once:true});return new Promise(r=>{finish=r;});});});
 const pending=http.request('POST','/v1/apps/'+app+'/volumes',{name:'owned'},originalTime(150));await aborted;
 const before=http.originalResponseCustodyData(app)[0];assert.equal(signal.aborted,true);assert.equal(before.classification,'request_timeout');assert.equal(before.settlement,'pending');assert.equal(before.status,null);assert.throws(()=>http.archiveKnownCompleted(app),{code:'effect_unknown'});
 finish(Response.json({id:'vol_late_original',echo},{status:201}));await assert.rejects(pending,{code:'effect_unknown'});const after=http.originalResponseCustodyData(app)[0];assert.equal(after.status,201);assert.equal(after.classification,'request_timeout');assert.equal(after.body_complete,false);assertPrivate(after);
});

test('128 KiB response bound keeps exact bounded prefix and actual cancellation',async t=>{
 const http=transport(),payload=Buffer.alloc(128*1024+7,120);let cancelled=0;
 const stream=new ReadableStream({start(c){c.enqueue(payload);},cancel(){cancelled++;}});mock(t,async()=>new Response(stream,{status:200}));
 await assert.rejects(http.request('GET','/v1/apps/'+app,undefined,originalTime(1000)),{code:'effect_unknown'});
 const r=http.originalResponseCustodyData(app)[0];assert.equal(r.classification,'reply_too_large');assert.equal(r.body_complete,false);assert.equal(r.body_sha256,hash(payload.subarray(0,128*1024)));assert.equal(cancelled,1);assertPrivate(r);
});

test('invalid JSON keeps original status/full SHA; fixed request budget does not retry',async t=>{
 const http=new FlyHttpTransport(token,1),raw='invalid-'+echo;let calls=0;mock(t,async()=>{calls++;return new Response(raw,{status:503});});
 await assert.rejects(http.request('GET','/v1/apps/'+app,undefined,originalTime(1000)),{code:'effect_unknown'});const r=http.originalResponseCustodyData(app)[0];assert.equal(r.status,503);assert.equal(r.classification,'reply_invalid_json');assert.equal(r.body_complete,true);assert.equal(r.body_sha256,hash(raw));assertPrivate(r);
 await assert.rejects(http.request('GET','/v1/apps/'+app,undefined,originalTime(1000)),{code:'busy'});assert.equal(calls,1);
});

test('known-completed archival is explicit and app-scoped; secret request/reply remains private',async t=>{
 const http=new FlyHttpTransport(token);mock(t,async()=>Response.json({value:echo}));
 await http.request('POST','/v1/apps/'+app+'/secrets/FIXED_SECRET',{value:echo},originalTime(1000));await http.request('GET','/v1/apps/foreign-fixture?private='+echo,undefined,originalTime(1000));
 const before=http.originalResponseCustodyData(app);assert.equal(before.length,1);assertPrivate(before);assertPrivate(http.originalResponseCustodyData('foreign-fixture'));
 http.archiveKnownCompleted(app);assert.deepEqual(http.originalResponseCustodyData(app),[]);assert.equal(http.originalResponseCustodyData('foreign-fixture').length,1);assert.equal(http.requestBudgetData().attempted,2);
});
