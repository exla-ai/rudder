// Ignored by default. REAL isolated PostgreSQL transaction/constraint component.
// Authentication/registration/CAS rows below are synthetic DATA fixtures only;
// this test never constructs a Root/Native/provider owner or qualifies a VM.
import test from'node:test';import assert from'node:assert/strict';import fs from'node:fs/promises';import{randomUUID,randomBytes,createHash}from'node:crypto';import{createRequire}from'node:module';
import{createAuthority,createAuthorityMigrator,createProviderRetirementMigrator,assertOriginalProviderFenceRequest}from'../cloud/dist/project-authority/index.js';
import{createManagedProviderOwnershipMigrator}from'../cloud/dist/managed-home-provisioning/index.js';
const require=createRequire(new URL('../cloud/package.json',import.meta.url)),pg=require('pg'),stateFile=process.env.RUDDER_PROVIDER_OWNER_PG_TEST_STATE;
const sha=value=>createHash('sha256').update(value).digest('hex');
test('real PG provider first-owner exclusivity, elapsed-time non-transfer and old-writer fencing',{skip:!stateFile,timeout:90000},async t=>{
 const st=await fs.lstat(stateFile);assert(st.isFile()&&!st.isSymbolicLink()&&st.nlink===1&&st.uid===process.getuid()&&(st.mode&0o777)===0o600);assert.notEqual(process.getuid(),0,'actual Node fixture must run as the nonroot cluster owner');
 const state=JSON.parse(await fs.readFile(stateFile,'utf8'));assert.match(state.nonce,/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
 const work='/var/tmp/rudder-managed-storage-pg-'+state.nonce;assert.equal(state.work,work);assert.equal(state.socket,work+'/socket');assert.equal(state.data,work+'/data');assert.equal(state.ca,work+'/tls/server.crt');assert.equal(state.host,'localhost');assert.equal(state.port,55432);assert.equal(state.fixtureVariant,'provider-owner-v1');
 assert.equal(await fs.realpath(work),work);assert.equal((await fs.readFile(work+'/qualification-nonce','utf8')).trim(),state.nonce);
 for(const dir of [state.socket,state.data]){const info=await fs.lstat(dir);assert(info.isDirectory()&&!info.isSymbolicLink()&&info.uid===process.getuid()&&(info.mode&0o777)===0o700);}
 const proof={format_version:1,scope:'provider-owner-real-PG-component-only',qualification:state.nonce,fixtureVariant:'provider-owner-v1',executedCases:1,ok:false,
  nonrootFixtureUser:true,syntheticEnrollmentAndCasRows:true,realRootRegistration:false,physicalNative:false,physicalProviderFence:false,actualS3:false,sdkExecution:false,normalRuntimeRetirement:false,productionEnablement:false,models:false};let phase='original-cluster-identity';
 const authorityId=randomUUID(),S='rudder_project_authority',role='provider_owner_component',settings={host:state.socket,port:state.port,user:'postgres',database:'postgres'};
 const ca=await fs.readFile(state.ca,'utf8');assert(ca.includes('BEGIN CERTIFICATE'));const runtimePassword=randomBytes(24).toString('hex');
 const config=user=>({mode:'postgres',authorityId,connection:{host:state.host,port:state.port,database:'postgres',user,password:user==='postgres'?state.adminPassword:runtimePassword,ca},maxConnections:4,maxQueued:8,connectMs:750,requestMs:10000,statementMs:9000,lockMs:1000,idleTransactionMs:10000});
 const admin=new pg.Client(settings);admin.on('error',()=>{});let runtime,migration,providerMigration,retirementMigration;
 t.after(async()=>{let failure;try{await runtime?.close();await providerMigration?.close();await retirementMigration?.close();await migration?.close();await admin.end();proof.fixtureConnectionsClosed=true;}catch(e){proof.ok=false;failure=e;}
  if(!proof.ok)proof.failurePhase=phase;
  const file=await fs.open(work+'/node-proof.json','wx',0o600);try{await file.writeFile(JSON.stringify(proof,null,2)+'\n');await file.sync();}finally{await file.close();}const dir=await fs.open(work,'r');try{await dir.sync();}finally{await dir.close();}if(failure)throw failure;});
 await admin.connect();const identity=(await admin.query("SELECT current_setting('data_directory') AS data,current_setting('unix_socket_directories') AS socket,current_setting('ssl') AS ssl,current_setting('fsync') AS fsync,current_setting('synchronous_commit') AS synchronous_commit,current_setting('full_page_writes') AS full_page_writes,current_user AS user")).rows[0];
 assert.equal(identity.data,state.data);assert.equal(identity.socket,state.socket);assert.equal(identity.user,'postgres');for(const key of ['ssl','fsync','synchronous_commit','full_page_writes'])assert.equal(identity[key],'on');proof.actualPostgres=true;
 assert.equal((await admin.query('SELECT to_regnamespace($1)::text AS n',[S])).rows[0].n,null,'provider variant must run BEFORE any baseline or follow-up schema');proof.freshEmptySchema=true;phase='actual-tls-migrations';
 assert.match(runtimePassword,/^[a-f0-9]{48}$/);await admin.query(`CREATE ROLE ${role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS PASSWORD '${runtimePassword}'`);
 migration=createAuthorityMigrator(config('postgres'));assert.equal((await migration.migrate(role)).status,'committed');providerMigration=createManagedProviderOwnershipMigrator(config('postgres'));assert.equal((await providerMigration.migrate([role])).status,'committed');
 // Fixed minimal authentication DATA tables for this SQL component, NOT the
 // production Root enrollment producer or a serialized capability constructor.
 await admin.query(`CREATE TABLE ${S}.managed_home_fresh_projects(authority_id uuid,account_id text,home_id text,project_id uuid,origin text);
 CREATE TABLE ${S}.managed_home_fresh_preparations(authority_id uuid,account_id text,home_id text,project_id uuid,engine_instance_id uuid,boot_id text);
 CREATE TABLE ${S}.managed_home_intents(authority_id uuid,account_id text,home_id text,phase text,revoked_at timestamptz,intent_json jsonb);
 CREATE TABLE ${S}.managed_home_installation(authority_id uuid,state text,policy_sha256 text);
 CREATE TABLE ${S}.managed_home_registrations(authority_id uuid,account_id text,home_id text,phase text,challenge_json jsonb);
 GRANT SELECT ON ${S}.managed_home_installation,${S}.managed_home_fresh_projects,${S}.managed_home_fresh_preparations,${S}.managed_home_intents,${S}.managed_home_registrations TO ${role};GRANT UPDATE(origin) ON ${S}.managed_home_fresh_projects TO ${role};GRANT UPDATE(phase) ON ${S}.managed_home_intents TO ${role};`);
 runtime=createAuthority(config(role));const api=runtime,account='provider-component',home='mh-'+randomUUID(),engine=randomUUID(),boot=randomUUID(),install=randomUUID(),project=randomUUID(),witness=randomUUID();
 const service={accountId:account,principalId:'service',credentialId:'service-key',credentialSha256:sha('component-service'),engine:{instanceId:engine,bootId:boot,credentialId:'engine-key',credentialSha256:sha('component-engine')}};
 await admin.query(`UPDATE ${S}.authorities SET state='ready' WHERE authority_id=$1`,[authorityId]);
 await admin.query(`INSERT INTO ${S}.authority_credentials(account_id,credential_id,principal_id,kind,token_sha256,expires_at) VALUES($1,'service-key','service','service',$2,clock_timestamp()+interval '1 day'),($1,'engine-key','engine','engine',$3,clock_timestamp()+interval '1 day')`,[account,service.credentialSha256,service.engine.credentialSha256]);
 await admin.query(`INSERT INTO ${S}.engine_instances(account_id,engine_instance_id,machine_id,machine_installation_id,machine_boot_id,protocol_version,authenticated_key_id)VALUES($1,$2,'original-machine',$3,$4,1,'engine-key')`,[account,engine,install,boot]);
 await admin.query(`INSERT INTO ${S}.account_principals(authority_id,account_id,principal_id,operations)VALUES($1,$2,'service','["project:bootstrap","handoff:admit","retention:admit","verification:admit"]')`,[authorityId,account]);
 const binding={app_name:'original-app',network_name:'original-network',machine_id:'original-machine',instance_id:'original-instance',incarnation:install,store_id:randomUUID(),config_sha256:'a'.repeat(64),machine_config_sha256:'b'.repeat(64)};
 await admin.query(`INSERT INTO ${S}.managed_home_fresh_projects VALUES($1,$2,$3,$4,'fresh-v4-empty');`,[authorityId,account,home,project]);
 await admin.query(`INSERT INTO ${S}.managed_home_fresh_preparations VALUES($1,$2,$3,$4,$5,$6)`,[authorityId,account,home,project,engine,boot]);
 await admin.query(`INSERT INTO ${S}.managed_home_intents VALUES($1,$2,$3,'routable',NULL,$4)`,[authorityId,account,home,JSON.stringify({policy:{ownerMode:'provider-fenced-v1'}})]);
 await admin.query(`INSERT INTO ${S}.managed_home_installation VALUES($1,'ready',$2)`,[authorityId,sha(JSON.stringify({ownerMode:'provider-fenced-v1'}))]);
 await admin.query(`INSERT INTO ${S}.managed_home_registrations VALUES($1,$2,$3,'enrolled',$4)`,[authorityId,account,home,JSON.stringify({body:{binding,engine_instance_id:engine}})]);
 const known=x=>{assert.equal(x.status,'committed');return x.value;};for(const[rootId,kind]of[['source','source'],['environment','environment'],['handoff','handoff']])known(await api.admitRetainedRoot(service,{rootId,kind,digest:sha(rootId),sizeBytes:'1',retentionReceipt:{sqlComponentDataOnly:true}}));
 known(await api.admitHandoff(service,{projectId:project,witnessId:witness,rootId:'handoff',previousOwnerEpoch:'0',previousEngineInstanceId:null,legacyOrigin:'fresh-v4:'+project,processesQuiesced:true,effectsReconciled:true,launchersDisabled:true,credentialsFenced:true}));
 phase='provider-bootstrap';
 known(await api.bootstrapProviderProject(service,{projectId:project,projectKey:'fresh-v4:'+project,idempotencyKey:'bootstrap',witnessId:witness,head:'base',sourceRoot:'source',environmentRoot:'environment',topics:['policy/verification']}));
 phase='first-owner-concurrency';
 const tls=(await admin.query("SELECT a.usename,bool_and(s.ssl) AS ssl FROM pg_stat_activity a JOIN pg_stat_ssl s USING(pid) WHERE a.datname='postgres' AND a.application_name='rudder-project-authority' GROUP BY a.usename")).rows;assert(tls.some(r=>r.usename==='postgres'&&r.ssl===true));assert(tls.some(r=>r.usename===role&&r.ssl===true));proof.verifiedTLSFactories=true;
 const acquire=key=>({projectId:project,idempotencyKey:key,expectedOwnerEpoch:'1',witnessId:witness});
 const raced=await Promise.allSettled([api.acquireProviderOwner(service,acquire('first-a')),api.acquireProviderOwner(service,acquire('first-b'))]);assert.equal(raced.filter(r=>r.status==='fulfilled'&&r.value.status==='committed').length,1);assert.equal(raced.filter(r=>r.status==='rejected').length,1);proof.firstOwnerExclusivity=true;
 const accepted=raced.find(r=>r.status==='fulfilled').value.value,grant=accepted.data;assert.equal(grant.ownershipMode,'provider-fenced-v1');assert.equal(grant.ownerEpoch,'2');assert(!('leaseUntil'in grant));assert(!('durationMs'in grant));
 assert.deepEqual(known(await api.acquireProviderOwner(service,acquire(grant.requestId))),accepted,'same original idempotency result is immutable DATA, no Native owner constructed');proof.immutableReceiptRetry=true;phase='old-writer-refusal';
 await assert.rejects(api.acquireOwner(service,acquire('old-writer')),{code:'stale_fence'});
 await assert.rejects(api.renewOwner(service,{projectId:project,ownerEpoch:'2',leaseId:grant.ownershipId,requestId:randomUUID(),expectedSerial:'0'}),{code:'stale_fence'});
 await assert.rejects(api.releaseOwner(service,{projectId:project,ownerEpoch:'2',leaseId:grant.ownershipId,idempotencyKey:'release',witnessId:witness}),{code:'stale_fence'});
 await assert.rejects(api.acceptDecision(service,{projectId:project,ownerEpoch:'2',leaseId:grant.ownershipId,idempotencyKey:'legacy-decision',topic:'policy/verification',expectedVersion:'0',statement:'wrong mode'}),{code:'stale_fence'});proof.oldApisRefused=true;
 const raw=new pg.Client({host:state.host,port:state.port,database:'postgres',user:role,password:runtimePassword,ssl:{rejectUnauthorized:true,servername:state.host,ca}});raw.on('error',()=>{});await raw.connect();try{await raw.query('BEGIN');await raw.query("SELECT set_config('rudder.account_id',$1,true)",[account]);await assert.rejects(raw.query(`UPDATE ${S}.projects SET lease_id=$2,lease_until=clock_timestamp()+interval '1 day' WHERE project_id=$1`,[project,randomUUID()]));await raw.query('ROLLBACK');}finally{await raw.end();}proof.oldSqlLeaseWriterRefused=true;phase='elapsed-time-does-not-regrant';
 // No heartbeat for longer than the old15s lease. Nothing becomes available.
 await admin.query('SELECT pg_sleep(16)');await assert.rejects(api.acquireProviderOwner(service,acquire('time-cannot-regrant')),{code:'handoff_required'});proof.elapsedTimeNoRegrant=true;
 const op={projectId:project,ownershipMode:'provider-fenced-v1',ownerEpoch:'2',ownershipId:grant.ownershipId,requestId:randomUUID(),expectedSerial:'0'};const heartbeat=known(await api.heartbeatProviderOwner(service,op));assert.equal(heartbeat.heartbeatSerial,'1');assert.equal(heartbeat.ownershipId,grant.ownershipId);assert.deepEqual(known(await api.heartbeatProviderOwner(service,op)),heartbeat);proof.sameOwnerHeartbeat=true;phase='immutable-mode-and-unsupported-replacement';
 retirementMigration=createProviderRetirementMigrator(config('postgres'));assert.equal((await retirementMigration.migrate([role])).status,'committed');
 const row=(await admin.query(`SELECT owner_epoch::text,owner_engine_instance,ownership_id,lease_until,provider_heartbeat_serial::text FROM ${S}.projects WHERE project_id=$1`,[project])).rows[0];assert.equal(row.ownership_id,grant.ownershipId);assert.equal(row.owner_epoch,'2');assert.equal(row.owner_engine_instance,engine);assert.equal(row.lease_until,null);
 await assert.rejects(admin.query(`UPDATE ${S}.projects SET ownership_mode='clock-lease-v1' WHERE project_id=$1`,[project]));proof.modeChangeRefused=true;
 await assert.rejects(api.acquireProviderOwner(service,{...acquire('replacement'),expectedOwnerEpoch:'2'}),{code:'handoff_required'});proof.replacementRefused=true;
 await assert.rejects(api.heartbeatProviderOwner(service,{...op,requestId:randomUUID(),ownershipId:randomUUID(),expectedSerial:'1'}),{code:'stale_fence'});proof.wrongOwnershipRefused=true;
 phase='restriction-only-provider-retirement';
 await admin.query(`UPDATE ${S}.account_principals SET operations=operations||'["owner:fence"]'::jsonb WHERE authority_id=$1 AND account_id=$2 AND principal_id='service'`,[authorityId,account]);
 const retirementRequest={projectId:project,idempotencyKey:'retire-original',ownershipMode:'provider-fenced-v1',ownerEpoch:'2',ownershipId:grant.ownershipId,action:'replace'};
 // Synthetic decision DATA satisfy the real topic FKs. These rows grant no
 // decision/owner authority; valid inserts isolate the retirement trigger.
 const topicInputs=(await admin.query(`SELECT operation_id,accepted_seq FROM ${S}.idempotency WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 ORDER BY accepted_seq LIMIT 2`,[authorityId,account,project])).rows;assert.equal(topicInputs.length,2);
 for(const [n,topic]of ['existing-retirement-guard','blocked-after-retirement'].entries())await admin.query(`INSERT INTO ${S}.decisions(authority_id,account_id,project_id,decision_id,topic,version,principal_id,scope,statement,project_seq) VALUES($1,$2,$3,$4,$5,1,'service','project','{}'::jsonb,$6)`,[authorityId,account,project,topicInputs[n].operation_id,topic,topicInputs[n].accepted_seq]);
 assert.equal((await admin.query(`INSERT INTO ${S}.topics(authority_id,account_id,project_id,topic,version,decision_id) VALUES($1,$2,$3,'existing-retirement-guard',1,$4)`,[authorityId,account,project,topicInputs[0].operation_id])).rowCount,1);
 const retired=known(await api.beginProviderOwnerRetirement(service,retirementRequest));assert.equal(retired.data.physicalFenceObserved,false);assert.equal(retired.data.effectsReconciled,false);assert.equal(retired.data.ownerTransferred,false);assert.equal(retired.data.inventory.complete,false);
 const frozen=(await admin.query(`SELECT mode,ownership_mode,owner_epoch::text,owner_engine_instance,ownership_id,provider_binding,provider_retirement_id FROM ${S}.projects WHERE project_id=$1`,[project])).rows[0];
 assert.equal(frozen.mode,'frozen');assert.equal(frozen.ownership_mode,'provider-fenced-v1');assert.equal(frozen.owner_epoch,'2');assert.equal(frozen.ownership_id,grant.ownershipId);assert.equal(frozen.owner_engine_instance,engine);assert.deepEqual(frozen.provider_binding,grant.provider);assert.equal(frozen.provider_retirement_id,retired.data.retirementId);
 await assert.rejects(api.acquireProviderOwner(service,acquire(grant.requestId)),{code:'handoff_required'});await assert.rejects(api.heartbeatProviderOwner(service,{...op,requestId:randomUUID(),expectedSerial:'1'}),{code:'stale_fence'});
 await assert.rejects(admin.query(`UPDATE ${S}.projects SET mode='ready',provider_retirement_id=NULL WHERE project_id=$1`,[project]),{code:'P0001'});
 await assert.rejects(admin.query(`UPDATE ${S}.projects SET owner_epoch=3 WHERE project_id=$1`,[project]));
 await assert.rejects(admin.query(`INSERT INTO ${S}.topics(authority_id,account_id,project_id,topic,version,decision_id) VALUES($1,$2,$3,'blocked-after-retirement',1,$4)`,[authorityId,account,project,topicInputs[1].operation_id]),{code:'P0001'});await assert.rejects(admin.query(`DELETE FROM ${S}.topics WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND topic='existing-retirement-guard'`,[authorityId,account,project]),{code:'P0001'});assert.equal((await admin.query(`SELECT 1 FROM ${S}.topics WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND topic='existing-retirement-guard'`,[authorityId,account,project])).rowCount,1);proof.retirementAdmissionClosedWithoutTransfer=true;
 // Reopened/historical known receipt remains DATA and cannot recreate the
 // original fencing request owner, even with an actual configured PG pool.
 const reopened=createAuthority(config(role));try{assert.deepEqual(known(await reopened.beginProviderOwnerRetirement(service,retirementRequest)),retired);assert.throws(()=>reopened.takeOriginalProviderFenceRequest(service,project,retirementRequest.idempotencyKey),{code:'handoff_required'});}finally{await reopened.close();}
 const originalRequest=api.takeOriginalProviderFenceRequest(service,project,retirementRequest.idempotencyKey);assertOriginalProviderFenceRequest(originalRequest);assert.throws(()=>JSON.stringify(originalRequest));await originalRequest.assertPendingOriginal();
 assert.throws(()=>originalRequest.takeOriginalDispatchData(),{code:'handoff_required'});assert.throws(()=>api.takeOriginalProviderFenceRequest(service,project,retirementRequest.idempotencyKey),{code:'handoff_required'});proof.originalRetirementRequestNotReconstructed=true;
 // Actual server COMMIT completes, then ONLY its original reply is lost.
 const previousQuery=pg.Client.prototype.query,dispatchPhase=new WeakSet();let commitObserved=false;
 pg.Client.prototype.query=function(query,...args){const sql=typeof query==='string'?query:query?.text,actual=previousQuery.call(this,query,...args),eligible=this.connectionParameters.user===role&&this.connectionParameters.database==='postgres';
  if(eligible&&sql?.startsWith(`UPDATE ${S}.provider_owner_retirements SET state='fencing'`))return actual.then(value=>{assert.equal(value.rowCount,1);dispatchPhase.add(this);return value;});
  if(eligible&&sql==='COMMIT'&&dispatchPhase.has(this))return actual.then(()=>{commitObserved=true;throw Object.assign(Error('fixture lost actual fencing-intent COMMIT reply'),{code:'ECONNRESET'});});return actual;};
 try{await assert.rejects(originalRequest.startDispatchRetaining(),{code:'handoff_required'});assert(commitObserved);}finally{pg.Client.prototype.query=previousQuery;}
 assert.equal((await admin.query(`SELECT state FROM ${S}.provider_owner_retirements WHERE retirement_id=$1`,[retired.data.retirementId])).rows[0].state,'fencing');
 assert.throws(()=>originalRequest.takeOriginalDispatchData(),{code:'handoff_required'});await assert.rejects(originalRequest.startDispatchRetaining(),{code:'handoff_required'});await assert.rejects(originalRequest.assertPendingOriginal(),{code:'handoff_required'});proof.unknownDispatchNoFenceAuthority=true;
 // A second actual first-owned project tests the known dispatch-intent path.
 // Enrollment/CAS/initial-handoff remain the SAME explicit component fixtures;
 // no positive physical-fence or replacement witness is created.
 phase='known-dispatch-intent-remains-unproven';const second=randomUUID(),secondWitness=randomUUID();
 await admin.query(`INSERT INTO ${S}.managed_home_fresh_projects VALUES($1,$2,$3,$4,'fresh-v4-empty')`,[authorityId,account,home,second]);
 await admin.query(`INSERT INTO ${S}.managed_home_fresh_preparations VALUES($1,$2,$3,$4,$5,$6)`,[authorityId,account,home,second,engine,boot]);
 known(await api.admitHandoff(service,{projectId:second,witnessId:secondWitness,rootId:'handoff',previousOwnerEpoch:'0',previousEngineInstanceId:null,legacyOrigin:'fresh-v4:'+second,processesQuiesced:true,effectsReconciled:true,launchersDisabled:true,credentialsFenced:true}));
 known(await api.bootstrapProviderProject(service,{projectId:second,projectKey:'fresh-v4:'+second,idempotencyKey:'second-bootstrap',witnessId:secondWitness,head:'base',sourceRoot:'source',environmentRoot:'environment',topics:['policy/verification']}));
 const secondGrant=known(await api.acquireProviderOwner(service,{projectId:second,idempotencyKey:'second-first',expectedOwnerEpoch:'1',witnessId:secondWitness})).data;
 known(await api.beginProviderOwnerRetirement(service,{projectId:second,idempotencyKey:'second-retire',ownershipMode:'provider-fenced-v1',ownerEpoch:secondGrant.ownerEpoch,ownershipId:secondGrant.ownershipId,action:'release'}));
 const secondRequest=api.takeOriginalProviderFenceRequest(service,second,'second-retire');await secondRequest.startDispatchRetaining();await secondRequest.assertPendingOriginal();
 const dispatch=secondRequest.takeOriginalDispatchData();assert.equal(dispatch.scope,'original-provider-fence-request-only');assert.equal(dispatch.physicalFenceObserved,false);assert.equal(dispatch.retirement.ownerEpoch,secondGrant.ownerEpoch);assert.equal(dispatch.retirement.ownershipId,secondGrant.ownershipId);assert.equal(dispatch.retirement.action,'release');assert.equal(dispatch.retirement.ownerTransferred,false);
 assert.throws(()=>secondRequest.takeOriginalDispatchData(),{code:'handoff_required'});await secondRequest.startDispatchRetaining(); // same known promise, no second DB dispatch
 assert.equal((await admin.query(`SELECT owner_epoch::text,ownership_id,mode FROM ${S}.projects WHERE project_id=$1`,[second])).rows[0].mode,'frozen');proof.knownDispatchIntentNotPhysicalFence=true;
 phase='completed';proof.ok=true;
});
