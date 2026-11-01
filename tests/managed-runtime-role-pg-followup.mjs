import {qualifyOriginalStagingActivations,activationCampaignOwner,activateSdkPrecursorRetaining} from './managed-staging-activation-pg-cases.mjs';
// ROOT-RUN ONLY via the existing hash-bound real PG17/TLS followup. Actual
// production factories/SQL/roles/COMMITs/loopback WS; EXPLICIT synthetic Root,
// image/native evidence and Fly HTTP. No physical readiness or lease issuer.
import assert from 'node:assert/strict';
import http from 'node:http';
import {once} from 'node:events';
import {createRequire} from 'node:module';
import {createHmac,randomBytes,randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {createAuthorityMigrator} from '../cloud/dist/project-authority/index.js';
import {AuthorityPool,RESTRICTED_ROLE_CHECK} from '../cloud/dist/project-authority/pool.js';
import {budgets} from '../cloud/dist/project-authority/config.js';
import {createManagedHomeMigrator,createManagedHomeOperationMigrator,createManagedOwnerRenewalMigrator,createManagedHostTransportMigrator,createManagedRuntimeImageQualificationMigrator,createManagedRuntimeImageQualificationRegistry,createManagedCompilerSdkQualificationRegistry,createManagedCompilerSdkQualificationMigrator,createManagedStorageMigrator,createManagedProviderOwnershipMigrator,A2_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/index.js';
import {RootRuntimeImageQualifications} from '../cloud/dist/managed-home-provisioning/runtime-qualification.js';
import {RENEWAL_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/renewal-schema.js';
import {TRANSPORT_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/transport-schema.js';
import {RUNTIME_QUALIFICATION_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/runtime-qualification-schema.js';
import {RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256} from '../cloud/dist/managed-home-provisioning/runtime-qualification-completion-schema.js';
import {runtimeQualificationMac} from '../cloud/dist/managed-home-provisioning/runtime-qualification-protocol.js';
import {TRANSPORT_DOMAINS} from '../cloud/dist/managed-home-provisioning/transport.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {flyFetchFixture} from './managed-storage-pg-fixtures.mjs';
import {prepareFixturePolicy as prepareBaseClockFixture} from './managed-owner-renewal-pg-followup.mjs';
const require=createRequire(new URL('../cloud/package.json',import.meta.url)),pg=require('pg'),{WebSocket,WebSocketServer}=require('ws');
const S='rudder_project_authority',RUNTIME='rudder_managed_runtime',REGISTRATION='rudder_managed_registration';
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const retained=[],pairs=[],databases=[];let fixture,result;
const sign=(key,domain,body)=>createHmac('sha256',key).update(domain).update(canonical(body,24576)).digest('hex');
const packet=(key,domain,body)=>({body,mac:sign(key,domain,body)});
async function bounded(original,ms){retained.push(original);let timer;try{return await Promise.race([original,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('fixture original operation deadline')),ms);})]);}finally{clearTimeout(timer);}}
async function step(name,work){result.currentStep=name;const start=performance.now();await work();result.steps.push({name,passed:true,milliseconds:Math.ceil(performance.now()-start)});}
export async function prepareFixturePolicy(context){assert.equal(fixture,undefined);fixture=context;
 // BASE's actual Acquire requires v4 clock admission. Reuse the existing
 // EXPLICIT synthetic fixture before any base intent/hash/provider effect.
 // This is NOT clock qualification and is NOT inherited by the role DBs.
 return prepareBaseClockFixture(context);}
async function client(config){const c=config.connection,p=new pg.Client({...c,ssl:{rejectUnauthorized:true,servername:c.host,ca:c.ca},application_name:'managed-runtime-role-PG-followup',connectionTimeoutMillis:1000});p.on('error',()=>{});retained.push(p);await p.connect();return p;}
async function role(d,name,work){const p=await client(d.config(name));try{return await work(p);}finally{await p.end();}}
function actualPool(c){return new AuthorityPool({...c.connection,ssl:{rejectUnauthorized:true,servername:c.connection.host,ca:c.connection.ca},application_name:'managed-runtime-role-consumer'},budgets(c),'runtime');}

async function database(label,basePolicy,{activation=false}={}){
 assert(databases.length<6);const name='runtime_role_'+randomUUID().replaceAll('-',''),authorityId=randomUUID();
 const held={name,authorityId,label,createStarted:true,createKnown:false,registry:null,admin:null};databases.push(held);retained.push(held);
 // Actual separate fresh database in SAME verified disposable PG17 cluster.
 // Quoted identifier is generated internally; no existing database is adopted.
 await fixture.query('CREATE DATABASE "'+name+'" TEMPLATE template0');held.createKnown=true;
 held.config=user=>{const c=fixture.config(user);return {...c,authorityId,connection:{...c.connection,database:name}};};
 held.admin=await client(held.config('postgres'));held.query=(sql,a=[])=>held.admin.query(sql,a);
 const p=structuredClone(basePolicy),inventory={kind:'blob',algorithm:'sha256',digest:hash('fixture inventory '+name),size_bytes:1024};
 delete p.ownerLeaseSafety;assert.equal(p.ownerLeaseSafety,undefined); // fresh DB policy, BEFORE migration/hash/intent
 if(activation){p.ownerMode='provider-fenced-v1';p.sdkInitialSource='sdk-fixed-suite-v1';p.compilerSdk=structuredClone(A2_COMPILER_SDK);}
 p.controlPlane={profile:'qualification-v1',qualificationReceiptSha256:hash('synthetic origin policy only')};
 p.imagePreseed.runtimeMount={profile:'baked-cas-v1',inventory,image:p.image,imageReceiptSha256:p.runtimeImageReceiptSha256,fullManifestSha256:p.imagePreseed.fullManifestSha256,installerInput:structuredClone(p.imagePreseed.installerInput)};held.policy=p;
 held.candidate={image:p.image,image_receipt_sha256:p.runtimeImageReceiptSha256,source_manifest_sha256:hash('fixture source '+name),inventory,manifest:{kind:'blob',algorithm:'sha256',digest:p.imagePreseed.fullManifestSha256,size_bytes:2048},installer_input:structuredClone(p.imagePreseed.installerInput),native_sha256:p.nativeSha256,buddy_sha256:hash('fixture jj'),service_sha256:p.serviceSha256,launcher_sha256:p.launcherSha256};
 const ac=held.config('postgres'),m=[createAuthorityMigrator(ac),createManagedHomeMigrator(ac,p),createManagedHomeOperationMigrator(ac),createManagedOwnerRenewalMigrator(ac,RENEWAL_MIGRATION_SHA256),createManagedHostTransportMigrator(ac),createManagedRuntimeImageQualificationMigrator(ac,p)];retained.push(...m);
 for(const [i,args]of [[0,[RUNTIME]],[1,[RUNTIME,REGISTRATION]],[2,[RUNTIME,REGISTRATION]],[3,[RUNTIME]],[4,[REGISTRATION]],[5,[RUNTIME,REGISTRATION]]])assert.equal((await m[i].migrate(...args)).status,'committed');
 if(activation){
  const sdk=createManagedCompilerSdkQualificationMigrator(ac,p),storage=createManagedStorageMigrator(ac,hash('synthetic storage receipt for schema ONLY')),provider=createManagedProviderOwnershipMigrator(ac);retained.push(sdk,storage,provider);m.push(sdk,storage,provider);
  assert.equal((await sdk.migrate(RUNTIME,REGISTRATION)).status,'committed');assert.equal((await storage.migrate([RUNTIME,REGISTRATION])).status,'committed');assert.equal((await provider.migrate([RUNTIME,REGISTRATION])).status,'committed');
 }
 for(const x of m)await x.close();
 for(const table of ['authorities','managed_home_installation','managed_owner_clock_installation','managed_host_transport_installation','managed_runtime_role_installation'])assert.equal((await held.query(`SELECT state FROM ${S}.${table} WHERE authority_id=$1`,[authorityId])).rows[0].state,'frozen');
 for(const [table,version,sha]of [['managed_owner_renewal_migrations',4,RENEWAL_MIGRATION_SHA256],['managed_host_transport_migrations',1,TRANSPORT_MIGRATION_SHA256],['managed_runtime_role_migrations',1,RUNTIME_QUALIFICATION_MIGRATION_SHA256],['managed_runtime_role_completion_migrations',1,RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256]])assert.equal((await held.query(`SELECT sha256 FROM ${S}.${table} WHERE version=$1`,[version])).rows[0].sha256,sha);
 for(const name of [RUNTIME,REGISTRATION])assert.equal((await held.query(RESTRICTED_ROLE_CHECK,[S,name])).rows[0].unsafe,false);
 const rp=actualPool(held.config(RUNTIME)),ap=actualPool(held.config(REGISTRATION));retained.push(rp,ap);held.consumer=new RootRuntimeImageQualifications(rp,ap,authorityId,p);retained.push(held.consumer);
 if(activation){
  const sdkRegistry=createManagedCompilerSdkQualificationRegistry({mode:'postgres',authority:held.config(RUNTIME),registrationAuthority:held.config(REGISTRATION),policy:p,flyToken:'synthetic-managed-pg-provider-only',tokenEncryptionKey:randomBytes(32)},{...held.candidate,sdk:structuredClone(A2_COMPILER_SDK)});
  held.sdkPrecursor={...held,registry:sdkRegistry};retained.push(sdkRegistry,held.sdkPrecursor);
 }
 held.registry=createManagedRuntimeImageQualificationRegistry({mode:'postgres',authority:held.config(RUNTIME),registrationAuthority:held.config(REGISTRATION),policy:p,flyToken:'synthetic-managed-pg-provider-only',tokenEncryptionKey:randomBytes(32)},held.candidate);retained.push(held.registry);
 await assert.rejects(held.registry.provisionRuntimeImageQualification('runtime-role-fixture','frozen'));
 assert.equal((await held.query(`SELECT count(*)::text AS n FROM ${S}.managed_home_intents`)).rows[0].n,'0');
 // Explicit fixture administrator admission only. Runtime role and clock are
 // NOT readied here; the actual disposable role producer works while frozen.
 for(const table of ['authorities','managed_home_installation','managed_host_transport_installation'])assert.equal((await held.query(`UPDATE ${S}.${table} SET state='ready' WHERE authority_id=$1 AND state='frozen'`,[authorityId])).rowCount,1);
 return held;
}

async function pair(d,worker){assert(pairs.length<12);const accepted=once(fixture.ws,'connection'),c=new WebSocket('ws://127.0.0.1:'+fixture.http.address().port,{handshakeTimeout:2000,followRedirects:false,perMessageDeflate:false});c.on('error',()=>{});const opened=once(c,'open');
 const held={client:c,socket:null,closed:false,d};pairs.push(held);const [s]=await bounded(accepted,4000);held.socket=s;s.on('error',()=>{});
 held.closedClient=once(c,'close');held.closedServer=once(s,'close').then(()=>{d.registry.closeManagedHostSocket(s);held.closed=true;});await bounded(opened,4000);d.registry.bindAuthenticatedManagedHomeSocket(s,worker);return held;}
async function close(d,p){if(p.closed)return;d.registry.closeManagedHostSocket(p.socket);p.client.close(1000,'fixture close');await bounded(Promise.all([p.closedClient,p.closedServer]),4000);assert(p.closed);}

async function campaign(d,budget,key='original'){
 const account='runtime-role-fixture',receipt=await d.registry.provisionRuntimeImageQualification(account,key),home=fixture.fly.forHome(receipt.provision.homeId);assert(home);
 const config=home.secrets.get('RUDDER_HOST_CONFIG_V1'),rootKey=Buffer.from(home.secrets.get('RUDDER_ROOT_REGISTRATION_V1').trim(),'hex'),token=home.secrets.get('RUDDER_HOST_TOKEN_V1');
 const worker=await d.registry.authenticateManagedHomeWorker(receipt.provision.homeId,token),original=await budget(30000),begin={version:1,kind:'begin',home_id:receipt.provision.homeId,incarnation:JSON.parse(config).incarnation,config_sha256:hash(config),root_nonce:randomUUID(),constraint:original.constraint},registrationStarted=performance.now();
 const challenge=await d.registry.beginOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_begin_v1\0',begin));
 const ns=Object.fromEntries(['mnt','user','net','pid'].map(n=>[n,{device:'1',inode:'9007199254740994'}])),proc=(pid,image)=>({pid,start_ticks:'9007199254740995',image_sha256:image,exe:{device:'1',inode:String(pid+9000)},namespaces:ns});
 const evidence={boot_id:randomUUID(),root:proc(101,d.policy.launcherSha256),engine:proc(102,d.policy.nativeSha256),service:proc(103,d.policy.serviceSha256),host_worker_tid:104,host_worker_start_ticks:'9007199254740996',root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true};
 const admitted=await d.registry.finishOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_proof_v1\0',{...challenge.body,kind:'proof',evidence}));retained.push(admitted);await admitted.assertOriginalCurrent();const registration=admitted.acknowledgementData();
 assert.equal(registration.mac,sign(rootKey,'rudder_engine_registration_ack_v1\0',registration.body));
 const derivation=Object.fromEntries(['challenge_id','engine_instance_id','credential_id','root_nonce'].map(k=>[k,challenge.body[k]])),credential=Buffer.from('rdre_'+sign(rootKey,'rudder_engine_registration_credential_v1\0',derivation));
 const socket=await pair(d,worker),t=await budget(5000),start={version:1,kind:'host_transport_begin',registration:registration.body,serial:'1',generation:randomUUID(),attempt_nonce:randomUUID(),constraint:t.constraint,remaining_milliseconds:t.remaining()};
 const ch=await d.registry.beginManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.begin,start));assert.equal(ch.mac,sign(credential,TRANSPORT_DOMAINS.challenge,ch.body));
 const commit={...ch.body,kind:'host_transport_commit',native_acceptance_sha256:hash('fixture native accepted')},active=await d.registry.commitManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.commit,commit));retained.push(active.owner);
 assert.equal(active.packet.mac,sign(credential,TRANSPORT_DOMAINS.ack,active.packet.body));
 await d.registry.confirmManagedHostTransport(worker,socket.socket,packet(credential,TRANSPORT_DOMAINS.confirm,{version:1,kind:'host_transport_confirm',registration:registration.body,serial:active.packet.body.serial,generation:active.packet.body.generation,attempt_nonce:active.packet.body.attempt_nonce,commit_sha256:active.packet.body.commit_sha256}));await active.owner.assertCurrent();
 const enable=await budget(30000),end=BigInt(enable.constraint.boottime_deadline_nanoseconds),stamp=end-29900000000n;
 const selected=['package/dist/native/rudder-native','package/dist/native/rudder-jj','usr/local/bin/rudder-root-storage','usr/local/bin/rudder-machine-root'].map((installer_path,i)=>({installer_path,sha256:[d.candidate.native_sha256,d.candidate.buddy_sha256,d.candidate.service_sha256,d.candidate.launcher_sha256][i],size_bytes:1024+i}));
 const observation={format_version:1,kind:'runtime-image-ready-observation-data-v1',scope:'original-enable-ack-and-current-original-client-only',request_nonce:randomUUID(),bootstrap_nonce:begin.root_nonce,engine_instance_id:registration.body.engine_instance_id,epoch:registration.body.binding.epoch,mount_id_data:'123',original_enable_constraint:enable.constraint,root_ack_observed_boottime_nanoseconds:String(stamp),native_received_boottime_nanoseconds:String(stamp+1n),native_issued_boottime_nanoseconds:String(stamp+2n),client_current_checked_boottime_nanoseconds:String(stamp+3n),registration_proof_sha256:registration.body.proof_sha256,config_sha256:hash(config),boot_id:evidence.boot_id,service_pid_data:103,source_authority:false,helper_enrolled:false,scheduler_ready:false,normal_retirement_authority:false,inventory:d.candidate.inventory,manifest:d.candidate.manifest,installer_input:d.candidate.installer_input,selected_executables:selected};
 const body={version:1,kind:'runtime_image_qualification_ready',qualification_id:receipt.scope.qualification_id,qualification_nonce:receipt.scope.qualification_nonce,registration_binding:registration.body.binding,root_nonce:begin.root_nonce,registration_proof_sha256:registration.body.proof_sha256,scope:receipt.scope,observation,selected_executables:selected,remaining_milliseconds:enable.remaining()};
 const request={body,mac:runtimeQualificationMac(rootKey,'ready',body)},held={d,account,receipt,worker,socket,registration,registrationStarted,request,enable,rootKey,credential};retained.push(held);
 held.publish=()=>d.registry.publishRuntimeImageQualification(worker,socket.socket,request);held.rows=async()=>{const q=(await d.query(`SELECT state,ack_json FROM ${S}.managed_runtime_qualifications WHERE qualification_id=$1`,[receipt.scope.qualification_id])).rows[0];return {q,role:(await d.query(`SELECT * FROM ${S}.managed_runtime_image_roles`)).rows[0]??null,completion:(await d.query(`SELECT * FROM ${S}.managed_runtime_role_completions`)).rows[0]??null};};
 held.historical=()=>d.registry.runtimeImageQualificationResultData(account,receipt.scope.qualification_id);return held;
}

// Intercept only REAL pg.Client query settlements for this isolated database.
// SQL always executes in actual PG. COMMIT-loss injects AFTER actual COMMIT
// succeeds; no fake pool/transaction/row or production Context issuer exists.
async function fault(d,options,work){const previous=pg.Client.prototype.query,phase=new WeakMap();let fired=false,observed=false;
 const queryTarget=typeof options.query==='string'&&options.query.length>0,commitTarget=['prepared','completion'].includes(options.commit);
 assert.equal(Number(queryTarget)+Number(commitTarget),1,'one explicit fixture fault boundary required');
 assert(!queryTarget||options.query==='INSERT INTO '+S+'.managed_runtime_image_roles','fixed fixture query target required');
 const record=()=>{observed=true;result.faultBoundaries??=[];assert(result.faultBoundaries.length<8);result.faultBoundaries.push({database_label:d.label,kind:queryTarget?'query':'commit',target:queryTarget?'prepared-role-INSERT':options.commit,actual_query_completed:true});};
 pg.Client.prototype.query=function(query,...args){const sql=typeof query==='string'?query:query?.text,eligible=this.connectionParameters.database===d.name&&this.connectionParameters.user===REGISTRATION;
  if(eligible&&sql?.includes('INSERT INTO '+S+'.managed_runtime_image_roles'))phase.set(this,'prepared');
  if(eligible&&sql?.includes('INSERT INTO '+S+'.managed_runtime_role_completions'))phase.set(this,'completion');
  const actual=previous.call(this,query,...args);if(eligible&&queryTarget&&sql?.includes(options.query)&&!fired){fired=true;return actual.then(async value=>{record();await options.action(this,sql);return value;});}
  if(eligible&&commitTarget&&sql==='COMMIT'&&phase.get(this)===options.commit&&!fired){fired=true;return actual.then(async value=>{record();await options.action(this,sql);return value;});}
  if(sql==='ROLLBACK'||sql==='COMMIT')phase.delete(this);return actual;};
 try{const value=await work();assert(fired&&observed,'exact intended actual pg fault boundary was not completed');return value;}finally{pg.Client.prototype.query=previous;}}

async function grants(d,c){
 for(const table of ['managed_runtime_qualifications','managed_runtime_image_roles','managed_runtime_role_completions']){const r=(await d.query('SELECT relrowsecurity,relforcerowsecurity FROM pg_class WHERE oid=$1::regclass',[S+'.'+table])).rows[0];assert(r.relrowsecurity&&r.relforcerowsecurity);}
 for(const name of [RUNTIME,REGISTRATION])await role(d,name,async p=>{
  assert.equal((await p.query('SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()')).rows[0].ssl,true);
  assert.equal((await p.query(`SELECT count(*)::text AS n FROM ${S}.managed_runtime_qualifications`)).rows[0].n,'0');
  await p.query('BEGIN');await p.query("SELECT set_config('rudder.account_id',$1,true)",[c.account]);assert.equal((await p.query(`SELECT count(*)::text AS n FROM ${S}.managed_runtime_qualifications`)).rows[0].n,'1');await p.query('ROLLBACK');
  await p.query('BEGIN');await p.query("SELECT set_config('rudder.account_id','different-tenant',true)");assert.equal((await p.query(`SELECT count(*)::text AS n FROM ${S}.managed_runtime_qualifications`)).rows[0].n,'0');
  // Artifact binding/completion are intentionally global public immutable DATA;
  // private account/Root proof stays tenant-RLS, never copied into completion.
  assert.equal((await p.query(`SELECT count(*)::text AS n FROM ${S}.managed_runtime_image_roles`)).rows[0].n,'1');assert.equal((await p.query(`SELECT count(*)::text AS n FROM ${S}.managed_runtime_role_completions`)).rows[0].n,'1');await p.query('ROLLBACK');
  for(const table of ['managed_runtime_image_roles','managed_runtime_role_completions'])for(const sql of [`UPDATE ${S}.${table} SET ready_request_sha256='${'f'.repeat(64)}'`,`DELETE FROM ${S}.${table}`,`TRUNCATE ${S}.${table}`])await assert.rejects(p.query(sql),{code:'42501'});
  for(const sql of [`UPDATE ${S}.managed_runtime_role_installation SET state='ready'`,`DELETE FROM ${S}.managed_runtime_role_migrations`,`DELETE FROM ${S}.managed_runtime_role_completion_migrations`,`ALTER TABLE ${S}.managed_runtime_image_roles ADD COLUMN forbidden integer`,'SET ROLE postgres'])await assert.rejects(p.query(sql),{code:'42501'});
  if(name===RUNTIME)for(const table of ['managed_runtime_image_roles','managed_runtime_role_completions'])await assert.rejects(p.query(`INSERT INTO ${S}.${table} SELECT * FROM ${S}.${table}`),{code:'42501'});
  if(name===REGISTRATION){await p.query('BEGIN');await p.query("SELECT set_config('rudder.account_id','different-tenant',true)");await assert.rejects(p.query(`INSERT INTO ${S}.managed_runtime_role_completions SELECT * FROM ${S}.managed_runtime_role_completions`),{code:'42501'});await p.query('ROLLBACK');}
 });
 const rows=await c.rows(),text=JSON.stringify(rows.completion.completion_json);assert(!text.includes(c.account)&&!text.includes(c.receipt.provision.homeId)&&!text.includes(c.registration.body.credential_id));
 result.restrictedRoles=true;result.tenantIsolation=true;result.immutablePreparedAndCompletion=true;result.globalArtifactDataOnly=true;result.actualTlsRoleSessions=true;
}

async function lockedRevocation(d,c,work){const original=pg.Client.prototype.query;let entered=false,lockedClient,revoker,update,observed=false,settled=false;
 pg.Client.prototype.query=function(query,...args){const sql=typeof query==='string'?query:query?.text,eligible=this.connectionParameters.database===d.name&&this.connectionParameters.user===REGISTRATION,actual=original.call(this,query,...args);
  if(!entered&&eligible&&sql?.includes('FOR SHARE OF r,h,e,c,a,g')){entered=true;lockedClient=this;return actual.then(async value=>{
   revoker=await client(d.config('postgres'));await revoker.query('BEGIN');await revoker.query("SET LOCAL statement_timeout='4s'");const pid=(await revoker.query('SELECT pg_backend_pid() AS pid')).rows[0].pid;
   update=revoker.query(`UPDATE ${S}.authority_credentials SET revoked_at=clock_timestamp() WHERE account_id=$1 AND credential_id=$2`,[c.account,c.registration.body.credential_id]);retained.push(update);update.catch(()=>{});
   for(let i=0;i<80;i++){const row=(await d.query("SELECT pg_blocking_pids($1) AS blockers",[pid])).rows[0];if(row.blockers.includes(this.processID)){observed=true;break;}await sleep(10);}assert(observed,'actual auth FOR SHARE must serialize revocation');return value;});}
  if(eligible&&sql==='COMMIT'&&this===lockedClient&&!settled)return actual.then(async value=>{await update;await revoker.query('ROLLBACK');await revoker.end();settled=true;return value;});return actual;};
 try{const value=await work();assert(entered&&observed&&settled);result.authRevocationLock=true;return value;}finally{pg.Client.prototype.query=original;if(revoker&&!settled){const pending=Promise.resolve(update).catch(()=>{}).then(()=>revoker.query('ROLLBACK')).then(()=>revoker.end());retained.push(pending);pending.catch(()=>{});}}}

export async function qualify({budget,backend,graph}){
 assert(fixture&&backend.evidence.actualS3===false);result={scope:'actual-PG-runtime-role-and-completion-component-only',actualRuntimeRolePgQualification:false,actualPostgres:true,actualTlsRoleSessions:false,frozenMigrations:false,frozenHashes:false,restrictedRoles:false,tenantIsolation:false,immutablePreparedAndCompletion:false,globalArtifactDataOnly:false,authRevocationLock:false,knownOriginalCompletion:false,lostFirstCommitNoAdmission:false,unknownSecondCommitHistoricalFact:false,socketCloseBeforeCommit:false,socketCloseAfterCommit:false,providerLeaseExpiryAfterCommit:false,expiredOriginalRefused:false,fixtureRootHmac:true,fixtureFly:true,fixtureBaseClockAdmission:true,fixtureImageNativeEvidence:true,fixtureProviderWallClockStep:true,actualS3:false,physicalRoot:false,physicalNative:false,configuredSidecar:false,qualifiedClockContinuity:false,normalRootRetirement:false,productionEnablement:false,models:false,steps:[]};
 const previousFetch=globalThis.fetch;fixture.fly=flyFetchFixture();globalThis.fetch=fixture.fly.fetch;fixture.http=http.createServer();fixture.ws=new WebSocketServer({server:fixture.http,maxPayload:24576,perMessageDeflate:false});retained.push(fixture.http,fixture.ws);fixture.ws.on('error',()=>{});await bounded(new Promise(resolve=>fixture.http.listen(0,'127.0.0.1',resolve)),4000);
 try{
  await step('real-frozen-hashes-known-completion-locking-and-role-boundaries',async()=>{
   const d=await database('known',graph.policy),m=createManagedRuntimeImageQualificationMigrator(d.config('postgres'),d.policy);retained.push(m);
   await d.query(`UPDATE ${S}.managed_runtime_role_completion_migrations SET sha256=$1 WHERE version=1`,['f'.repeat(64)]);await assert.rejects(m.migrate(RUNTIME,REGISTRATION));
   await d.query(`UPDATE ${S}.managed_runtime_role_completion_migrations SET sha256=$1 WHERE version=1`,[RUNTIME_ROLE_COMPLETION_MIGRATION_SHA256]);assert.equal((await m.migrate(RUNTIME,REGISTRATION)).status,'committed');await m.close();
   const c=await campaign(d,budget),ack=await lockedRevocation(d,c,()=>c.publish());assert.equal(ack.mac,runtimeQualificationMac(c.rootKey,'ack',ack.body));assert.equal(ack.body.result.state,'published');
   assert((await c.rows()).completion);await assert.rejects(d.consumer.assertPublishedRole(d.policy),{code:'unconfigured'});await d.query(`UPDATE ${S}.managed_runtime_role_installation SET state='ready' WHERE authority_id=$1`,[d.authorityId]);await d.consumer.assertPublishedRole(d.policy);
   await grants(d,c);await assert.rejects(c.publish());result.frozenMigrations=true;result.frozenHashes=true;result.knownOriginalCompletion=true;
  });
  await step('real-first-COMMIT-loss-is-prepared-DATA-without-completion-or-new-ACK',async()=>{
   const d=await database('first-loss',graph.policy),c=await campaign(d,budget);await fault(d,{commit:'prepared',action:()=>{throw Object.assign(Error('fixture lost original COMMIT response'),{code:'ECONNRESET'});}},()=>assert.rejects(c.publish(),{code:'effect_unknown'}));
   const rows=await c.rows();assert(rows.role&&rows.q.ack_json&&!rows.completion);await d.query(`UPDATE ${S}.managed_runtime_role_installation SET state='ready' WHERE authority_id=$1`,[d.authorityId]);await assert.rejects(d.consumer.assertPublishedRole(d.policy),{code:'unconfigured'});
   const h=await c.historical();assert.equal(h.original_ack_success,false);assert.equal(h.prepared_role_data.completion_json,null);await assert.rejects(c.publish());assert(!(await c.rows()).completion);result.lostFirstCommitNoAdmission=true;
  });
  await step('real-second-COMMIT-loss-preserves-previously-known-fact-not-original-ACK',async()=>{
   const d=await database('second-loss',graph.policy),c=await campaign(d,budget);await fault(d,{commit:'completion',action:()=>{throw Object.assign(Error('fixture lost completion COMMIT response'),{code:'ECONNRESET'});}},()=>assert.rejects(c.publish(),{code:'effect_unknown'}));
   const rows=await c.rows();assert(rows.role&&rows.completion);assert.equal((await c.historical()).original_ack_success,false);await d.query(`UPDATE ${S}.managed_runtime_role_installation SET state='ready' WHERE authority_id=$1`,[d.authorityId]);await d.consumer.assertPublishedRole(d.policy);
   const before=hash(canonical(rows.completion));await assert.rejects(c.publish());assert.equal(hash(canonical((await c.rows()).completion)),before);result.unknownSecondCommitHistoricalFact=true;
  });
  await step('real-awaited-insert-and-post-COMMIT-original-socket-close',async()=>{
   const d=await database('socket',graph.policy),c=await campaign(d,budget,'before-commit');await fault(d,{query:'INSERT INTO '+S+'.managed_runtime_image_roles',action:()=>close(d,c.socket)},()=>assert.rejects(c.publish(),{code:'effect_unknown'}));assert(!(await c.rows()).role);assert(!(await c.rows()).completion);result.socketCloseBeforeCommit=true;
   const next=await campaign(d,budget,'after-commit');await fault(d,{commit:'prepared',action:()=>close(d,next.socket)},()=>assert.rejects(next.publish(),{code:'effect_unknown'}));assert((await next.rows()).role);assert(!(await next.rows()).completion);await d.query(`UPDATE ${S}.managed_runtime_role_installation SET state='ready' WHERE authority_id=$1`,[d.authorityId]);await assert.rejects(d.consumer.assertPublishedRole(d.policy),{code:'unconfigured'});result.socketCloseAfterCommit=true;
  });
  await step('real-COMMIT-known-after-fixture-provider-lease-expiry-refuses-completion',async()=>{
   const d=await database('lease',graph.policy),c=await campaign(d,budget),now=Date.now;
   try{await fault(d,{commit:'prepared',action:()=>{const wall=now();Date.now=()=>wall+16000;}},()=>assert.rejects(c.publish(),{code:'effect_unknown'}));}finally{Date.now=now;}
   assert(c.enable.remaining()>0,'outer30 remains live: actual fixture provider15s must be the refusal');assert((await c.rows()).role);assert(!(await c.rows()).completion);await d.query(`UPDATE ${S}.managed_runtime_role_installation SET state='ready' WHERE authority_id=$1`,[d.authorityId]);await assert.rejects(d.consumer.assertPublishedRole(d.policy),{code:'unconfigured'});result.providerLeaseExpiryAfterCommit=true;
  });
  await step('expired-original-repeated-HTTP-shaped-call-cannot-mint-new-window',async()=>{
   const d=await database('late',graph.policy),c=await campaign(d,budget);const exactRequest=hash(canonical(c.request,24576));
   // Actual elapsed original initial-registration cutoff, not a copied/new
   // budget or a forged clock. SAME already signed packet is submitted twice.
   await sleep(Math.max(0,c.registrationStarted+31000-performance.now()));
   await assert.rejects(c.publish());await assert.rejects(c.publish());assert(!(await c.rows()).completion);result.expiredOriginalRefused=true;
   assert.equal(hash(canonical(c.request,24576)),exactRequest);
  });
  await step('all-actual-original-registry-carriers-refuse-normal-close',async()=>{
   for(const d of databases)await assert.rejects(d.registry.close(),{code:'effect_unknown'});
   result.retainedOriginalRegistries=databases.length;
  });
  for(const p of pairs)if(!p.closed){
   await close(p.d,p);}
  await bounded(new Promise(resolve=>fixture.ws.close(resolve)),4000);await bounded(new Promise(resolve=>fixture.http.close(resolve)),4000);
  result.actualClosedSocketPairs=pairs.length;result.actualFreshDatabases=databases.length;result.actualRuntimeRolePgQualification=true;result.currentStep='complete-original-registries-retained-until-component-process-teardown';return result;
 }finally{globalThis.fetch=previousFetch;}
}
/** Separate explicit ROOT-RUN entry point. Do not append this to qualify():
 * it is a bounded split under the SAME inherited Node150/guest180/control210.
 * Three fresh DBs, <=6 actual WS pairs; no original policy/case cap grows. */
export async function qualifyActivationOnly({budget,backend,graph}){
 assert(fixture&&backend.evidence.actualS3===false&&databases.length===0&&pairs.length===0);
 result={scope:'actual-PG-original-staging-activation-component-only',role:'runtime',actualStagingActivationPgQualification:false,actualPostgres:true,fixtureRootHmac:true,fixtureImageNativeEvidence:true,fixtureCompilerEvidence:true,fixtureFly:true,fixtureBaseClockAdmission:true,physicalRoot:false,physicalNative:false,physicalCompilerSdk:false,actualS3:false,providerOwnerQualified:false,qualifiedClockContinuity:false,productionEnablement:false,models:false,steps:[]};
 const previousFetch=globalThis.fetch;fixture.fly=flyFetchFixture();globalThis.fetch=fixture.fly.fetch;
 fixture.http=http.createServer();fixture.ws=new WebSocketServer({server:fixture.http,maxPayload:24576,perMessageDeflate:false});retained.push(fixture.http,fixture.ws);fixture.ws.on('error',()=>{});
 await bounded(new Promise(resolve=>fixture.http.listen(0,'127.0.0.1',resolve)),4000);
 const created=[];
 try{
  const checks=await qualifyOriginalStagingActivations({role:'runtime',freshCampaign:async(role,label)=>{
   assert.equal(role,'runtime');assert(created.length<3);result.currentStep='activation-'+label;
   const d=await database('activation-'+label,graph.policy,{activation:true});created.push(d);
   await activateSdkPrecursorRetaining({d,budget,fixture,pair,retained});
   const c=await campaign(d,budget,'activation-'+label);
   return activationCampaignOwner({role,d,c,pg,foreignDatabase:()=>created[0],retained});
  }});
  Object.assign(result,checks);assert.equal(result.checks.length,3);assert.equal(databases.length,3);assert.equal(pairs.length,6);
  for(const d of databases){await assert.rejects(d.registry.close(),{code:'effect_unknown'});await assert.rejects(d.sdkPrecursor.registry.close(),{code:'effect_unknown'});}
  for(const p of pairs)if(!p.closed)await close(p.d,p);
  await bounded(new Promise(resolve=>fixture.ws.close(resolve)),4000);await bounded(new Promise(resolve=>fixture.http.close(resolve)),4000);
  result.actualFreshDatabases=databases.length;result.actualClosedSocketPairs=pairs.length;result.actualStagingActivationPgQualification=true;result.currentStep='activation-complete-original-publishers-admin-unknowns-retained';return result;
 }finally{globalThis.fetch=previousFetch;}
}

export function failureData(){return structuredClone({...result,scope:result?.scope??'actual-PG-runtime-role-and-completion-component-only',actualRuntimeRolePgQualification:result?.actualRuntimeRolePgQualification??false,databaseOutcomes:databases.map(d=>({name:d.name,label:d.label,authority_id:d.authorityId,create_started:d.createStarted,create_known:d.createKnown})),currentStep:result?.currentStep??'before-followup'});}
