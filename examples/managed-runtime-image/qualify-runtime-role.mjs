#!/usr/bin/env node
// SOURCE ONLY / UNRUN. Trusted staging control process, not a managed VM Root
// launcher. Root deploys it at the fixed HTTPS staging origin and owns ALL
// actual Fly/PG/VM cleanup. No credential/token/config contents go to stdout.
import * as fs from 'node:fs/promises';
import {constants,readSync} from 'node:fs';
import path from 'node:path';
import {randomUUID} from 'node:crypto';
import {StagingSdkRuntimeFlow} from '../../cloud/dist/managed-home-provisioning/staging-sdk-runtime-flow.js';
import {S3FabricObjectStorage} from '../../cloud/dist/fabric-cas.js';
import {readManagedHomeStartupConfig} from '../../cloud/dist/managed-home-provisioning/startup-config.js';
import {originalTime} from '../../cloud/dist/managed-home-provisioning/fly.js';
import {canonical,hash} from '../../cloud/dist/managed-home-provisioning/types.js';
import {RuntimeImageQualificationDriver} from '../../cloud/dist/managed-home-provisioning/runtime-qualification-driver.js';
import {CompilerSdkQualificationDriver} from '../../cloud/dist/managed-home-provisioning/compiler-qualification-driver.js';

let driver;let output;let storage;let startupCheck='private-input';let outputFailure=false;let statusBusy=false;
// Closed diagnostic DATA only. Never serialize input, exception text, provider
// metadata or role identifiers, and never repeat a failed storage readback.
function startupFailureData(check,ownersRetained,retention){
 const checks=['private-input','input-schema','output-directory','flow-process','supervisor-data','setup-clock','services-data','storage-construction','storage-retention','startup-config','flow-construction','flow-start','initial-status'];
 const states=['unconfigured','checking','qualified','failed'];
 return {format_version:1,error:'qualification_driver_start_failed',owners_retained:ownersRetained===true,configured_activation:false,
  startup_check:checks.includes(check)?check:'unclassified',
  storage_retention:{state:states.includes(retention?.state)?retention.state:'unavailable',io_settled:typeof retention?.io_settled==='boolean'?retention.io_settled:null}};
}
const retain=new Set(); // original config FD/driver/promises retained on errors
const exact=(o,names)=>{if(!o||typeof o!=='object'||Array.isArray(o)||Object.keys(o).length!==names.length||names.some(k=>!Object.hasOwn(o,k)))throw Error();};
// Optional identity/restriction DATA only; no role/owner/clock authority.
function validateSupervisorBirthData(supervisor){
 const withBirth=Object.hasOwn(supervisor??{},'birth_selection');
 exact(supervisor,['format_version','boot_nonce','supervisor_pid','pg_work','setup_end_boottime_milliseconds',...(withBirth?['birth_selection']:[])]);
 if(!withBirth)return; // Existing five-field controls remain compatible.
 if(supervisor.format_version!==1||typeof supervisor.boot_nonce!=='string'||!/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(supervisor.boot_nonce))throw Error();
 const birth=supervisor.birth_selection;
 if(birth?.source==='generated-v1'){exact(birth,['source']);return;}
 exact(birth,['source','kind','sha256','bytes','dev','ino']);
 if(birth.source!=='protected-birth-file-v1'||birth.kind!=='operator-planned-staging-birth-v1'||typeof birth.sha256!=='string'||!/^[a-f0-9]{64}$/.test(birth.sha256)||!Number.isSafeInteger(birth.bytes)||birth.bytes<1||birth.bytes>4096||!Number.isSafeInteger(birth.dev)||birth.dev<0||!Number.isSafeInteger(birth.ino)||birth.ino<1)throw Error();
}
const stamp=s=>[s.dev,s.ino,s.size,s.mode,s.mtimeNs,s.ctimeNs].map(String).join(':');
async function privateJson(file){
 if(typeof file!=='string'||!path.isAbsolute(file))throw Error();
 const f=await fs.open(file,constants.O_RDONLY|constants.O_NOFOLLOW|constants.O_CLOEXEC);retain.add(f);
 const before=await f.stat({bigint:true});if(!before.isFile()||before.uid!==BigInt(process.getuid())||before.gid!==BigInt(process.getgid())||before.nlink!==1n||(before.mode&0o7777n)!==0o600n||before.size>262144n||before.size<2n)throw Error();
 const raw=await f.readFile({encoding:'utf8'}),after=await f.stat({bigint:true}),named=await fs.lstat(file,{bigint:true});
 if(stamp(before)!==stamp(after)||stamp(before)!==stamp(named))throw Error();return JSON.parse(raw);
}
/** Private DATA output only, never a registry/Root capability constructor. */
async function writeJson(file,value){
 const bytes=Buffer.from(JSON.stringify(value,null,2)+'\n');if(bytes.length>131072)throw Error();
 const tmp=file+'.'+randomUUID()+'.tmp',f=await fs.open(tmp,constants.O_WRONLY|constants.O_CREAT|constants.O_EXCL|constants.O_NOFOLLOW,0o600);retain.add(f);
 await f.writeFile(bytes);await f.sync();await f.close();retain.delete(f);await fs.rename(tmp,file);
 const dir=await fs.open(path.dirname(file),constants.O_RDONLY|constants.O_DIRECTORY|constants.O_NOFOLLOW);retain.add(dir);await dir.sync();await dir.close();retain.delete(dir);
}
async function status(){if(!output||outputFailure||statusBusy)return;statusBusy=true;try{
 let historical=null;try{historical=await driver?.historicalResultData();}catch{}
 await writeJson(path.join(output,'status.json'),{driver:driver?.statusData()??null,historical_data:historical,physical_qualification_claim:false,root_ack_receipt_claim:false,normal_retirement_claim:false});
}catch{outputFailure=true;}finally{statusBusy=false;}}
try{
 const i=await privateJson(process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT);
 startupCheck='input-schema';
 const flow=i.qualification_kind==='sdk-runtime-ordinary-v2';
 exact(i,flow?['format_version','qualification_kind','postgres','candidate','account_id','command_key','activation_authority','services','supervisor']:['format_version','qualification_kind','postgres','candidate','account_id','command_key']);if(i.format_version!==1||!['runtime-image-v1','compiler-sdk-v1','sdk-runtime-ordinary-v2'].includes(i.qualification_kind))throw Error();
 if((flow||i.qualification_kind==='compiler-sdk-v1')!==Object.hasOwn(i.candidate??{},'sdk'))throw Error();
 exact(i.postgres,['mode','authority','registrationAuthority','policy','flyToken','tokenEncryptionKeyHex']);
 const hex=i.postgres.tokenEncryptionKeyHex;if(typeof hex!=='string'||!/^[a-f0-9]{64}$/.test(hex)||/^0+$/.test(hex))throw Error();
 const {tokenEncryptionKeyHex,...postgres}=i.postgres;postgres.tokenEncryptionKey=Buffer.from(hex,'hex');
 startupCheck='output-directory';output=process.env.RUDDER_RUNTIME_QUALIFICATION_OUTPUT;if(typeof output!=='string'||!path.isAbsolute(output))throw Error();
 await fs.mkdir(output,{mode:0o700});const dir=await fs.lstat(output);if(!dir.isDirectory()||dir.uid!==process.getuid()||dir.gid!==process.getgid()||(dir.mode&0o7777)!==0o700)throw Error();
 let start;
 if(flow){
  startupCheck='flow-process';
  if(process.getuid()!==0||process.getgid()!==0||i.account_id!=='rudder-v4-qualification')throw Error();
  startupCheck='supervisor-data';
  validateSupervisorBirthData(i.supervisor);
  const state=await privateJson(path.join(path.dirname(process.env.RUDDER_RUNTIME_QUALIFICATION_INPUT),'supervisor.json'));
  if(JSON.stringify(i.supervisor)!==JSON.stringify(state)||state.supervisor_pid!==process.ppid||!Number.isSafeInteger(state.setup_end_boottime_milliseconds))throw Error();
  startupCheck='setup-clock';const uptime=await fs.open('/proc/uptime',constants.O_RDONLY|constants.O_CLOEXEC|constants.O_NOFOLLOW);retain.add(uptime);
  const setup={remaining(){if(process.ppid!==state.supervisor_pid)throw Error();const b=Buffer.alloc(128),n=readSync(uptime.fd,b,0,b.length,0),m=/^([0-9]+\.[0-9]{2}) /.exec(b.subarray(0,n).toString('ascii'));if(!m)throw Error();
   const left=Math.floor(state.setup_end_boottime_milliseconds-Number(m[1])*1000-10);if(!Number.isSafeInteger(left)||left<1)throw Error();return left;}};retain.add(setup);setup.remaining();
  startupCheck='services-data';exact(i.services,['format_version','boot_nonce','authority_id','policy_sha256','candidate_sha256','storageReceiptSha256','s3','serviceCredentials']);
  exact(i.services.s3,['bucket','region','roleArn','webIdentityTokenFile']);
  if(i.services.format_version!==1||i.services.policy_sha256!==hash(canonical(postgres.policy))||i.services.candidate_sha256!==hash(canonical(i.candidate))||i.services.boot_nonce!==state.boot_nonce||i.services.authority_id!==postgres.authority.authorityId||i.services.s3.bucket!=='rudder-v4-qualification-597088032164-us-east-1'||i.services.s3.region!=='us-east-1'||i.services.s3.roleArn!=='arn:aws:iam::597088032164:role/rudder-v4-qualification-fabric-v1'||i.services.s3.webIdentityTokenFile!=='/.fly/oidc_token')throw Error();
  startupCheck='storage-construction';storage=S3FabricObjectStorage.fromWebIdentityForImmutableBatches(i.services.s3);retain.add(storage);
  startupCheck='storage-retention';
  const storageClock=originalTime(Math.min(300000,setup.remaining()));retain.add(storageClock);
  const readback=storage.qualifyImmutableBatchRetentionRetaining({remaining:()=>Math.min(storageClock.remaining(),setup.remaining())});retain.add(readback);await readback;setup.remaining();storage.assertQualifiedBatchRetention();
  startupCheck='startup-config';const full=readManagedHomeStartupConfig(JSON.stringify({formatVersion:1,authority:postgres.authority,registrationAuthority:postgres.registrationAuthority,policy:postgres.policy,tokenEncryptionKeyHex:hex,storageReceiptSha256:i.services.storageReceiptSha256,serviceCredentials:i.services.serviceCredentials}),{storage,flyToken:postgres.flyToken});retain.add(full);
  startupCheck='flow-construction';if(full.mode!=='postgres')throw Error();driver=new StagingSdkRuntimeFlow(full,i.activation_authority,i.candidate,i.account_id,i.command_key,setup);retain.add(driver);
  startupCheck='flow-start';
  start=driver.runRetaining();
 }else{
  startupCheck='flow-construction';
  const Driver=i.qualification_kind==='compiler-sdk-v1'?CompilerSdkQualificationDriver:RuntimeImageQualificationDriver;
  driver=new Driver(postgres,i.candidate,i.account_id,i.command_key);retain.add(driver);start=driver.startRetaining();
 }
 startupCheck='initial-status';retain.add(start);await status();start.then(status,async()=>{await status();});
 // Referenced timer + actual listening server retain original owners, including
 // failed/unknown children/provider effects, until Root's owned teardown. A
 // budget expiry closes effect admission; it never exits/restarts this process.
 setInterval(()=>{void status();},2000);
}catch{
 let retention;try{retention=storage?.batchRetentionStatus();}catch{}
 if(output)await writeJson(path.join(output,'failure.json'),startupFailureData(startupCheck,!!driver,retention)).catch(()=>{});
 // Static diagnostic only; never print the private input/error cause. Keep any
 // actual owner alive for Root reconciliation instead of implicit cleanup.
 process.stderr.write('Runtime qualification driver requires private reconciliation\n');
 setInterval(()=>{},60000);
}
