// Fixed Root/operator installation for ONE fresh local staging PG cluster.
// No fake Fly/Root transport, image qualification, engine/credential, project,
// retained-root, lease, runtime-role or completion row is inserted here.
import fs from 'node:fs/promises';
import {constants} from 'node:fs';
import path from 'node:path';
import {createHash,randomBytes} from 'node:crypto';
import {createRequire} from 'node:module';
import {createAuthorityMigrator} from '../../cloud/dist/project-authority/index.js';
import {createManagedHomeMigrator,createManagedHomeOperationMigrator,createManagedOwnerRenewalMigrator,createManagedHostTransportMigrator,createManagedRuntimeImageQualificationMigrator,createManagedCompilerSdkQualificationMigrator,createManagedStorageMigrator,createManagedImageBatchMigrator,createManagedProviderOwnershipMigrator} from '../../cloud/dist/managed-home-provisioning/index.js';
import {validatePolicy,canonical,hash} from '../../cloud/dist/managed-home-provisioning/types.js';
import {validateRuntimeQualificationCandidate} from '../../cloud/dist/managed-home-provisioning/runtime-qualification-protocol.js';
import {validateCompilerQualificationCandidate} from '../../cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import {validateStagingCodexAccessSnapshot} from '../../cloud/dist/managed-home-provisioning/staging-codex-profile-import.js';
import {validatePositiveIngressInput} from '../../cloud/dist/managed-home-provisioning/staging-positive-ingress-driver.js';
import {RESTRICTED_ROLE_CHECK} from '../../cloud/dist/project-authority/pool.js';
import {RENEWAL_MIGRATION_SHA256} from '../../cloud/dist/managed-home-provisioning/renewal-schema.js';
const require=createRequire(new URL('../../cloud/package.json',import.meta.url)),pg=require('pg');
const S='rudder_project_authority',P='/run/rudder-runtime-qualification',ROLE={runtime:'rudder_runtime_floor',registration:'rudder_runtime_registration'};
const [mode,work]=process.argv.slice(2);let admin;const retained=[];let out;
const assert=ok=>{if(!ok)throw Error('staging installation refused');};
const exact=(o,keys)=>assert(o&&typeof o==='object'&&!Array.isArray(o)&&Object.keys(o).length===keys.length&&keys.every(k=>Object.hasOwn(o,k)));
const uuid=x=>assert(typeof x==='string'&&/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(x));
// Optional protected operator DATA only. New reviewed activation is required.
// FALSE before accessing its value; no command owner, clock or role is issued.
const GENUINE_OPERATOR_PROJECTION_ALLOWED=false;
function projectGenuineIngressData(plan,flow,image){
 if(!Object.hasOwn(plan,'genuine_ingress'))return undefined;
 assert(GENUINE_OPERATOR_PROJECTION_ALLOWED);assert(flow===true);
 const input=plan.genuine_ingress;exact(input,['experiment','artifact_data']);
 const experiment=input.experiment,artifact=input.artifact_data;
 exact(experiment,['case','experimentNonce','model','profileId']);
 assert(['CloseWins','AdmissionWins','CarrierContention'].includes(experiment.case));uuid(experiment.experimentNonce);
 for(const name of ['model','profileId'])assert(typeof experiment[name]==='string'&&Buffer.byteLength(experiment[name])>0&&Buffer.byteLength(experiment[name])<=256&&!/[\x00-\x1f\x7f]/.test(experiment[name]));
 const keys=['feature','image_digest','integrated_revision','source_manifest_sha256','root_receipt_manifest_sha256','fixture_manifest_sha256','native_elf_sha256','engine_elf_sha256'];
 exact(artifact,keys);for(const name of keys)assert(typeof artifact[name]==='string');
 for(const name of ['source_manifest_sha256','root_receipt_manifest_sha256','fixture_manifest_sha256','native_elf_sha256','engine_elf_sha256'])assert(/^[a-f0-9]{64}$/.test(artifact[name])&&artifact[name]!=='0'.repeat(64));
 assert(/^[a-f0-9]{40}$/.test(artifact.integrated_revision)&&artifact.integrated_revision!=='0'.repeat(40));
 assert(/^sha256:[a-f0-9]{64}$/.test(artifact.image_digest)&&artifact.image_digest!=='sha256:'+'0'.repeat(64));
 assert(typeof image==='string'&&/^[^\s@]+@sha256:[a-f0-9]{64}$/.test(image)&&image.slice(image.lastIndexOf('@')+1)===artifact.image_digest);
 assert(artifact.root_receipt_manifest_sha256==='08ec75f53153899d855c53621a86d2781e89ce7bb12c77f97a55cd386dd0f264'&&artifact.fixture_manifest_sha256==='7a5a41afc55a7569a709d1d2224c3a5b1368ac16731f52f6b0f3411c6d7ecacf');
 const features={CloseWins:'managed-ingress-fixture-close-wins',AdmissionWins:'managed-ingress-fixture-admission-wins',CarrierContention:'managed-ingress-fixture-carrier-contention'};
 assert(artifact.feature===features[experiment.case]);
 // Construct all levels from validated primitive strings; no caller aliases.
 return Object.freeze({experiment:Object.freeze({case:experiment.case,experimentNonce:experiment.experimentNonce,model:experiment.model,profileId:experiment.profileId}),artifact_data:Object.freeze({feature:artifact.feature,image_digest:artifact.image_digest,integrated_revision:artifact.integrated_revision,source_manifest_sha256:artifact.source_manifest_sha256,root_receipt_manifest_sha256:artifact.root_receipt_manifest_sha256,fixture_manifest_sha256:artifact.fixture_manifest_sha256,native_elf_sha256:artifact.native_elf_sha256,engine_elf_sha256:artifact.engine_elf_sha256})});
}
// Positive selection is protected DATA, validated before migrations or driver effects.
function projectPositiveIngressData(plan,flow,genuine){
 if(!Object.hasOwn(plan,'positive_ingress'))return undefined;
 assert(flow===true&&!genuine);
 return validatePositiveIngressInput(plan.positive_ingress);
}
const sig=s=>[s.dev,s.ino,s.mode,s.size,s.mtimeNs,s.ctimeNs].map(String).join(':');
async function readRoot(file,max=262144){const f=await fs.open(file,constants.O_RDONLY|constants.O_CLOEXEC|constants.O_NOFOLLOW);retained.push(f);const before=await f.stat({bigint:true});
 assert(before.isFile()&&before.uid===0n&&before.gid===0n&&before.nlink===1n&&(before.mode&0o7777n)===0o600n&&before.size>0n&&before.size<=BigInt(max));
 const raw=await f.readFile();assert(sig(before)===sig(await f.stat({bigint:true}))&&sig(before)===sig(await fs.lstat(file,{bigint:true})));return raw;}
async function writeJson(file,value){const tmp=file+'.pending',f=await fs.open(tmp,'wx',0o600);retained.push(f);await f.writeFile(JSON.stringify(value,null,2)+'\n');await f.sync();await fs.link(tmp,file);await fs.unlink(tmp);const dir=await fs.open(path.dirname(file),'r');retained.push(dir);await dir.sync();}
const digest=b=>createHash('sha256').update(b).digest('hex');
async function main(){
 assert(process.argv.length===4&&['migrate','admit'].includes(mode)&&process.getuid()===0&&process.getgid()===0&&Object.keys(process.env).length===0);
 assert(typeof work==='string'&&/^\/var\/lib\/rudder-runtime-qualification\/run-[a-f0-9-]{36}$/.test(work));
 const root=await fs.lstat(work);assert(root.isDirectory()&&root.uid===0&&root.gid===0&&(root.mode&0o7777)===0o700);
 const state=JSON.parse(await readRoot(work+'/supervisor.json'));uuid(state.boot_nonce);assert(work.endsWith(state.boot_nonce)&&state.supervisor_pid===process.ppid);
 const pgWork='/var/lib/rudder-runtime-qualification-pg/run-'+state.boot_nonce;assert(state.pg_work===pgWork);
 const raw=await readRoot(P+'/operator.json'),plan=JSON.parse(raw);
 const flow=Object.hasOwn(plan,'staging_flow'),genuine=Object.hasOwn(plan,'genuine_ingress'),positive=Object.hasOwn(plan,'positive_ingress'),profileSnapshot=Object.hasOwn(plan,'positive_profile_snapshot');
 if(genuine)assert(GENUINE_OPERATOR_PROJECTION_ALLOWED); // before optional value access
 exact(plan,['format_version','boot_nonce','qualification_id','authority_id','account_id','command_key','policy','candidate','fly_token',...(flow?['staging_flow']:[]),...(genuine?['genuine_ingress']:[]),...(positive?['positive_ingress']:[]),...(profileSnapshot?['positive_profile_snapshot']:[])]);
 assert(plan.format_version===1&&plan.boot_nonce===state.boot_nonce);uuid(plan.qualification_id);uuid(plan.authority_id);
 assert(typeof plan.account_id==='string'&&/^[A-Za-z0-9_:.@-]{1,180}$/.test(plan.account_id)&&typeof plan.command_key==='string'&&/^[A-Za-z0-9_:.\-]{1,128}$/.test(plan.command_key));
 assert(typeof plan.fly_token==='string'&&plan.fly_token.length>20&&plan.fly_token.length<=16384&&!/[\r\n\0]/.test(plan.fly_token));
 const policy=validatePolicy(plan.policy);assert(policy.controlPlane?.profile==='qualification-v1'&&policy.ownerLeaseSafety===undefined);
 const compilerSdk=Object.hasOwn(plan.candidate??{},'sdk');
 const candidate=compilerSdk?validateCompilerQualificationCandidate(plan.candidate,policy):validateRuntimeQualificationCandidate(plan.candidate,policy);
 const qualificationKind=flow?'sdk-runtime-ordinary-v2':compilerSdk?'compiler-sdk-v1':'runtime-image-v1',policySha=hash(canonical(policy)),candidateSha=hash(canonical(candidate));
 const positiveIngress=projectPositiveIngressData(plan,flow,genuine);
 if(profileSnapshot){assert(positiveIngress&&flow&&!genuine);const validated=validateStagingCodexAccessSnapshot(plan.positive_profile_snapshot,positiveIngress.profileId);validated.fill(0);}
 const genuineIngress=projectGenuineIngressData(plan,flow,policy.image); // shared migrate/admit validation before PG
 let services;
 if(flow){
  assert(Number.isSafeInteger(state.setup_end_boottime_milliseconds)&&state.setup_end_boottime_milliseconds>0);
  exact(plan.staging_flow,['format_version','services_sha256']);assert(plan.staging_flow.format_version===1&&/^[a-f0-9]{64}$/.test(plan.staging_flow.services_sha256));
  assert(compilerSdk&&plan.account_id==='rudder-v4-qualification'&&plan.command_key.length<=96&&policy.ownerMode==='provider-fenced-v1'&&policy.ownerLeaseSafety===undefined&&policy.sdkInitialSource==='sdk-fixed-suite-v1');
  const rawServices=await readRoot(P+'/services.json');assert(digest(rawServices)===plan.staging_flow.services_sha256);services=JSON.parse(rawServices);
  exact(services,['format_version','boot_nonce','authority_id','policy_sha256','candidate_sha256','storageReceiptSha256','s3','serviceCredentials']);
  assert(services.format_version===1&&services.boot_nonce===state.boot_nonce&&services.authority_id===plan.authority_id&&services.policy_sha256===policySha&&services.candidate_sha256===candidateSha&&/^[a-f0-9]{64}$/.test(services.storageReceiptSha256));
  exact(services.s3,['bucket','region','roleArn','webIdentityTokenFile']);assert(services.s3.bucket==='rudder-v4-qualification-597088032164-us-east-1'&&services.s3.region==='us-east-1'&&services.s3.roleArn==='arn:aws:iam::597088032164:role/rudder-v4-qualification-fabric-v1'&&services.s3.webIdentityTokenFile==='/.fly/oidc_token');
  assert(Array.isArray(services.serviceCredentials)&&services.serviceCredentials.length===1);
  const c=services.serviceCredentials[0];exact(c,['accountId','principalId','credentialId','tokenBase64url']);assert(c.accountId===plan.account_id);
  for(const k of ['principalId','credentialId'])assert(typeof c[k]==='string'&&/^[A-Za-z0-9_:.@-]{1,180}$/.test(c[k]));
  const token=Buffer.from(c.tokenBase64url,'base64url');assert(token.length===32&&token.some(x=>x!==0)&&token.toString('base64url')===c.tokenBase64url);
  assert(policy.imagePreseed?.batchTransport==='batch-v1'&&/^[a-f0-9]{64}$/.test(policy.imagePreseed.batchReceiptSha256));
 }
 // Optional admission reuses the SAME protected migration record. Bind its
 // full original operator hash before connecting to PG; defaults keep ordering.
 let genuineMigrated;
 if(genuineIngress!==undefined&&mode==='admit'){
  genuineMigrated=JSON.parse(await readRoot(work+'/migrated.json'));
  assert(genuineMigrated.operator_file_sha256===digest(raw));
 }
 const ca=await fs.readFile(pgWork+'/tls/server.crt','utf8');assert(ca.includes('BEGIN CERTIFICATE'));
 admin=new pg.Client({host:pgWork+'/socket',port:55432,database:'postgres',user:'postgres',application_name:'runtime-floor-staging-operator'});admin.on('error',()=>{});await admin.connect();
 const q=(sql,args=[])=>admin.query(sql,args);
 const identity=(await q(`SELECT current_setting('data_directory') AS data,current_setting('unix_socket_directories') AS socket,current_setting('listen_addresses') AS listen,current_setting('port') AS port,current_setting('ssl') AS ssl,current_setting('fsync') AS fsync,current_setting('synchronous_commit') AS sync,current_setting('full_page_writes') AS full,current_setting('data_checksums') AS checksums,current_setting('server_version_num') AS version,(SELECT system_identifier::text FROM pg_control_system()) AS system_id,to_regnamespace($1)::text AS authority_schema`,[S])).rows[0];
 assert(identity.data===pgWork+'/data'&&identity.socket===pgWork+'/socket'&&identity.listen==='localhost'&&identity.port==='55432'&&Number(identity.version)>=170000&&Number(identity.version)<180000);
 for(const k of ['ssl','fsync','sync','full','checksums'])assert(identity[k]==='on');
 out={format_version:1,scope:'isolated-staging-PG-and-provisioning-operator-only',qualification_kind:qualificationKind,boot_nonce:state.boot_nonce,qualification_id:plan.qualification_id,authority_id:plan.authority_id,policy_sha256:policySha,candidate_sha256:candidateSha,operator_file_sha256:digest(raw),system_id:identity.system_id,server_identity:identity,runtime_role_qualified:false,compiler_sdk_role_qualified:false,engine_enrolled:false,owner_lease_qualified:false,production_enabled:false};
 if(mode==='migrate'){
  assert(identity.authority_schema===null);
  const secrets={admin:randomBytes(24).toString('hex'),runtime:randomBytes(24).toString('hex'),registration:randomBytes(24).toString('hex'),encryption:randomBytes(32).toString('hex')};
  await writeJson(work+'/credentials.json',secrets); // before SQL effects/unknown
  await q(`ALTER ROLE postgres PASSWORD '${secrets.admin}'`);
  for(const [kind,name]of Object.entries(ROLE))await q(`CREATE ROLE ${name} LOGIN PASSWORD '${secrets[kind]}' NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS`);
  const config=(user,password)=>({mode:'postgres',authorityId:plan.authority_id,connection:{host:'localhost',port:55432,database:'postgres',user,password,ca},maxConnections:4,maxQueued:8,connectMs:750,requestMs:10000,statementMs:9000,lockMs:1000,idleTransactionMs:10000});
  const ac=config('postgres',secrets.admin),migrators=[createAuthorityMigrator(ac),createManagedHomeMigrator(ac,policy),createManagedHomeOperationMigrator(ac),
   // Transport needs these tables. SQL hash is explicitly NOT a ClockSafety
   // receipt/policy. Its clock installation stays FROZEN and never admits lease.
   createManagedOwnerRenewalMigrator(ac,RENEWAL_MIGRATION_SHA256),createManagedHostTransportMigrator(ac),createManagedRuntimeImageQualificationMigrator(ac,policy)];
  retained.push(...migrators);
  assert((await migrators[0].migrate(ROLE.runtime)).status==='committed');
  assert((await migrators[1].migrate(ROLE.runtime,ROLE.registration)).status==='committed');
  assert((await migrators[2].migrate(ROLE.runtime,ROLE.registration)).status==='committed');
  assert((await migrators[3].migrate(ROLE.runtime)).status==='committed');
  assert((await migrators[4].migrate(ROLE.registration)).status==='committed');
  assert((await migrators[5].migrate(ROLE.runtime,ROLE.registration)).status==='committed');
  if(compilerSdk){const sdkMigrator=createManagedCompilerSdkQualificationMigrator(ac,policy);retained.push(sdkMigrator);migrators.push(sdkMigrator);
   assert((await sdkMigrator.migrate(ROLE.runtime,ROLE.registration)).status==='committed');}
  if(flow){
   const providerOwner=createManagedProviderOwnershipMigrator(ac);retained.push(providerOwner);migrators.push(providerOwner);assert((await providerOwner.migrate(Object.values(ROLE))).status==='committed');
   const storage=createManagedStorageMigrator(ac,services.storageReceiptSha256);retained.push(storage);migrators.push(storage);assert((await storage.migrate(Object.values(ROLE))).status==='committed');
   const batch=createManagedImageBatchMigrator(ac,policy.imagePreseed.batchReceiptSha256);retained.push(batch);migrators.push(batch);assert((await batch.migrate(Object.values(ROLE))).status==='committed');
  }
  const names=['authorities','managed_home_installation','managed_owner_clock_installation','managed_host_transport_installation','managed_runtime_role_installation'];
  if(compilerSdk)names.push('managed_compiler_role_installation');
  for(const table of names)assert((await q(`SELECT state FROM ${S}.${table} WHERE authority_id=$1`,[plan.authority_id])).rows[0]?.state==='frozen');
  for(const role of Object.values(ROLE)){const safe=await q(RESTRICTED_ROLE_CHECK,[S,role]);assert(safe.rows.length===1&&!safe.rows[0].unsafe);}
  const runtimeConnection=config(ROLE.runtime,secrets.runtime).connection;
  const tls=new pg.Client({...runtimeConnection,ssl:{rejectUnauthorized:true,servername:'localhost',ca}});tls.on('error',()=>{});await tls.connect();
  const tlsObserved=(await tls.query('SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()')).rows[0];assert(tlsObserved?.ssl===true);await tls.end();
  for(const [host,ssl]of [['127.0.0.1',{rejectUnauthorized:true,servername:'127.0.0.1',ca}],['localhost',false]]){
   const wrong=new pg.Client({...runtimeConnection,host,ssl,connectionTimeoutMillis:1000});wrong.on('error',()=>{});let refused=false;
   try{await wrong.connect();}catch{refused=true;}finally{await wrong.end().catch(()=>{});}assert(refused);
  }
  const cfg={format_version:1,qualification_kind:qualificationKind,postgres:{mode:'postgres',authority:config(ROLE.runtime,secrets.runtime),registrationAuthority:config(ROLE.registration,secrets.registration),policy,flyToken:plan.fly_token,tokenEncryptionKeyHex:secrets.encryption},candidate,account_id:plan.account_id,command_key:plan.command_key,...(flow?{activation_authority:ac,services,supervisor:state}:{}),...(genuineIngress===undefined?{}:{genuine_ingress:genuineIngress}),...(positiveIngress===undefined?{}:{positive_ingress:positiveIngress}),...(profileSnapshot?{positive_profile_snapshot:plan.positive_profile_snapshot}:{})};
  // SAME actual driver factory later produces config; no manual guest JSON.
  await writeJson(work+'/driver-input.json',cfg);
  for(const m of migrators)await m.close();await admin.end();await writeJson(work+'/migrated.json',{...out,state:'frozen',lease_schema_dependency_only:RENEWAL_MIGRATION_SHA256});return;
 }
 const migrated=genuineIngress===undefined?JSON.parse(await readRoot(work+'/migrated.json')):genuineMigrated;
 for(const k of ['qualification_kind','boot_nonce','qualification_id','authority_id','policy_sha256','candidate_sha256','operator_file_sha256','system_id'])assert(migrated[k]===out[k]);
 const instruction=JSON.parse(await readRoot(P+'/admission.json'));
 exact(instruction,['format_version','kind','decision','boot_nonce','qualification_id','authority_id','policy_sha256','candidate_sha256']);
 assert(instruction.format_version===1&&instruction.kind==='operator-staging-provisioning-admission-v1'&&instruction.decision===(flow?'admit-staging-sdk-runtime-ordinary-only':'admit-isolated-provisioning-only'));
 for(const k of ['boot_nonce','qualification_id','authority_id','policy_sha256','candidate_sha256'])assert(instruction[k]===out[k]);
 const expected={provider:policy.providerReceiptSha256,durability:policy.durabilityReceiptSha256,'fresh-volume':policy.freshVolumeReceiptSha256,'control-origin':policy.controlPlane.qualificationReceiptSha256,image:policy.runtimeImageReceiptSha256,...(flow?{storage:services.storageReceiptSha256,'image-batch':policy.imagePreseed.batchReceiptSha256}:{})};
 for(const [name,sha]of Object.entries(expected))assert(digest(await readRoot(P+'/receipts/'+name+'.json',1048576))===sha);
 // These files/this instruction are actual Root operator policy admission, NOT
 // automatically verified physical claims. RUN.md requires Root review of their
 // genuine scopes; a3 is not Fly/fresh-volume/image/HA/Root readiness evidence.
 for(const role of Object.values(ROLE)){const safe=await q(RESTRICTED_ROLE_CHECK,[S,role]);assert(safe.rows.length===1&&!safe.rows[0].unsafe);}
 const rights=(await q(`SELECT has_table_privilege($1,$3,'INSERT') AS runtime_can_publish,has_column_privilege($2,$4,'state','UPDATE') AS registration_can_activate`,[ROLE.runtime,ROLE.registration,S+'.managed_runtime_role_completions',S+'.managed_runtime_role_installation'])).rows[0];assert(!rights.runtime_can_publish&&!rights.registration_can_activate);
 if(compilerSdk){const sdkRights=(await q(`SELECT has_table_privilege($1,$3,'INSERT') AS runtime_can_publish,has_column_privilege($2,$4,'state','UPDATE') AS registration_can_activate`,[ROLE.runtime,ROLE.registration,S+'.managed_compiler_role_completions',S+'.managed_compiler_role_installation'])).rows[0];assert(!sdkRights.runtime_can_publish&&!sdkRights.registration_can_activate);}
 await q('BEGIN');
 for(const table of ['authorities','managed_home_installation','managed_host_transport_installation']){const changed=await q(`UPDATE ${S}.${table} SET state='ready' WHERE authority_id=$1 AND state='frozen' RETURNING authority_id`,[plan.authority_id]);assert(changed.rowCount===1);}
 // No image qualification/engine/project/lease/runtime-role/completion
 // enrollment here (full-flow service enrollment is separate below). Actual Root-native and publisher are their only producers.
 if(flow){
  // Explicit operator service enrollment and storage-policy admission only.
  // Actual S3 workload policy readback still runs in the retained flow adapter;
  // this creates NO image publication, project, Source, engine or ownership.
  const c=services.serviceCredentials[0],tokenHash=digest(Buffer.from(c.tokenBase64url,'base64url'));
  await q(`INSERT INTO ${S}.account_principals(authority_id,account_id,principal_id,operations) VALUES($1,$2,$3,$4)`,[plan.authority_id,c.accountId,c.principalId,JSON.stringify(['retention:admit','handoff:admit','project:bootstrap'])]);
  await q(`INSERT INTO ${S}.authority_credentials(account_id,credential_id,principal_id,kind,token_sha256,expires_at) VALUES($1,$2,$3,'service',$4,clock_timestamp()+interval '1 hour')`,[c.accountId,c.credentialId,c.principalId,tokenHash]);
  const storage=await q(`UPDATE ${S}.managed_storage_installation SET state='ready' WHERE authority_id=$1 AND state='frozen' AND storage_receipt_sha256=$2 RETURNING authority_id`,[plan.authority_id,services.storageReceiptSha256]);assert(storage.rowCount===1);
  const batch=await q(`UPDATE ${S}.managed_image_batch_installation SET state='ready' WHERE authority_id=$1 AND state='frozen' AND qualification_receipt_sha256=$2 RETURNING authority_id`,[plan.authority_id,policy.imagePreseed.batchReceiptSha256]);assert(batch.rowCount===1);
 }
 const frozen=['managed_owner_clock_installation','managed_runtime_role_installation'];if(compilerSdk)frozen.push('managed_compiler_role_installation');
 for(const table of frozen)assert((await q(`SELECT state FROM ${S}.${table} WHERE authority_id=$1`,[plan.authority_id])).rows[0]?.state==='frozen');
 await q('COMMIT'); // unknown reply means NO admitted.json / NO driver launch
 await admin.end();await writeJson(work+'/admitted.json',{...out,state:'staging-provisioning-admitted',receipt_sha256:expected});
}
try{await main();}catch{process.stderr.write('STAGING_INSTALL_REFUSED_OR_UNKNOWN\n');process.exitCode=78;}
