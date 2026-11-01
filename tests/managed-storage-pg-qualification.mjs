// ROOT-RUN ONLY. Real disposable PostgreSQL/TLS/roles/transactions; fixture
// provider and Root evidence. No public Context/test issuer in production.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import http from 'node:http';
import {createRequire} from 'node:module';
import {createHmac,randomBytes,randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {pathToFileURL} from 'node:url';
import {createAuthorityMigrator} from '../cloud/dist/project-authority/index.js';
import {createManagedHomeRegistry,createManagedHomeMigrator,createManagedHomeOperationMigrator,createManagedStorageMigrator} from '../cloud/dist/managed-home-provisioning/index.js';
import {canonical} from '../cloud/dist/managed-home-provisioning/types.js';
import {S3FabricObjectStorage,FabricCas,canonicalFabricMetadata} from '../cloud/dist/fabric-cas.js';
import {handleManagedStorageHttp} from '../cloud/dist/managed-home-provisioning/storage-http.js';
import {TRANSPORT_DOMAINS} from '../cloud/dist/managed-home-provisioning/transport.js';
import {memoryS3,flyFetchFixture,componentGraph,hash} from './managed-storage-pg-fixtures.mjs';
const require=createRequire(new URL('../cloud/package.json',import.meta.url)),pg=require('pg');
const S='rudder_project_authority';
const statePath=process.env.RUDDER_MANAGED_STORAGE_PG_STATE;
assert(statePath,'explicit private fixture state required');
let phase='state-preflight',proof,admin,server,retention;
const owners=[]; // Original real registries remain held until fixture teardown.
const steps=[];
const step=async(name,work)=>{phase=name;const at=performance.now();const value=await work();steps.push({name,passed:true,milliseconds:Math.ceil(performance.now()-at)});return value;};
const safeCode=e=>typeof e?.code==='string'&&/^[a-zA-Z0-9_]{1,40}$/.test(e.code)?e.code:'assertion';
const sign=(key,domain,body)=>createHmac('sha256',key).update(domain).update(canonical(body,24576)).digest('hex');
const packet=(key,domain,body)=>({body,mac:sign(key,domain,body)});
async function privateJson(file,value){const pending=file+'.pending-'+randomUUID();const owned=await fs.open(pending,'wx',0o600);
  try{await owned.writeFile(JSON.stringify(value,null,2)+'\n');await owned.sync();}finally{await owned.close();}
  await fs.link(pending,file);await fs.unlink(pending);const dir=await fs.open(path.dirname(file),'r');try{await dir.sync();}finally{await dir.close();}}
function budget(maximum){const before=performance.now();const initial=fs.readFile('/proc/uptime','utf8');return initial.then(raw=>{
  const m=/^(\d+)\.(\d{2}) /.exec(raw);assert(m);const lower=BigInt(m[1])*1000000000n+BigInt(m[2])*10000000n;
  const constraint={format_version:1,maximum_milliseconds:maximum,boottime_deadline_nanoseconds:String(lower+BigInt(maximum)*1000000n)};
  return {constraint,remaining(){const n=Math.floor(before+maximum-performance.now()-10);assert(n>0,'fixture original budget expired');return n;}};});}
async function fixtureModule(input){assert(path.isAbsolute(input.path)&&input.path===await fs.realpath(input.path));const st=await fs.lstat(input.path);
  assert(st.isFile()&&st.nlink===1&&(st.mode&0o022)===0&&[0,process.getuid()].includes(st.uid));assert.match(input.sha256,/^[a-f0-9]{64}$/);assert.equal(hash(await fs.readFile(input.path)),input.sha256);
  return import(pathToFileURL(input.path).href);}
async function lostCommit(user,predicate,work){
  const original=pg.Client.prototype.query,armed=new WeakSet();let injected=false;
  pg.Client.prototype.query=function(query,...args){const sql=typeof query==='string'?query:query?.text;
    if(this.connectionParameters.user===user&&predicate(sql,args))armed.add(this);
    const result=original.call(this,query,...args);
    if(sql==='COMMIT'&&armed.has(this)){armed.delete(this);if(!injected)return result.then(()=>{injected=true;throw Object.assign(new Error('fixture lost acknowledgement'),{code:'ECONNRESET'});});}
    if(sql==='ROLLBACK')armed.delete(this);return result;};
  try{return {value:await work(),injected};}finally{pg.Client.prototype.query=original;}
}

async function qualify(){
  const st=await fs.lstat(statePath);assert(st.isFile()&&!st.isSymbolicLink()&&st.nlink===1&&st.uid===process.getuid()&&(st.mode&0o777)===0o600);
  const state=JSON.parse(await fs.readFile(statePath,'utf8'));assert.match(state.nonce,/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/);
  const roleActivationOnly=state.fixtureVariant==='role-activation-v1';
  assert([undefined,'managed-storage-v3','role-activation-v1'].includes(state.fixtureVariant));
  assert.equal(state.roleActivationOnly??false,roleActivationOnly);
  if(roleActivationOnly){
    assert(['sdk','runtime'].includes(state.selectedRole));assert.equal(state.backendModule,undefined);
    const module=state.selectedRole==='sdk'?'managed-compiler-sdk-role-pg-followup.mjs':'managed-runtime-role-pg-followup.mjs';
    assert.equal(state.followupModule?.path,'/var/tmp/rudder-authority-node-runtime/tests/'+module);
  }else assert.equal(state.selectedRole,undefined);
  const work='/var/tmp/rudder-managed-storage-pg-'+state.nonce;
  assert.equal(state.work,work);assert.equal(state.socket,work+'/socket');assert.equal(state.data,work+'/data');assert.equal(state.host,'localhost');assert.equal(state.port,55432);
  assert.equal((await fs.readFile(work+'/qualification-nonce','utf8')).trim(),state.nonce);assert.equal((await fs.stat(state.socket)).mode&0o777,0o700);
  assert.equal((await fs.stat(state.socket)).uid,process.getuid());
  const ca=await fs.readFile(state.ca,'utf8');assert(ca.includes('BEGIN CERTIFICATE'));
  const authorityId=randomUUID(),account='fixture-a:'+state.nonce,otherAccount='fixture-b:'+state.nonce;
  const role={runtime:'rudder_managed_runtime',registration:'rudder_managed_registration',storage:'rudder_managed_runtime'};
  const passwords=Object.fromEntries([...new Set(Object.values(role))].map(r=>[r,randomBytes(24).toString('hex')]));
  const config=user=>({mode:'postgres',authorityId,connection:{host:state.host,port:state.port,database:'postgres',user,password:user==='postgres'?state.adminPassword:passwords[user],ca},
    maxConnections:4,maxQueued:8,connectMs:750,requestMs:10000,statementMs:9000,lockMs:1000,idleTransactionMs:10000});
  admin=new pg.Client({host:state.socket,port:state.port,user:'postgres',database:'postgres',application_name:'managed-pg-fixture-operator'});admin.on('error',()=>{});await admin.connect();
  const q=(sql,args=[])=>admin.query(sql,args),count=async(table,where='',args=[])=>Number((await q(`SELECT count(*)::text AS n FROM ${S}.${table} ${where}`,args)).rows[0].n);
  // Reject accidental/shared targets BEFORE CREATE ROLE or any migration.
  const serverIdentity=(await q(`SELECT current_setting('data_directory') AS data,current_setting('unix_socket_directories') AS socket,current_setting('listen_addresses') AS listen,
    current_setting('port') AS port,current_setting('ssl') AS ssl,current_setting('fsync') AS fsync,current_setting('synchronous_commit') AS sync,current_setting('full_page_writes') AS full,
    current_setting('data_checksums') AS checksums,current_setting('server_version_num') AS version,to_regnamespace($1)::text AS authority_schema`,[S])).rows[0];
  assert.equal(serverIdentity.data,state.data);assert.equal(serverIdentity.socket,state.socket);assert.equal(serverIdentity.listen,'localhost');assert.equal(serverIdentity.port,'55432');
  for(const k of ['ssl','fsync','sync','full','checksums'])assert.equal(serverIdentity[k],'on');assert(Number(serverIdentity.version)>=170000&&Number(serverIdentity.version)<180000);assert.equal(serverIdentity.authority_schema,null);
  const socketStat=await fs.lstat(state.socket+'/.s.PGSQL.55432');assert(socketStat.isSocket()&&socketStat.uid===process.getuid());
  const followup=state.followupModule?await fixtureModule(state.followupModule):undefined;
  if(roleActivationOnly){assert.equal(typeof followup?.prepareFixturePolicy,'function');assert.equal(typeof followup.qualifyActivationOnly,'function');}
  const graph=componentGraph(canonicalFabricMetadata);const preparedFollowup=followup?.prepareFixturePolicy?await followup.prepareFixturePolicy({policy:structuredClone(graph.policy),authorityId,config,query:q}):undefined;
  if(preparedFollowup){assert(preparedFollowup.policy&&typeof preparedFollowup.policy==='object');graph.policy=preparedFollowup.policy;}
  const storageReceipt=state.backendModule?.storageReceiptSha256??hash('fixture storage qualification only');assert.match(storageReceipt,/^[a-f0-9]{64}$/);
  let backend;
  if(state.backendModule){
    const b=state.backendModule;assert(path.isAbsolute(b.path)&&b.path===await fs.realpath(b.path));const bs=await fs.lstat(b.path);assert(bs.isFile()&&bs.nlink===1&&[0,process.getuid()].includes(bs.uid)&&(bs.mode&0o022)===0);assert.match(b.sha256,/^[a-f0-9]{64}$/);
    assert.equal(hash(await fs.readFile(b.path)),b.sha256);const module=await import(pathToFileURL(b.path).href);
    backend=await module.createStorage({S3FabricObjectStorage,nonce:state.nonce});assert(backend.storage instanceof S3FabricObjectStorage);assert.equal(backend.evidence.actualS3,true);assert.equal(backend.evidence.scope,'explicit-root-supplied-qualified-S3-prefix');
  }else backend=memoryS3(S3FabricObjectStorage);
  const serviceCredentials=[account,otherAccount].map(accountId=>({accountId,principalId:'fixture-storage-service',credentialId:'fixture-storage-service',token:randomBytes(32)}));
  const fakeFly=flyFetchFixture(),originalFetch=globalThis.fetch;globalThis.fetch=fakeFly.fetch;
  const make=(authority=config(role.runtime),credentials=serviceCredentials)=>{const peer=user=>({...config(user),connection:{...config(user).connection,host:authority.connection.host,ca:authority.connection.ca}});const registry=createManagedHomeRegistry({mode:'postgres',authority,registrationAuthority:peer(role.registration),policy:graph.policy,flyToken:'synthetic-managed-pg-provider-only',tokenEncryptionKey:Buffer.alloc(32,23),
      freshStorage:{authority:{...authority,connection:{...authority.connection}},storage:backend.storage,storageReceiptSha256:storageReceipt,serviceCredentials:credentials}});owners.push(registry);return registry;};
  let migrationsClosed=false;const migrations=[createAuthorityMigrator(config('postgres')),createManagedHomeMigrator(config('postgres'),graph.policy),createManagedHomeOperationMigrator(config('postgres')),createManagedStorageMigrator(config('postgres'),storageReceipt)];
  proof={format_version:1,scope:'managed-home-v1-v2-v3-real-PG-component-only',qualification:state.nonce,roleActivationOnly,selectedRole:state.selectedRole,actualPostgres:true,verifiedTLS:false,fixtureOperatorEnrollment:true,
    fixtureFlyTransport:true,realRootRegistration:false,realRootKernelWitness:false,actualFabricCas:false,objectBackend:backend.evidence,productionStartup:false,Spec3:false,models:false,
    acceptedJournalAuthority:false,fleetHA:false,RPO:false,normalRetirementBlocked:false,serverIdentity,steps};
  try{
    await step('actual-factories-migrate-frozen-and-reject-privileged-runtime',async()=>{
      for(const name of new Set(Object.values(role)))await q(`CREATE ROLE ${name} LOGIN PASSWORD '${passwords[name]}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS`);
      await assert.rejects(migrations[0].migrate('postgres'),{code:'role_rejected'});assert.equal((await q('SELECT to_regnamespace($1)::text AS value',[S])).rows[0].value,null);
      for(const name of new Set([role.runtime,role.storage]))assert.equal((await migrations[0].migrate(name)).status,'committed');
      assert.equal((await migrations[1].migrate(role.runtime,role.registration)).status,'committed');assert.equal((await migrations[2].migrate(role.runtime,role.registration)).status,'committed');
      assert.equal((await migrations[3].migrate([role.storage])).status,'committed');
      if(preparedFollowup?.installAfterMigrations)await preparedFollowup.installAfterMigrations({authorityId,config,query:q});
      for(const table of ['authorities','managed_home_installation','managed_storage_installation'])assert.equal((await q(`SELECT state FROM ${S}.${table}`)).rows[0].state,'frozen');
      const before={...fakeFly.counts};await assert.rejects(make().provisionManagedHome(account,'frozen-attempt',['Frozen fixture']),{code:'unconfigured'});assert.deepEqual(fakeFly.counts,before);
      assert.equal(await count('managed_home_intents'),0);
    });
    await step('TLS-and-runtime-role-boundaries-stay-production-strict',async()=>{
      assert.throws(()=>createManagedHomeRegistry({mode:'postgres',authority:{...config(role.runtime),connection:{...config(role.runtime).connection,host:state.socket}},policy:graph.policy,flyToken:'synthetic-managed-pg-provider-only',tokenEncryptionKey:Buffer.alloc(32,23)}),{code:'invalid_request'});
      await assert.rejects(make(config('postgres')).provisionManagedHome(account,'unsafe-role',['Unsafe']),{code:'unavailable'});
      const wrongHost={...config(role.runtime),connection:{...config(role.runtime).connection,host:'127.0.0.1'}};
      await assert.rejects(make(wrongHost).provisionManagedHome(account,'bad-hostname',['Bad hostname']),{code:'unavailable'});
      const client=new pg.Client({...config(role.runtime).connection,ssl:false});client.on('error',()=>{});await assert.rejects(client.connect());await client.end().catch(()=>{});
      const rows=(await q(`SELECT a.usename,s.ssl FROM pg_stat_activity a JOIN pg_stat_ssl s USING(pid) WHERE a.application_name='rudder-managed-home-provisioning'`)).rows;
      assert(rows.some(r=>r.ssl));assert(rows.every(r=>r.ssl));proof.tlsObservedSessions=rows.map(r=>({role:r.usename,tls:r.ssl}));proof.verifiedTLS=true;
    });
    // Explicit fixture administrator activation ONLY. No Cloud activation API.
    await q(`UPDATE ${S}.authorities SET state='ready' WHERE authority_id=$1`,[authorityId]);await q(`UPDATE ${S}.managed_home_installation SET state='ready' WHERE authority_id=$1`,[authorityId]);
    await q(`UPDATE ${S}.managed_storage_installation SET state='ready' WHERE authority_id=$1`,[authorityId]);
    await q(`INSERT INTO ${S}.managed_image_qualifications VALUES($1,$2,$3,$4,$5)`,[authorityId,graph.policy.image,graph.policy.runtimeImageReceiptSha256,graph.image.manifest.digest,JSON.stringify(graph.image.installer)]);
    for(const c of serviceCredentials){await q(`INSERT INTO ${S}.authority_credentials(account_id,credential_id,principal_id,kind,token_sha256,expires_at) VALUES($1,$2,$3,'service',$4,clock_timestamp()+interval '1 day')`,[c.accountId,c.credentialId,c.principalId,hash(c.token)]);
      await q(`INSERT INTO ${S}.account_principals(authority_id,account_id,principal_id,operations) VALUES($1,$2,$3,$4)`,[authorityId,c.accountId,c.principalId,JSON.stringify(['retention:admit','handoff:admit','project:bootstrap'])]);}
    const registry=make();let receipt;
    await step('real-provision-commit-loss-reconciles-original-eight-effects-once',async()=>{
      const result=await lostCommit(role.runtime,sql=>sql?.includes("managed_home_effects SET state='acknowledged'"),async()=>{
        await assert.rejects(registry.provisionManagedHome(account,'home-original',['Active fixture','Unknown started','Concurrent fixture','Mixed followup fixture']),{code:'effect_unknown'});return true;});assert(result.injected);
      assert.equal(fakeFly.counts.app,1);receipt=await registry.reconcileProvisioning(account,'home-original');assert.equal(receipt.phase,'routable');
      assert.deepEqual(await registry.provisionManagedHome(account,'home-original',['Active fixture','Unknown started','Concurrent fixture','Mixed followup fixture']),receipt);assert.equal(fakeFly.counts.app,1);assert.equal(fakeFly.counts.machine,1);
      assert.equal(await count('managed_home_effects',"WHERE state='acknowledged'"),8);assert.equal(await count('managed_home_intents'),1);
      await assert.rejects(registry.provisionManagedHome(account,'home-original',['Changed request']),{code:'conflict'});assert.equal(receipt.nativeIngressReady,false);assert.equal(receipt.taskReady,false);
    });
    const otherReceipt=await registry.provisionManagedHome(otherAccount,'home-other',['Other tenant']);
    const home=fakeFly.forHome(receipt.homeId),otherHome=fakeFly.forHome(otherReceipt.homeId);assert(home&&otherHome);
    const token=home.secrets.get('RUDDER_HOST_TOKEN_V1'),rootKey=Buffer.from(home.secrets.get('RUDDER_ROOT_REGISTRATION_V1').trim(),'hex');
    const hostConfig=home.secrets.get('RUDDER_HOST_CONFIG_V1'),worker=await registry.authenticateManagedHomeWorker(receipt.homeId,token);
    const otherWorker=await registry.authenticateManagedHomeWorker(otherReceipt.homeId,otherHome.secrets.get('RUDDER_HOST_TOKEN_V1'));
    let challenge,registrationAck,credential;
    await step('actual-restricted-registration-producer-and-exact-identity',async()=>{
      await assert.rejects(registry.beginOriginalEngineRegistration({},{}),{code:'identity_changed'});
      const original=await budget(30000);const body={version:1,kind:'begin',home_id:receipt.homeId,incarnation:JSON.parse(hostConfig).incarnation,config_sha256:hash(hostConfig),root_nonce:randomUUID(),constraint:original.constraint};
      await assert.rejects(registry.beginOriginalEngineRegistration(worker,{body,mac:'f'.repeat(64)}),{code:'identity_changed'});assert.equal(await count('managed_home_registrations'),0);
      challenge=await registry.beginOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_begin_v1\0',body));
      assert.deepEqual(await registry.beginOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_begin_v1\0',body)),challenge);
      const ns=Object.fromEntries(['mnt','user','net','pid'].map(k=>[k,{device:'1',inode:'9007199254740994'}]));
      const proc=(pid,image)=>({pid,start_ticks:'9007199254740995',image_sha256:image,exe:{device:'1',inode:String(pid+9000)},namespaces:ns});
      const evidence={boot_id:randomUUID(),root:proc(101,graph.policy.launcherSha256),engine:proc(102,graph.policy.nativeSha256),service:proc(103,graph.policy.serviceSha256),host_worker_tid:104,host_worker_start_ticks:'9007199254740996',root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true};
      const proofBody={...challenge.body,kind:'proof',evidence};const wrong=structuredClone(proofBody);wrong.binding.image='registry.fly.io/foreign@sha256:'+'f'.repeat(64);
      await assert.rejects(registry.finishOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_proof_v1\0',wrong)),{code:'identity_changed'});
      assert.equal(await count('engine_instances'),0);
      const admitted=await registry.finishOriginalEngineRegistration(worker,packet(rootKey,'rudder_engine_registration_proof_v1\0',proofBody));registrationAck=admitted.acknowledgementData();await admitted.assertOriginalCurrent();
      assert.equal(registrationAck.mac,sign(rootKey,'rudder_engine_registration_ack_v1\0',registrationAck.body));assert.equal(registrationAck.body.proof_sha256,hash(canonical(proofBody,24576)));
      assert.equal(await count('engine_instances'),1);const derivation=Object.fromEntries(['challenge_id','engine_instance_id','credential_id','root_nonce'].map(k=>[k,challenge.body[k]]));
      credential=Buffer.from('rdre_'+sign(rootKey,'rudder_engine_registration_credential_v1\0',derivation));
      assert.equal((await q(`SELECT token_sha256 FROM ${S}.authority_credentials WHERE credential_id=$1`,[challenge.body.credential_id])).rows[0].token_sha256,hash(credential));
      proof.syntheticRegistrationProofSha256=registrationAck.body.proof_sha256;
    });
    const makeOperation=(operation,key,time)=>{const a=registrationAck.body,b=a.binding;const body={version:1,kind:'engine_operation',account_id:b.account_id,home_id:b.home_id,store_id:b.store_id,incarnation:b.incarnation,engine_instance_id:a.engine_instance_id,boot_id:a.boot_id,credential_id:a.credential_id,epoch:b.epoch,root_nonce:a.root_nonce,registration_proof_sha256:a.proof_sha256,
      request_nonce:randomUUID(),command_key:key,operation,payload_sha256:hash(canonical(operation)),constraint:time.constraint,remaining_milliseconds:time.remaining()};return packet(credential,'rudder_engine_operation_request_v1\0',body);};
    const perform=async input=>{const ack=await registry.performOriginalEngineOperation(worker,input);assert.equal(ack.mac,sign(credential,'rudder_engine_operation_ack_v1\0',ack.body));assert.equal(ack.body.request_sha256,hash(canonical(input.body,24576)));return ack;};
    // Hash-bound qualifier-only Root simulation. This closes over the ORIGINAL
    // fixture registration/credential; neither secret nor a production issuer
    // leaves this scope. Only the four fixed transport request phases can sign.
    const fixtureTransport=Object.freeze({
      signRequest(phase,fields){
        const allowed={begin:['serial','generation','attempt_nonce','constraint','remaining_milliseconds'],
          commit:['serial','generation','attempt_nonce','constraint','challenge_nonce','begin_sha256','native_acceptance_sha256'],
          confirm:['serial','generation','attempt_nonce','commit_sha256'],reconcile:['serial','generation','attempt_nonce','requests']};
        assert(Object.hasOwn(allowed,phase));assert.deepEqual(Object.keys(fields).sort(),[...allowed[phase]].sort());
        const body={version:1,kind:'host_transport_'+phase,registration:structuredClone(registrationAck.body),...JSON.parse(canonical(fields,16384).toString('utf8'))};
        canonical(body,16384);return packet(credential,TRANSPORT_DOMAINS[phase],body);
      },
      verifyResponse(phase,input){
        assert(['challenge','ack'].includes(phase));assert.deepEqual(Object.keys(input).sort(),['body','mac']);
        assert.equal(input.body.version,1);assert.equal(input.body.kind,'host_transport_'+phase);
        assert.deepEqual(input.body.registration,registrationAck.body);canonical(input.body,16384);
        assert.equal(input.mac,sign(credential,TRANSPORT_DOMAINS[phase],input.body));return structuredClone(input.body);
      },
      async dropCommitAck(phase,work){
        assert(['begin','commit'].includes(phase));assert.equal(typeof work,'function');
        const needle=phase==='begin'?'INSERT INTO '+S+'.managed_host_transports ':'UPDATE '+S+".managed_host_transports SET state='committed'";
        return lostCommit(role.registration,sql=>sql?.includes(needle),work);
      },
      async withCredentialRowsHeld(work,whileHeld){
        assert.equal(typeof work,'function');assert.equal(typeof whileHeld,'function');
        const originalQuery=pg.Client.prototype.query;let entered,release,claimed=false,cleanup;
        const enteredPromise=new Promise(resolve=>{entered=resolve;}),gate=new Promise(resolve=>{release=resolve;});
        pg.Client.prototype.query=function(query,...args){const sql=typeof query==='string'?query:query?.text;const actual=originalQuery.call(this,query,...args);
          if(!claimed&&this.connectionParameters.user===role.registration&&sql?.includes('managed_home_registrations r JOIN')&&sql.includes('FOR SHARE OF r,e,c,h,a,g')){
            claimed=true;return actual.then(async value=>{entered({backendPid:this.processID});await gate;return value;});}return actual;};
        const actualWork=Promise.resolve().then(work);actualWork.catch(()=>{});let timer;
        try{const original=await Promise.race([enteredPromise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('fixture original credential rows not acquired')),3000);})]);
          cleanup=await whileHeld(original);assert.equal(typeof cleanup,'function');release();await cleanup();cleanup=undefined;
          return {value:await actualWork,heldOriginalCredentialRows:claimed};
        }finally{release();clearTimeout(timer);if(cleanup)await cleanup().catch(()=>{});try{await actualWork;}catch{}pg.Client.prototype.query=originalQuery;}
      },
    });
    const active=receipt.projects[0],unknown=receipt.projects[1],concurrent=receipt.projects[2];let preparation;
    await step('no-raw-context-wrong-tenant-epoch-or-boot-operation-admission',async()=>{
      const good=makeOperation({op:'FreshProjectStatusV1',workspace_id:active.workspaceId},'status-auth',await budget(30000));const before=await count('managed_home_engine_operations');
      for(const [field,value] of [['account_id',otherAccount],['epoch','2'],['boot_id',randomUUID()]]){const changed=structuredClone(good.body);changed[field]=value;await assert.rejects(perform(packet(credential,'rudder_engine_operation_request_v1\0',changed)));}
      await assert.rejects(perform({...good,Context:{trusted:true}}));assert.equal(await count('managed_home_engine_operations'),before);
      const status=await perform(good);assert.equal(status.body.result.scope,'fresh-project-status-only');assert.equal(await count('managed_home_engine_operations'),before);
    });
    await step('independent-factories-serialize-same-key-on-real-home-row-lock',async()=>{
      const replica=make(),replicaWorker=await replica.authenticateManagedHomeWorker(receipt.homeId,token);
      const blocker=new pg.Client({host:state.socket,port:state.port,user:'postgres',database:'postgres'});blocker.on('error',()=>{});await blocker.connect();
      const request=makeOperation({op:'PrepareFreshProjectV1',workspace_id:concurrent.workspaceId},'prepare-concurrent',await budget(30000));
      await blocker.query('BEGIN');await blocker.query(`SELECT 1 FROM ${S}.managed_home_intents WHERE account_id=$1 AND home_id=$2 FOR UPDATE`,[account,receipt.homeId]);
      const blockerPid=(await blocker.query('SELECT pg_backend_pid() AS pid')).rows[0].pid;
      const one=registry.performOriginalEngineOperation(worker,request),two=replica.performOriginalEngineOperation(replicaWorker,request),settled=Promise.allSettled([one,two]);
      let observed=[];
      try{for(let i=0;i<200;i++){const r=await q("SELECT pid,wait_event_type,wait_event,pg_blocking_pids(pid) AS blockers FROM pg_stat_activity WHERE usename=$1 AND application_name='rudder-managed-home-provisioning' AND wait_event_type='Lock'",[role.registration]);
          // PostgreSQL may queue the second waiter behind the first waiter.
          // Both must reach this original lock through the observed wait graph.
          const blocked=new Set([blockerPid]);let changed=true;
          while(changed){changed=false;for(const row of r.rows)if(!blocked.has(row.pid)&&row.blockers.some(pid=>blocked.has(pid))){blocked.add(row.pid);changed=true;}}
          observed=r.rows.filter(v=>blocked.has(v.pid));if(observed.length>=2)break;await new Promise(resolve=>setTimeout(resolve,10));}
        assert(observed.length>=2,'both actual factories must overlap under original row lock');proof.realSessionBlockers=observed;
      }finally{await blocker.query('COMMIT');await blocker.end();}
      const results=await settled,good=results.filter(v=>v.status==='fulfilled');assert(good.length>=1);assert.equal(good[0].value.body.error,null);
      for(const r of good)assert.deepEqual(r.value,good[0].value);
      assert.equal(await count('managed_home_fresh_preparations','WHERE project_id=$1',[concurrent.projectId]),1);
    });
    await step('known-effect-lost-COMMIT-ACK-reconciles-without-second-preparation',async()=>{
      const before=await count('managed_home_fresh_preparations');const request=makeOperation({op:'PrepareFreshProjectV1',workspace_id:active.workspaceId},'prepare-original',await budget(30000));
      const lost=await lostCommit(role.registration,sql=>sql?.includes("managed_home_engine_operations SET state='replied'"),async()=>{await assert.rejects(perform(request),{code:'effect_unknown'});});assert(lost.injected);
      assert.equal(await count('managed_home_fresh_preparations'),before+1);const ack=await perform(request);assert.equal(ack.body.error,null);preparation=ack.body.result;assert.equal(preparation.assignment_epoch,'0');
      assert.deepEqual(await perform(request),ack);assert.equal(await count('managed_home_fresh_preparations'),before+1);
    });
    await step('unknown-Started-commit-never-dispatches-and-remains-unknown',async()=>{
      const request=makeOperation({op:'PrepareFreshProjectV1',workspace_id:unknown.workspaceId},'prepare-unknown-started',await budget(30000));
      const before=await count('managed_home_fresh_preparations');const lost=await lostCommit(role.registration,sql=>sql?.includes('INSERT INTO '+S+'.managed_home_engine_operations'),async()=>{await assert.rejects(perform(request),{code:'effect_unknown'});});assert(lost.injected);
      await assert.rejects(perform(request),{code:'effect_unknown'});assert.equal(await count('managed_home_fresh_preparations'),before);
      const row=(await q(`SELECT state FROM ${S}.managed_home_engine_operations WHERE command_key=$1`,[request.body.command_key])).rows[0];assert.equal(row.state,'started');proof.unknownStartedRetained=true;
    });
    await step('RLS-and-restricted-role-grants-on-real-runtime-connections',async()=>{
      const raw=new pg.Client({...config(role.runtime).connection,ssl:{rejectUnauthorized:true,servername:'localhost',ca}});raw.on('error',()=>{});await raw.connect();
      try{for(const table of ['managed_home_intents','managed_home_registrations','managed_home_fresh_preparations','managed_storage_transfers']){
          // No trusted account setting means RLS hides tenant rows. SELECT for
          // supported storage/runtime login still has tenant RLS, never DDL.
          assert.equal((await raw.query(`SELECT count(*)::text AS n FROM ${S}.${table}`)).rows[0].n,'0');}
        await raw.query('BEGIN');await raw.query("SELECT set_config('rudder.account_id',$1,true)",[otherAccount]);assert.equal((await raw.query(`SELECT count(*)::text AS n FROM ${S}.managed_home_intents`)).rows[0].n,'1');
        assert.equal((await raw.query(`SELECT count(*)::text AS n FROM ${S}.managed_home_registrations`)).rows[0].n,'0');await raw.query('ROLLBACK');
        for(const sql of [`UPDATE ${S}.managed_home_installation SET state='ready'`,`UPDATE ${S}.managed_storage_installation SET state='ready'`,`DELETE FROM ${S}.managed_image_qualifications`,`INSERT INTO ${S}.engine_instances DEFAULT VALUES`,`INSERT INTO ${S}.authority_credentials DEFAULT VALUES`,`SET ROLE ${role.registration}`])await assert.rejects(raw.query(sql),{code:'42501'});
      }finally{await raw.end();}
    });
    const liveHttp=new Set();let settledHttp=0;
    const requestHttp=async(homeId,bearer,route,method,body,contentType)=>new Promise((resolve,reject)=>{
      const held={request:null,response:null,socket:null,requestClosed:false,responseClosed:true,socketClosed:true,status:0,bytes:[],size:0,value:null,error:false};liveHttp.add(held);
      const complete=()=>{if(!held.requestClosed||!held.responseClosed||!held.socketClosed)return;liveHttp.delete(held);settledHttp++;
        if(held.error&&held.status===200)return reject(Error('fixture successful response incomplete'));
        resolve({status:held.error?0:held.status,body:held.value,transport_closed:held.error});};
      const req=http.request({host:'127.0.0.1',port:server.address().port,path:`/api/rudder/home/${homeId}/fresh-roots/${route}`,method,agent:false,
        headers:{authorization:'Bearer '+bearer,'content-type':contentType,'content-length':String(body.length)}},res=>{
          held.response=res;held.responseClosed=false;held.status=res.statusCode;res.on('data',b=>{held.size+=b.length;if(held.size>65536){held.error=true;req.destroy();}else held.bytes.push(b);});
          res.on('end',()=>{try{held.value=JSON.parse(Buffer.concat(held.bytes));}catch{held.error=true;}});res.on('error',()=>{held.error=true;});res.on('aborted',()=>{held.error=true;});
          res.once('close',()=>{held.responseClosed=true;complete();});});
      held.request=req;req.once('socket',socket=>{held.socket=socket;held.socketClosed=false;socket.once('close',()=>{held.socketClosed=true;complete();});});
      req.setTimeout(5000,()=>{held.error=true;req.destroy();});req.on('error',()=>{held.error=true;});req.once('close',()=>{held.requestClosed=true;complete();});req.end(body);
    });
    server=http.createServer((req,res)=>{handleManagedStorageHttp(req,res,new URL(req.url,'https://rudder-cloud-control.fly.dev'),registry).catch(()=>{res.destroy();});});await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
    const missing=(transfer,objects,homeId=receipt.homeId,bearer=token)=>requestHttp(homeId,bearer,transfer+'/missing','POST',Buffer.from(JSON.stringify({format_version:1,objects})),'application/json');
    const put=(transfer,item)=>requestHttp(receipt.homeId,token,`${transfer}/objects/${item.ref.kind}/${item.ref.digest}/${item.ref.size_bytes}`,'PUT',item.bytes,'application/octet-stream');
    const fixtureBatch=Object.freeze({
      put:async(transfer,header,objects)=>{assert.equal(header.transfer_id,transfer);assert(objects.every(o=>Buffer.isBuffer(o.bytes)));const raw=canonical(header,262144),length=Buffer.alloc(4);length.writeUInt32BE(raw.length);const body=Buffer.concat([length,raw,...header.objects.map(ref=>{const original=objects.find(v=>v.ref.kind===ref.kind&&v.ref.digest===ref.digest&&v.ref.size_bytes===ref.size_bytes);assert(original);return original.bytes;})]);assert(body.length<=4+262144+33554432);return requestHttp(receipt.homeId,token,transfer+'/batches/'+header.batch_id,'PUT',body,'application/vnd.rudder.image-batch-v1');},
      reconcile:(transfer,header)=>requestHttp(receipt.homeId,token,transfer+'/batches/'+header.batch_id+'/reconcile','POST',Buffer.from(JSON.stringify({format_version:1,request_nonce:header.request_nonce,header_sha256:hash(canonical(header,262144))})),'application/json'),
      lostCommit:(phase,work)=>{assert(['started','ack'].includes(phase));return lostCommit(role.runtime,sql=>phase==='started'?sql?.includes('INSERT INTO '+S+'.managed_image_batches'):sql?.includes("managed_image_batches SET state='replied'"),work);},
    });
    let imageTransfer;
    if(followup?.qualifyColdImage){proof.coldImageBatch=await step('actual-cold-image-batch-original-producers',async()=>{try{return await followup.qualifyColdImage({registry,worker,account,active,concurrent,operation:makeOperation,perform,budget,query:q,backend,graph,batch:fixtureBatch});}catch(error){if(followup.failureData)proof.separateFollowup=JSON.parse(canonical(followup.failureData(),24576).toString('utf8'));throw error;}});imageTransfer=proof.coldImageBatch.transfer_id;proof.imagePublished=true;proof.actualFabricCas=true;}else{
    await step('actual-streaming-image-CAS-publication-and-qualified-graph',async()=>{
      const original=await budget(300000);const begin=makeOperation({op:'BeginImagePreseedV1',workspace_id:active.workspaceId,manifest:graph.image.manifest,installer_input:graph.image.installer,image_receipt_sha256:graph.policy.runtimeImageReceiptSha256},'image-begin',original);
      await q(`UPDATE ${S}.managed_storage_installation SET state='frozen'`);const frozen=structuredClone(begin.body);frozen.command_key='image-frozen';frozen.request_nonce=randomUUID();
      const prior=await count('managed_storage_transfers');assert.equal((await perform(packet(credential,'rudder_engine_operation_request_v1\0',frozen))).body.error,'unconfigured');assert.equal(await count('managed_storage_transfers'),prior);
      await q(`UPDATE ${S}.managed_storage_installation SET state='ready'`);
      const wrong=structuredClone(begin.body);wrong.operation.image_receipt_sha256='f'.repeat(64);wrong.payload_sha256=hash(canonical(wrong.operation));wrong.command_key='image-wrong-pin';wrong.request_nonce=randomUUID();assert.notEqual((await perform(packet(credential,'rudder_engine_operation_request_v1\0',wrong))).body.error,null);
      const ack=await perform(begin);assert.equal(ack.body.error,null);imageTransfer=ack.body.result.transfer_id;
      assert.equal(ack.body.result.maximum_manifest_bytes,33554432);
      const absent=await missing(imageTransfer,graph.image.objects.map(i=>i.ref));assert.equal(absent.status,200);assert.equal(absent.body.missing.length,graph.image.objects.length);
      for(const item of graph.image.objects){const uploaded=await put(imageTransfer,item);assert.equal(uploaded.status,200);assert.deepEqual(uploaded.body.object,item.ref);}
      const duplicate=await put(imageTransfer,graph.image.objects[0]);assert.notEqual(duplicate.status,200);
      const published=await perform(makeOperation({op:'PublishImagePreseedV1',workspace_id:active.workspaceId,transfer_id:imageTransfer},'image-publish',original));assert.equal(published.body.error,null);assert.deepEqual(published.body.result.installer_input,graph.image.installer);
      assert.equal(await count('managed_image_publications'),1);proof.imagePublished=true;proof.actualFabricCas=true;
    });
    }
    let transfer,acquired,acquirePacket;
    await step('actual-source-env-publish-service-fresh-bootstrap-and-acquire',async()=>{
      const original=await budget(30000);const begin=makeOperation({op:'BeginFreshRootsV1',workspace_id:active.workspaceId,prepare_id:preparation.prepare.request_id,export_id:randomUUID(),head:'a'.repeat(40),source:graph.source,environment:graph.environment},'fresh-begin',original);
      const b=await perform(begin);assert.equal(b.body.error,null);transfer=b.body.result.transfer_id;
      const all=await missing(transfer,graph.all.map(i=>i.ref));assert.equal(all.status,200);assert(all.body.preseeded.length>=graph.image.objects.length-1);
      const other=await missing(transfer,[graph.source],otherReceipt.homeId,otherHome.secrets.get('RUDDER_HOST_TOKEN_V1'));assert.notEqual(other.status,200);
      const wanted=new Set(all.body.missing.map(r=>r.kind+':'+r.digest));for(const item of graph.all)if(wanted.has(item.ref.kind+':'+item.ref.digest))assert.equal((await put(transfer,item)).status,200);
      const publish=makeOperation({op:'PublishFreshRootsV1',workspace_id:active.workspaceId,transfer_id:transfer},'fresh-publish',original);const pub=await perform(publish);assert.equal(pub.body.error,null);assert.equal(pub.body.result.scope,'fresh-bootstrap-only');
      assert.deepEqual(await perform(publish),pub);assert.equal(await count('projects'),1);assert.equal(await count('handoff_witnesses'),1);assert.equal(await count('retained_roots'),3);
      const acquire=makeOperation({op:'AcquireFreshOwnerV1',workspace_id:active.workspaceId,transfer_id:transfer,acquire_key:'original-acquire',expected_owner_epoch:'1'},'fresh-acquire',original);
      const result=await perform(acquire);
      // Bounded diagnostic DATA only. Preserve the strict success assertion;
      // never copy a credential/packet/SQL/provider cause into failure output.
      proof.baseAcquireErrorCode=result.body.error===null?null:['unconfigured','expired','busy','rejected'].includes(result.body.error)?result.body.error:'unknown-code';
      assert.equal(result.body.error,null);acquirePacket=result;acquired=result.body.result.owner_receipt;assert.equal(acquired.data.ownerEpoch,'2');
      assert.deepEqual(await perform(acquire),result);assert.equal((await q(`SELECT owner_epoch::text AS epoch FROM ${S}.projects`)).rows[0].epoch,'2');proof.bootstrapAndOwner=true;
      const enrolled=await perform(makeOperation({op:'EnrollFreshProjectV1',workspace_id:active.workspaceId},'active-original-assignment',original));assert.equal(enrolled.body.error,null);assert.equal(enrolled.body.result.owner_epoch,'2');
      const route=await registry.resolveManagedHomeRoute(account,receipt.homeId,active.workspaceId);await route.assertRoutableCurrent();
      const cas=new FabricCas({storage:backend.storage,accountId:account});assert((await cas.closure(graph.source)).objects.some(r=>r.kind===graph.source.kind&&r.digest===graph.source.digest));
      assert.deepEqual((await cas.getRoot(pub.body.result.source_publication.root_id)).root,graph.source);assert.deepEqual((await cas.getRoot(pub.body.result.environment_publication.root_id)).root,graph.environment);
      const otherCas=new FabricCas({storage:backend.storage,accountId:otherAccount});await assert.rejects(otherCas.getObject(graph.source));
    });
    proof.v4MixedQualification=false;
    proof.hostTransportPgQualification=false;
    if(state.followupModule){await step('explicit-Root-supplied-separate-followup',async()=>{
      // Fixed export selection only; both paths inherit the SAME original budget.
      const qualifyFollowup=roleActivationOnly?followup.qualifyActivationOnly:followup.qualify;
      try{proof.separateFollowup=await qualifyFollowup.call(followup,{registry,worker,account,active,workspace:receipt.projects[3],initialAcquire:acquirePacket,operation:makeOperation,perform,budget,query:q,backend,graph,missing,put,fixtureTransport});}
      catch(error){if(followup.failureData)proof.separateFollowup=JSON.parse(canonical(followup.failureData(),24576).toString('utf8'));throw error;}
      proof.v4MixedQualification=proof.separateFollowup?.actualMixedPgQualification===true;
    });}
    await step('independent-service-credential-is-required-after-real-CAS-publication',async()=>{
      const bad=make(config(role.runtime),serviceCredentials.map(c=>({...c,token:randomBytes(32)}))),badWorker=await bad.authenticateManagedHomeWorker(receipt.homeId,token);
      const time=await budget(30000),op={op:'BeginFreshRootsV1',workspace_id:concurrent.workspaceId,prepare_id:concurrent.prepareId??(await q(`SELECT prepare_id::text FROM ${S}.managed_home_fresh_projects WHERE project_id=$1`,[concurrent.projectId])).rows[0].prepare_id,
        export_id:randomUUID(),head:'b'.repeat(40),source:graph.source,environment:graph.environment};
      const begin=await bad.performOriginalEngineOperation(badWorker,makeOperation(op,'bad-service-begin',time));assert.equal(begin.body.error,null);
      const prior=await count('projects'),publish=makeOperation({op:'PublishFreshRootsV1',workspace_id:concurrent.workspaceId,transfer_id:begin.body.result.transfer_id},'bad-service-publish',time);
      // Existing actual CAS bytes are account-scoped and verified. A Root MAC
      // cannot replace independently checked PG service admission credentials.
      await assert.rejects(bad.performOriginalEngineOperation(badWorker,publish),{code:'effect_unknown'});
      assert.equal(await count('projects'),prior);proof.wrongServiceCredentialRefused=true;
    });
    await step('revoked-engine-fences-new-authenticated-effects',async()=>{
      await q(`UPDATE ${S}.authority_credentials SET revoked_at=clock_timestamp() WHERE account_id=$1 AND credential_id=$2`,[account,challenge.body.credential_id]);
      await assert.rejects(perform(makeOperation({op:'FreshProjectStatusV1',workspace_id:active.workspaceId},'after-revoke',await budget(30000))));
    });
    if(followup?.afterEngineRevocation){await step('separate-followup-original-engine-revocation-boundary',async()=>{
      await followup.afterEngineRevocation();proof.hostTransportPgQualification=proof.separateFollowup?.actualTransportPgQualification===true;
    });}
    await step('actual-registry-retirement-refuses-held-registration-carriers',async()=>{await assert.rejects(registry.close(),{code:'effect_unknown'});proof.normalRetirementBlocked=true;proof.originalRegistriesRetained=owners.length;});
    assert.equal(liveHttp.size,0);proof.httpRequestsActuallySettled=settledHttp;
    for(const m of migrations)await m.close();migrationsClosed=true;
    proof.ok=true;proof.completedAt=new Date().toISOString();proof.normalCleanupPerformed=false;proof.retainedUntilFixtureProcessTeardown=true;
    await privateJson(work+'/node-proof.json',proof);
    // Keep actual owners, pools, HTTP server and the live runner until Root's
    // original fixture Child owner performs explicit process/VM teardown.
    retention=setInterval(()=>{void owners.length;},1000);
    process.stdout.write(JSON.stringify({scope:proof.scope,complete_retained:true,normal_retirement_blocked:true})+'\n');
  }finally{
    globalThis.fetch=originalFetch;
    // Migrators are independent, settled fixture resources. Registry owners
    // are deliberately not forged/closed/dropped as a cleanup success proof.
    if(!migrationsClosed)await Promise.allSettled(migrations.map(m=>m.close()));
  }
}
qualify().catch(async error=>{
  const location=/managed-storage-pg-qualification\.mjs:(\d+):(\d+)/.exec(String(error?.stack||''));
  const failed={...(proof??{}),ok:false,phase,code:safeCode(error),testSourceLocation:location?{line:Number(location[1]),column:Number(location[2])}:null,steps,normalCleanupPerformed:false,productionStartup:false,Spec3:false,models:false,realRootRegistration:false};
  if(statePath){try{const s=JSON.parse(await fs.readFile(statePath,'utf8'));await privateJson(s.work+'/node-proof.json',failed);}catch{}}
  // No raw PostgreSQL/provider/S3 cause, configuration or credential output.
  process.stdout.write(JSON.stringify({qualification_failed:true,phase,code:failed.code})+'\n');
  retention=setInterval(()=>{void owners.length;},1000);process.exitCode=1;
});
