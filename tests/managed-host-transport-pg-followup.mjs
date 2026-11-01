// ROOT-RUN ONLY via the exact hash-bound disposable real-PG fixture hook.
// Real PostgreSQL/TLS/roles and actual registry producers, fixture HMAC Root,
// fake Fly and memory S3. No physical Root/native/clock/sidecar/lease authority.
import assert from 'node:assert/strict';
import http from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
import {randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {createManagedHostTransportMigrator} from '../cloud/dist/managed-home-provisioning/index.js';
import {TRANSPORT_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/transport-schema.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {prepareFixturePolicy as prepareClockFixture} from './managed-owner-renewal-pg-followup.mjs';
const require=createRequire(new URL('../cloud/package.json',import.meta.url));
const pg=require('pg'),{WebSocket,WebSocketServer}=require('ws');
const S='rudder_project_authority',REGISTRATION='rudder_managed_registration',RUNTIME='rudder_managed_runtime';
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
let prepared,held,result;
// Keep every original fixture socket/promise/opaque owner until actual socket
// closure or exact fixture process/VM teardown. This is not Root retirement.
const pairs=[],pending=[],opaqueOwners=[];
async function bounded(original,ms,label){assert(pending.length<128);pending.push(original);let timer;
 try{return await Promise.race([original,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(label)),ms);})]);}
 finally{clearTimeout(timer);}}
async function step(name,work){assert(result);const start=performance.now();result.currentStep=name;await work();result.steps.push({name,passed:true,milliseconds:Math.ceil(performance.now()-start)});}
function commitFields(challenge){const {version,kind,registration,...fields}=challenge;assert.equal(version,1);assert.equal(kind,'host_transport_challenge');
 return {...fields,native_acceptance_sha256:hash('fixture native acceptance DATA only:'+fields.attempt_nonce)};}

export async function prepareFixturePolicy(context){
 assert.equal(prepared,undefined);const clock=await prepareClockFixture(context);prepared={...context};
 return {policy:clock.policy,async installAfterMigrations(){
  await clock.installAfterMigrations();
  const migration=createManagedHostTransportMigrator(context.config('postgres'));
  try{assert.equal((await migration.migrate(REGISTRATION)).status,'committed');}finally{await migration.close();}
  const row=(await context.query(`SELECT state,migration_sha256 FROM ${S}.managed_host_transport_installation WHERE authority_id=$1`,[context.authorityId])).rows[0];
  assert.equal(row.state,'frozen');assert.equal(row.migration_sha256,TRANSPORT_MIGRATION_SHA256);
  assert.equal((await context.query(`SELECT sha256 FROM ${S}.managed_host_transport_migrations WHERE version=1`)).rows[0].sha256,TRANSPORT_MIGRATION_SHA256);
  prepared.migrationFrozen=true;
 }};
}

async function roleClient(role,work){const connection=prepared.config(role).connection;
 const client=new pg.Client({...connection,ssl:{rejectUnauthorized:true,servername:connection.host,ca:connection.ca},application_name:'managed-host-transport-fixture-role'});
 client.on('error',()=>{});await client.connect();try{return await work(client);}finally{await client.end();}}

async function newPair(registry,worker){assert(pairs.length<12);const accepted=once(held.ws,'connection');
 const client=new WebSocket('ws://127.0.0.1:'+held.http.address().port,{handshakeTimeout:3000,followRedirects:false,perMessageDeflate:false});client.on('error',()=>{});
 const opened=once(client,'open'),pair={client,socket:null,clientClosed:false,serverClosed:false,clientClose:null,serverClose:null};pairs.push(pair);
 const [socket]=await bounded(accepted,4000,'original loopback WS accept timeout');pair.socket=socket;socket.on('error',()=>{});
 pair.clientClose=once(client,'close').then(()=>{pair.clientClosed=true;});
 pair.serverClose=once(socket,'close').then(()=>{pair.serverClosed=true;registry.closeManagedHostSocket(socket);});
 await bounded(opened,4000,'original loopback WS open timeout');registry.bindAuthenticatedManagedHomeSocket(socket,worker);return pair;
}
async function closePair(pair){if(!pair.socket)return;if(!pair.clientClosed)pair.client.close(1000,'fixture generation closed');
 await bounded(Promise.all([pair.clientClose,pair.serverClose]),4000,'actual original loopback close events missing');
 assert(pair.clientClosed&&pair.serverClosed);}

export async function qualify({registry,worker,account,workspace,initialAcquire,operation,perform,budget,query,backend,fixtureTransport}){
 assert(prepared?.migrationFrozen);assert.equal(backend.evidence.actualS3,false);assert(fixtureTransport&&Object.isFrozen(fixtureTransport));
 result={scope:'actual-PG-host-transport-generation-component-only',actualTransportPgQualification:false,actualPostgres:true,
  transportMigrationFrozen:true,restrictedRegistrationRole:false,tenantRls:false,actualLoopbackWebSockets:false,replacement:false,
  currentRegistrationRefusal:false,staleSerialRefused:false,expiredOriginalRefused:false,unchangedOriginalAckReconciliation:false,authRowLockRevocationSerialized:false,
  committedAckLossRetained:false,fixtureRootHmac:true,fixtureFly:true,fixtureClockAdmission:true,
  physicalRoot:false,physicalNative:false,actualConfiguredSidecar:false,actualTransportHmacOnWebSocket:false,actualS3:false,
  qualifiedClockContinuity:false,normalRootRetirement:false,productionEnablement:false,steps:[]};
 held={registry,worker,account,query,fixtureTransport,latest:null,http:http.createServer()};
 held.ws=new WebSocketServer({server:held.http,maxPayload:16384,perMessageDeflate:false});held.ws.on('error',()=>{});
 await bounded(new Promise((resolve,reject)=>{held.http.once('error',reject);held.http.listen(0,'127.0.0.1',resolve);}),4000,'loopback listener timeout');
 let serial=9007199254740993n;
 const begin=async(pair,maximum=4000,n=serial++)=>{const original=await budget(maximum),fields={serial:n.toString(),generation:randomUUID(),attempt_nonce:randomUUID(),constraint:original.constraint,remaining_milliseconds:original.remaining()};
  const input=fixtureTransport.signRequest('begin',fields),challengePacket=await registry.beginManagedHostTransport(worker,pair.socket,input);
  const challenge=fixtureTransport.verifyResponse('challenge',challengePacket);assert.equal(challenge.begin_sha256,hash(canonical(input.body,16384)));assert.deepEqual(challenge.constraint,original.constraint);
  return {pair,original,input,challenge,commit:fixtureTransport.signRequest('commit',commitFields(challenge))};};
 const commit=async(g)=>{g.original.remaining();const completed=await registry.commitManagedHostTransport(worker,g.pair.socket,g.commit);
  const ack=fixtureTransport.verifyResponse('ack',completed.packet);assert.equal(ack.commit_sha256,hash(canonical(g.commit.body,16384)));assert.deepEqual(ack.constraint,g.original.constraint);
  opaqueOwners.push(completed.owner);g.owner=completed.owner;g.ack=ack;g.packet=completed.packet;await g.owner.assertCurrent();await registry.assertManagedHostSocketCurrent(worker,g.pair.socket);return g;};
 const confirm=async(g)=>registry.confirmManagedHostTransport(worker,g.pair.socket,fixtureTransport.signRequest('confirm',{
  serial:g.ack.serial,generation:g.ack.generation,attempt_nonce:g.ack.attempt_nonce,commit_sha256:g.ack.commit_sha256}));
 const one=await newPair(registry,worker);
 await step('actual-frozen-transport-migration-does-not-authenticate',async()=>{
  const b=await budget(4000),input=fixtureTransport.signRequest('begin',{serial:serial.toString(),generation:randomUUID(),attempt_nonce:randomUUID(),constraint:b.constraint,remaining_milliseconds:b.remaining()});
  await assert.rejects(registry.beginManagedHostTransport(worker,one.socket,input));
  assert.equal((await query(`SELECT count(*)::text AS n FROM ${S}.managed_host_transports`)).rows[0].n,'0');
  // Explicit disposable fixture administrator only, never production ready API.
  const activated=await query(`UPDATE ${S}.managed_host_transport_installation SET state='ready' WHERE authority_id=$1 AND state='frozen' AND migration_sha256=$2`,[prepared.authorityId,TRANSPORT_MIGRATION_SHA256]);assert.equal(activated.rowCount,1);
 });
 let original,next,replacement;
 await step('actual-original-worker-socket-MAC-and-highest-wide-serial',async()=>{
  original=await begin(one);const foreign=await newPair(registry,worker);next=await begin(foreign);
  await assert.rejects(registry.commitManagedHostTransport(worker,foreign.socket,original.commit));
  const changed=commitFields(original.challenge);changed.generation=randomUUID();await assert.rejects(registry.commitManagedHostTransport(worker,one.socket,fixtureTransport.signRequest('commit',changed)));
  const wrongMac=structuredClone(next.commit);wrongMac.mac='f'.repeat(64);await assert.rejects(registry.commitManagedHostTransport(worker,foreign.socket,wrongMac));
  assert(original.original.remaining()>250);const liveOriginal=(await query(`SELECT clock_timestamp()+interval '0.25 seconds'<original_end AS live FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND serial=$3`,[prepared.authorityId,account,original.input.body.serial])).rows[0];assert.equal(liveOriginal.live,true);
  await assert.rejects(registry.commitManagedHostTransport(worker,one.socket,original.commit));
  const stale=(await query(`SELECT state FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND serial=$3`,[prepared.authorityId,account,original.input.body.serial])).rows[0];assert.equal(stale.state,'pending');
  await commit(next);await confirm(next);assert.throws(()=>JSON.stringify(next.owner));result.staleSerialRefused=true;
 });
 await step('actual-new-generation-supersedes-only-original-socket-and-old-close-cannot-clear-it',async()=>{
  replacement=await commit(await begin(await newPair(registry,worker)));await confirm(replacement);
  await assert.rejects(next.owner.assertCurrent());await closePair(next.pair);await replacement.owner.assertCurrent();await registry.assertManagedHostSocketCurrent(worker,replacement.pair.socket);
  await closePair(original.pair);await replacement.owner.assertCurrent();result.replacement=true;result.actualLoopbackWebSockets=true;
 });
 await step('actual-original-auth-deadline-refuses-late-commit-and-readback-clock',async()=>{
  const expired=await begin(await newPair(registry,worker),1000);await sleep(1150);
  await assert.rejects(registry.commitManagedHostTransport(worker,expired.pair.socket,expired.commit));
  await assert.rejects(registry.commitManagedHostTransport(worker,expired.pair.socket,expired.commit));
  const row=(await query(`SELECT state,clock_timestamp()<original_end AS live FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND serial=$3`,[prepared.authorityId,account,expired.input.body.serial])).rows[0];assert.equal(row.state,'pending');assert.equal(row.live,false);
  await replacement.owner.assertCurrent();await closePair(expired.pair);result.expiredOriginalRefused=true;
 });
 await step('actual-late-original-operation-ACK-is-unchanged-DATA-with-no-effect-retry',async()=>{
  const originalBudget=await budget(2000),input=operation({op:'PrepareFreshProjectV1',workspace_id:workspace.workspaceId},'transport-retained-preparation',originalBudget),ack=await perform(input);
  assert.equal(ack.body.error,null);const before=(await query(`SELECT count(*)::text AS n FROM ${S}.managed_home_fresh_preparations WHERE project_id=$1`,[workspace.projectId])).rows[0].n;assert.equal(before,'1');
  await sleep(2150);assert.throws(()=>originalBudget.remaining());
  const references=[{request_nonce:input.body.request_nonce,request_sha256:ack.body.request_sha256},{request_nonce:initialAcquire.body.request_nonce,request_sha256:initialAcquire.body.request_sha256}];
  const request=fixtureTransport.signRequest('reconcile',{serial:replacement.ack.serial,generation:replacement.ack.generation,attempt_nonce:replacement.ack.attempt_nonce,requests:references});
  const found=await registry.reconcileManagedHostTransport(worker,replacement.pair.socket,request);assert.equal(found.length,2);
  assert.deepEqual(found.find(v=>v.body.request_nonce===ack.body.request_nonce),ack);assert.deepEqual(found.find(v=>v.body.request_nonce===initialAcquire.body.request_nonce),initialAcquire);
  const again=await registry.reconcileManagedHostTransport(worker,replacement.pair.socket,request);assert.deepEqual(again,found);
  const {version,kind,registration,...badFields}=structuredClone(request.body);badFields.requests[0].request_sha256='f'.repeat(64);
  await assert.rejects(registry.reconcileManagedHostTransport(worker,replacement.pair.socket,fixtureTransport.signRequest('reconcile',badFields)));
  assert.equal((await query(`SELECT count(*)::text AS n FROM ${S}.managed_home_fresh_preparations WHERE project_id=$1`,[workspace.projectId])).rows[0].n,before);
  result.unchangedOriginalAckReconciliation=true;result.originalExpiredOperationConstraint=ack.body.constraint;result.noAcquireLeaseAdoption=true;
 });
 await step('actual-begin-COMMIT-ACK-loss-retains-original-and-never-adopts-row-readback',async()=>{
  const pair=await newPair(registry,worker),b=await budget(4000),fields={serial:(serial++).toString(),generation:randomUUID(),attempt_nonce:randomUUID(),constraint:b.constraint,remaining_milliseconds:b.remaining()},input=fixtureTransport.signRequest('begin',fields);
  const loss=await fixtureTransport.dropCommitAck('begin',async()=>{await assert.rejects(registry.beginManagedHostTransport(worker,pair.socket,input));});assert(loss.injected);
  const row=(await query(`SELECT state,request_sha256 FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND serial=$3`,[prepared.authorityId,account,fields.serial])).rows[0];assert.equal(row.state,'pending');assert.equal(row.request_sha256,hash(canonical(input.body,16384)));
  await assert.rejects(registry.beginManagedHostTransport(worker,pair.socket,input));await assert.rejects(registry.assertManagedHostSocketCurrent(worker,pair.socket));await closePair(pair);
  result.beginCommitAckLossRetained=true;
 });
 await step('actual-commit-COMMIT-ACK-loss-cannot-construct-owner-from-stored-ACK',async()=>{
  const unknown=await begin(await newPair(registry,worker));const loss=await fixtureTransport.dropCommitAck('commit',async()=>{await assert.rejects(registry.commitManagedHostTransport(worker,unknown.pair.socket,unknown.commit));});assert(loss.injected);
  const row=(await query(`SELECT state,ack_json FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND serial=$3`,[prepared.authorityId,account,unknown.input.body.serial])).rows[0];assert.equal(row.state,'committed');fixtureTransport.verifyResponse('ack',row.ack_json); // DATA inspection only, no owner factory.
  await assert.rejects(registry.assertManagedHostSocketCurrent(worker,unknown.pair.socket));await assert.rejects(replacement.owner.assertCurrent());
  await assert.rejects(registry.commitManagedHostTransport(worker,unknown.pair.socket,unknown.commit));await closePair(unknown.pair);await closePair(replacement.pair);
  held.latest=await commit(await begin(await newPair(registry,worker)));await confirm(held.latest);
  const rows=(await query(`SELECT serial::text FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND state='committed'`,[prepared.authorityId,account])).rows;assert.deepEqual(rows,[{serial:held.latest.ack.serial}]);
  result.committedAckLossRetained=true;result.commitLossFault='after-real-COMMIT-query-success-fixture-throws-ECONNRESET';result.actualNetworkCut=false;
 });
 await step('actual-registration-TLS-role-grants-and-forced-tenant-RLS',async()=>{
  const classes=(await query(`SELECT relrowsecurity,relforcerowsecurity FROM pg_class WHERE oid=$1::regclass`,[S+'.managed_host_transports'])).rows[0];assert(classes.relrowsecurity&&classes.relforcerowsecurity);
  await roleClient(REGISTRATION,async client=>{
   assert.equal((await client.query(`SELECT count(*)::text AS n FROM ${S}.managed_host_transports`)).rows[0].n,'0');
   assert.equal((await client.query(`SELECT has_table_privilege(current_user,$1,'SELECT') AS allowed`,[S+'.managed_owner_renewals'])).rows[0].allowed,true,'production transport role must be granted renewal receipt SELECT; no fixture GRANT');
   const registration=held.latest.ack.registration;
   for(const [table,where,id]of [['authorities','authority_id',prepared.authorityId],['authority_credentials','credential_id',registration.credential_id],['engine_instances','engine_instance_id',registration.engine_instance_id],['managed_home_installation','authority_id',prepared.authorityId]]){
    assert.equal((await client.query(`SELECT has_column_privilege(current_user,$1,'transport_lock_version','UPDATE') AS allowed`,[S+'.'+table])).rows[0].allowed,true);
    await client.query('BEGIN');await client.query("SELECT set_config('rudder.account_id',$1,true)",[account]);assert.equal((await client.query(`UPDATE ${S}.${table} SET transport_lock_version=transport_lock_version WHERE ${where}=$1 RETURNING transport_lock_version`,[id])).rowCount,1);await client.query('ROLLBACK');
   }
   const row=(await query(`SELECT * FROM ${S}.managed_host_transports WHERE authority_id=$1 AND account_id=$2 AND state='committed'`,[prepared.authorityId,account])).rows[0];
   await client.query('BEGIN');await client.query("SELECT set_config('rudder.account_id',$1,true)",[account]);assert(Number((await client.query(`SELECT count(*)::text AS n FROM ${S}.managed_host_transports`)).rows[0].n)>0);await client.query('ROLLBACK');
   const other=(await query(`SELECT account_id FROM ${S}.managed_home_intents WHERE account_id<>$1 LIMIT 1`,[account])).rows[0].account_id;
   await client.query('BEGIN');await client.query("SELECT set_config('rudder.account_id',$1,true)",[other]);assert.equal((await client.query(`SELECT count(*)::text AS n FROM ${S}.managed_host_transports`)).rows[0].n,'0');
   await assert.rejects(client.query(`INSERT INTO ${S}.managed_host_transports(authority_id,account_id,home_id,engine_instance_id,boot_id,serial,generation,attempt_nonce,request_sha256,request_json,challenge_json,original_end,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,clock_timestamp()+interval '1 second','pending')`,[row.authority_id,account,row.home_id,row.engine_instance_id,row.boot_id,'9223372036854775800',randomUUID(),randomUUID(),row.request_sha256,JSON.stringify(row.request_json),JSON.stringify(row.challenge_json)]),{code:'42501'});await client.query('ROLLBACK');
   for(const sql of [`UPDATE ${S}.managed_host_transport_installation SET state='ready'`,`UPDATE ${S}.managed_home_installation SET state='ready'`,`UPDATE ${S}.authorities SET state='ready'`,
    `UPDATE ${S}.authority_credentials SET token_sha256='${'f'.repeat(64)}'`,`UPDATE ${S}.authority_credentials SET expires_at=clock_timestamp()+interval '1 day'`,`UPDATE ${S}.authority_credentials SET revoked_at=NULL`,`UPDATE ${S}.engine_instances SET revoked_at=NULL`,
    `DELETE FROM ${S}.managed_host_transport_migrations`,`ALTER TABLE ${S}.managed_host_transports ADD COLUMN forbidden integer`,`TRUNCATE ${S}.managed_host_transports`,'SET ROLE postgres'])await assert.rejects(client.query(sql),{code:'42501'});
  });
  await roleClient(RUNTIME,async client=>{await assert.rejects(client.query(`SELECT count(*) FROM ${S}.managed_host_transports`),{code:'42501'});});
  result.restrictedRegistrationRole=true;result.tenantRls=true;
 });
 await step('actual-production-six-row-auth-lock-serializes-attempted-revocation',async()=>{
  const pair=await newPair(registry,worker),b=await budget(4000),input=fixtureTransport.signRequest('begin',{serial:(serial++).toString(),generation:randomUUID(),attempt_nonce:randomUUID(),constraint:b.constraint,remaining_milliseconds:b.remaining()});
  const registration=held.latest.ack.registration;let observed=[];
  const heldRows=await fixtureTransport.withCredentialRowsHeld(()=>registry.beginManagedHostTransport(worker,pair.socket,input),async({backendPid})=>{
   const connection=prepared.config('postgres').connection,revoker=new pg.Client({...connection,ssl:{rejectUnauthorized:true,servername:connection.host,ca:connection.ca},application_name:'managed-host-transport-fixture-revoker'});revoker.on('error',()=>{});await revoker.connect();
   await revoker.query('BEGIN');const revokerPid=(await revoker.query('SELECT pg_backend_pid() AS pid')).rows[0].pid;
   const update=revoker.query(`UPDATE ${S}.authority_credentials SET revoked_at=clock_timestamp() WHERE account_id=$1 AND credential_id=$2`,[account,registration.credential_id]);pending.push(update);update.catch(()=>{});
   const cleanup=async()=>{try{await update;await revoker.query('ROLLBACK');}finally{await revoker.end();}};
   try{for(let i=0;i<80;i++){const rows=(await query("SELECT pid,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock'",[revokerPid])).rows;
     observed=rows.filter(row=>row.blockers.includes(backendPid));if(observed.length)break;await sleep(10);}assert.equal(observed.length,1,'actual revocation must wait on original production auth row locks');return cleanup;
   }catch(error){const rollback=cleanup();pending.push(rollback);rollback.catch(()=>{});throw error;}
  });
  assert.equal(heldRows.heldOriginalCredentialRows,true);fixtureTransport.verifyResponse('challenge',heldRows.value);
  assert.equal((await query(`SELECT revoked_at IS NULL AS unchanged FROM ${S}.authority_credentials WHERE account_id=$1 AND credential_id=$2`,[account,registration.credential_id])).rows[0].unchanged,true);
  await closePair(pair);await held.latest.owner.assertCurrent();result.authRowLockRevocationSerialized=true;result.actualBlockedRevocationSessions=observed.length;
 });
 result.currentStep='await-existing-original-engine-revocation';return result;
}

// Runs only AFTER the base qualifier's actual committed credential revocation.
// Never un-revokes a credential or uses socket closure as proof of revocation.
export async function afterEngineRevocation(){
 assert(held?.latest&&result&&!result.actualTransportPgQualification);
 await step('actual-current-registration-revocation-refuses-even-still-open-original-socket',async()=>{
  assert.equal(held.latest.pair.socket.readyState,WebSocket.OPEN);const registration=held.latest.ack.registration;
  const row=(await held.query(`SELECT revoked_at IS NOT NULL AS revoked FROM ${S}.authority_credentials WHERE account_id=$1 AND credential_id=$2`,[held.account,registration.credential_id])).rows[0];assert.equal(row.revoked,true);
  await assert.rejects(held.latest.owner.assertCurrent());await assert.rejects(held.registry.assertManagedHostSocketCurrent(held.worker,held.latest.pair.socket));
  result.currentRegistrationRefusal=true;
 });
 for(const pair of pairs)if(pair.socket)await closePair(pair);
 await bounded(new Promise(resolve=>held.ws.close(resolve)),4000,'fixture WS server did not actually close');
 await bounded(new Promise(resolve=>held.http.close(resolve)),4000,'fixture HTTP listener did not actually close');
 result.actualClosedSocketPairs=pairs.filter(p=>p.clientClosed&&p.serverClosed).length;assert.equal(result.actualClosedSocketPairs,pairs.length);
 result.currentStep='complete-retained-until-fixture-process-teardown';result.actualTransportPgQualification=true;
}

// Bounded public fixture DATA only. No raw driver/SQL error, key, bearer,
// socket, registry, opaque owner or preparation/acquire capability is exposed.
export function failureData(){return structuredClone(result??{scope:'actual-PG-host-transport-generation-component-only',actualTransportPgQualification:false,currentStep:'before-followup-entry'});}
