// Real transport/promise/Response mechanisms with inert mocked HTTP only.
// No provider/Root/native/SDK authority is constructed by these cases.
import test from 'node:test';
import assert from 'node:assert/strict';
import {FlyHttpTransport,originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
const token='synthetic-private-retention-fixture-token';
function mock(t,fetch){const previous=globalThis.fetch;globalThis.fetch=fetch;t.after(()=>{globalThis.fetch=previous;});}
const path=app=>'/v1/apps/'+app+'/machines/fixture-machine';

test('default transport keeps all16 original app groups beyond128 aggregate requests',async t=>{
 let calls=0;mock(t,async()=>{calls++;return Response.json({known_original_reply:true});});const http=new FlyHttpTransport(token),time=originalTime(10000);
 for(let n=0;n<16;n++)for(let r=0;r<9;r++)assert.equal((await http.request('GET',path('home-'+n),undefined,time)).status,200);
 assert.equal(calls,144);assert.deepEqual(http.requestBudgetData(),{maximum:null,attempted:144});
 for(let n=0;n<16;n++){const rows=http.originalResponseCustodyData('home-'+n);assert.equal(rows.length,9);assert(rows.every(r=>r.status===200&&r.body_complete&&r.settlement==='settled'));}
 await assert.rejects(http.request('GET',path('seventeenth'),undefined,time),{code:'busy'});assert.equal(calls,144,'group refusal occurs before any provider effect');
});

test('each default original retains its own128 bound without consuming unrelated capacity',async t=>{
 let calls=0;mock(t,async()=>{calls++;return Response.json({owned:true});});const http=new FlyHttpTransport(token),time=originalTime(10000);
 for(let r=0;r<128;r++)await http.request('GET',path('one-original'),undefined,time);
 await assert.rejects(http.request('GET',path('one-original'),undefined,time),{code:'busy'});assert.equal(calls,128);assert.equal(http.originalResponseCustodyData('one-original').length,128);
 await http.request('GET',path('distinct-original'),undefined,time);assert.equal(calls,129);assert.equal(http.originalResponseCustodyData('distinct-original').length,1);assert.equal(http.originalResponseCustodyData('one-original').length,128);
});

test('unknown original remains charged; only distinct known-complete archive frees a group',async t=>{
 let calls=0;mock(t,async url=>{calls++;if(String(url).includes('/unknown-original/'))throw Error('inert original fetch failed after dispatch');return Response.json({owned:true});});
 const http=new FlyHttpTransport(token),time=originalTime(10000);await assert.rejects(http.request('POST',path('unknown-original'),{nonce:'inert'},time),{code:'effect_unknown'});
 for(let n=1;n<16;n++)await http.request('GET',path('known-'+n),undefined,time);
 assert.throws(()=>http.archiveKnownCompleted('unknown-original'),{code:'effect_unknown'});const unknown=http.originalResponseCustodyData('unknown-original');assert.equal(unknown.length,1);assert.equal(unknown[0].classification,'fetch_error');
 await assert.rejects(http.request('GET',path('new-original'),undefined,time),{code:'busy'});assert.equal(calls,16);
 http.archiveKnownCompleted('known-1');await http.request('GET',path('new-original'),undefined,time);assert.equal(calls,17);assert.deepEqual(http.originalResponseCustodyData('unknown-original'),unknown,'unknown custody is neither evicted nor reconstructed');assert.equal(http.originalResponseCustodyData('new-original').length,1);
});

test('explicit campaign128 remains a lifetime cap even after every successful request is archived',async t=>{
 let calls=0;mock(t,async()=>{calls++;return Response.json({owned:true});});const http=new FlyHttpTransport(token,128),time=originalTime(10000);
 for(let r=0;r<128;r++){await http.request('GET',path('qualification'),undefined,time);http.archiveKnownCompleted('qualification');}
 assert.deepEqual(http.originalResponseCustodyData('qualification'),[]);assert.deepEqual(http.requestBudgetData(),{maximum:128,attempted:128});
 await assert.rejects(http.request('GET',path('different-qualification'),undefined,time),{code:'busy'});assert.equal(calls,128,'archival cannot renew/reset an explicit campaign');
});

test('unidentified original requests consume bounded default capacity too',async t=>{
 let calls=0;mock(t,async()=>{calls++;return Response.json({owned:true});});const http=new FlyHttpTransport(token),time=originalTime(10000);
 // A list request has no app correlation; it is retained in one bounded group.
 for(let r=0;r<128;r++)await http.request('GET','/v1/apps',undefined,time);
 await assert.rejects(http.request('GET','/v1/apps',undefined,time),{code:'busy'});assert.equal(calls,128);
 for(let n=1;n<16;n++)await http.request('GET',path('known-'+n),undefined,time);
 await assert.rejects(http.request('GET',path('sixteenth-named-plus-unidentified'),undefined,time),{code:'busy'});assert.equal(calls,143);
});
