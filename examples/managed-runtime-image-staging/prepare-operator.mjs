// Private operator preparation only. No provider, role, source or owner grant.
import fs from 'node:fs/promises';
import {constants as C} from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {createHash,randomUUID,randomBytes} from 'node:crypto';
import {need,validateLifecycle,parseStrict,validatePositive,canonical as hostCanonical,hash as hostHash} from './host-contract.mjs';
const sha=b=>createHash('sha256').update(b).digest('hex');
const encode=v=>Buffer.from(JSON.stringify(v,null,2)+'\n');
const stamp=s=>[s.dev,s.ino,s.uid,s.gid,s.mode,s.nlink,s.size,s.mtimeNs,s.ctimeNs].map(String).join(':');
const retained=[];
export async function readStable(p,maximum=16*1024*1024){
 need(path.isAbsolute(p)&&path.normalize(p)===p&&await fs.realpath(p)===p);
 const a=await fs.lstat(p,{bigint:true});need(a.isFile()&&a.nlink===1n&&(Number(a.mode)&0o022)===0&&[0,process.getuid()].includes(Number(a.uid))&&a.size>0n&&a.size<=BigInt(maximum));
 const fd=await fs.open(p,C.O_RDONLY|C.O_NOFOLLOW);retained.push([p,fd,a]);need(stamp(a)===stamp(await fd.stat({bigint:true})));
 const raw=await fd.readFile();need(raw.length<=maximum&&stamp(a)===stamp(await fd.stat({bigint:true}))&&stamp(a)===stamp(await fs.lstat(p,{bigint:true})));return raw;
}
async function checkRetained(){for(const [p,fd,a]of retained)need(stamp(a)===stamp(await fd.stat({bigint:true}))&&stamp(a)===stamp(await fs.lstat(p,{bigint:true})));}
export function operatorData(binding,values,api,flyToken){
 const {validatePolicy,validateCompilerQualificationCandidate,C1_COMPILER_SDK,validatePositiveIngressInput,canonical,hash}=api;
 validateLifecycle(binding);const {workerBuildReceipt:worker,controllerBuildReceipt:controller,volumeReceipt:volume,pgEvidence:pg,s3Evidence:s3}=values;
 need(typeof flyToken==='string'&&flyToken.trim()===flyToken&&flyToken.length>=20&&flyToken.length<=16384&&!/[\x00-\x1f\x7f]/.test(flyToken));
 for(const k of Object.keys(binding.capture))need(worker[k]===binding.capture[k]&&controller[k]===binding.capture[k]);
 need(worker.sourceQualified===false&&worker.sdkQualified===false&&worker.runtimeQualified===false&&worker.image.split(':')[0]+'@'+worker.digest===binding.worker_image);
 need(Array.isArray(worker.commands)&&worker.commands.length>0&&worker.commands.every(c=>c.actualWait===true&&c.exitCode===0&&c.pidfdHeld===true&&c.pipeEofClaimed===false));
 need(hostHash(worker.generatedJs)===binding.capture.generatedJsSha256);
 need(controller.kind==='SAME_CAPTURE_CONTROLLER_BUILD_DATA'&&controller.originalBuild.actualWait===true&&controller.originalBuild.exitCode===0&&controller.originalBuild.pidfdHeld===true&&controller.originalBuild.pipeEofClaimed===false);
 need(volume.kind==='operator-reviewed-fresh-volume-component-v1'&&volume.isolated_only===true&&volume.component_passed===true&&volume.cleanup_verified===true&&volume.filesystem_write_fsync_rename_readback===true&&volume.root_sdk_source_authority===false&&volume.cold_restart_durability_qualified===false&&pg.ok===true&&pg.cleanupVerified===true&&s3.actualCasPassed===true);
 const positive=validatePositiveIngressInput(validatePositive(binding.positive_ingress)),image=binding.worker_image,product=worker.productData,data=product.runtimeData,elf=product.managedFiles;
 const node=product.components.components.find(x=>x.path==='/usr/local/bin/node');need(node&&data.layout==='baked-image-cas-v1'&&data.total_bytes>0&&product.sdkQualified===false&&product.runtimeQualified===false);
 const review={format_version:1,scope:'explicit isolated same-capture staging operator admission DATA; no qualification or fleet grant',capture:binding.capture,new_projects_only:true,owner_mode:'provider-fenced-v1',no_time_based_regrant:true,sdk_role_ready_claim:false,runtime_role_ready_claim:false,source_ready_claim:false,normal_runtime_retirement_claim:false,fleet_HA_claim:false,clock_safety_claim:false,actual_model_execution:false,evidence:Object.values(binding.inputs).map(r=>({path:r.path,raw_sha256:r.sha256})),limitations:['candidate source_manifest_sha256 is exact worker build-receipt.json DATA, not source qualification','provider enrollment, Root, SDK and ordinary positive ingress remain physical gates','provider replacement/release/restart adoption unsupported','shared build-cache reuse disabled']};
 const files={},receipts={};for(const name of ['provider','durability','fresh-volume','control-origin','image','storage','image-batch']){
 const value={kind:'operator-isolated-staging-policy-admission-v1',part:name,image,control_origin:'https://rudder-v4-qualification.fly.dev',review,...(name==='fresh-volume'?{fresh_volume:volume}:{}),no_role_or_owner_grant:true};files['receipts/'+name+'.json']=encode(value);receipts[name]=sha(files['receipts/'+name+'.json']);}
 const policy=validatePolicy({ownerMode:'provider-fenced-v1',sdkInitialSource:'sdk-fixed-suite-v1',compilerSdk:C1_COMPILER_SDK,organization:'exla',region:'sjc',image,runtimeManifest:product.runtimeManifest,
 imagePreseed:{fullManifestSha256:data.roots.manifest.digest,installerInput:data.roots.installer_input,batchTransport:'batch-v1',batchReceiptSha256:receipts['image-batch'],runtimeMount:{profile:'baked-cas-v1',inventory:data.inventory,image,imageReceiptSha256:receipts.image,fullManifestSha256:data.roots.manifest.digest,installerInput:data.roots.installer_input}},
 runtimeImageReceiptSha256:receipts.image,providerReceiptSha256:receipts.provider,durabilityReceiptSha256:receipts.durability,freshVolumeReceiptSha256:receipts['fresh-volume'],controlPlane:{profile:'qualification-v1',qualificationReceiptSha256:receipts['control-origin']},
 nativeSha256:elf['/opt/rudder/dist/native/rudder-native'].sha256,launcherSha256:elf['/usr/local/bin/rudder-machine-root'].sha256,serviceSha256:elf['/usr/local/bin/rudder-root-storage'].sha256,nodeSha256:node.sha256,sidecarPolicySha256:product.managedMetadata['/usr/local/share/rudder-managed-image/sidecar-policy-pin.json'].sha256,
 volumeGb:2,cpus:2,memoryMb:4096,userName:'Rudder isolated SDK qualification',userEmail:'qualification@exla.ai'});
 const candidate=validateCompilerQualificationCandidate({image,image_receipt_sha256:receipts.image,source_manifest_sha256:binding.inputs.workerBuildReceipt.sha256,inventory:data.inventory,manifest:data.roots.manifest,installer_input:data.roots.installer_input,native_sha256:policy.nativeSha256,buddy_sha256:elf['/opt/rudder/dist/native/rudder-jj'].sha256,service_sha256:policy.serviceSha256,launcher_sha256:policy.launcherSha256,sdk:C1_COMPILER_SDK},policy);
 const suffix=randomUUID(),authorityId=randomUUID();
 let snapshot;
 if(Object.hasOwn(binding.inputs,'positiveProfileSnapshot')){need(typeof api.validateStagingCodexAccessSnapshot==='function');const raw=api.validateStagingCodexAccessSnapshot(values.positiveProfileSnapshot,positive.profileId);snapshot=parseStrict(raw.toString());}
 const template={...(snapshot?{positive_profile_snapshot:snapshot}:{}),positive_ingress:positive,format_version:1,qualification_id:randomUUID(),authority_id:authorityId,account_id:'rudder-v4-qualification',command_key:'sdk-v4-'+suffix,policy,candidate,fly_token:flyToken};
 const services={format_version:1,authority_id:authorityId,policy_sha256:hash(canonical(policy)),candidate_sha256:hash(canonical(candidate)),storageReceiptSha256:receipts.storage,s3:{bucket:'rudder-v4-qualification-597088032164-us-east-1',region:'us-east-1',roleArn:'arn:aws:iam::597088032164:role/rudder-v4-qualification-fabric-v1',webIdentityTokenFile:'/.fly/oidc_token'},serviceCredentials:[{accountId:'rudder-v4-qualification',principalId:'stage-service-'+suffix,credentialId:'stage-credential-'+suffix,tokenBase64url:randomBytes(32).toString('base64url')}]};
 Object.assign(files,{'operator.template.private.json':encode(template),'services.template.private.json':encode(services),'review.json':encode(review),'prepared.json':encode({scope:'private immutable policy/candidate/credential templates only; actual original boot nonce and binder completion still mandatory',image,authorityId,policySha256:services.policy_sha256,candidateSha256:services.candidate_sha256,receipts,rootSdkOrSourceAuthority:false})});return files;
}
async function privateDir(p){const s=await fs.lstat(p);need(s.isDirectory()&&!s.isSymbolicLink()&&(s.mode&0o777)===0o700&&s.uid===process.getuid());}
async function syncDir(p){const f=await fs.open(p,C.O_RDONLY|C.O_DIRECTORY|C.O_NOFOLLOW);try{await f.sync();}finally{await f.close();}}
export async function prepare(bindingPath,output,volumePath){
 const binding=validateLifecycle(parseStrict((await readStable(bindingPath)).toString()));need(output===binding.bundle_directory&&volumePath===binding.fresh_volume_receipt_path);await privateDir(path.dirname(output));
 const values={},inputRaws={};for(const [k,ref]of Object.entries(binding.inputs)){const raw=await readStable(ref.path);need(sha(raw)===ref.sha256);inputRaws[k]=raw;values[k]=parseStrict(raw.toString());if(k==='positiveProfileSnapshot'){await privateDir(path.dirname(ref.path));const st=await fs.lstat(ref.path);need((st.mode&0o777)===0o600);}}
 const worker=values.workerBuildReceipt;need(worker.sourceDirectory===binding.source_directory);
 const controller={...binding.controller};delete controller.receiptSha256;need(hostCanonical(controller)===hostCanonical(values.controllerBuildReceipt));
 const source=binding.source_directory;need(await fs.realpath(source)===source&&sha(await readStable(worker.sourceArchive,512*1024*1024))===binding.capture.sourceArchiveSha256);
 for(const [relative,pin]of Object.entries(worker.generatedJs)){need(!path.isAbsolute(relative)&&!relative.split('/').some(s=>s==='..'||s==='.')&&relative.endsWith('.js'));const raw=await readStable(path.join(source,relative));need(raw.length===pin.bytes&&sha(raw)===pin.sha256);}
 const reportRaw=await readStable(values.volumeReceipt.raw_report_path);need(sha(reportRaw)===values.volumeReceipt.raw_report_sha256);const report=parseStrict(reportRaw.toString()),a=report.artifactBinding;
 need(a.workerImage===binding.worker_image&&a.sourceRevision===binding.capture.sourceRevision&&a.sourceArchiveSha256===binding.capture.sourceArchiveSha256&&a.buildReceiptPath===binding.inputs.workerBuildReceipt.path&&a.buildReceiptSha256===binding.inputs.workerBuildReceipt.sha256&&report.componentPassed===true&&report.cleanupVerified===true);
 const load=relative=>{need(Object.hasOwn(worker.generatedJs,relative));return import(pathToFileURL(path.join(source,relative)).href);};
 const types=await load('cloud/dist/managed-home-provisioning/types.js'),protocol=await load('cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js'),catalog=await load('cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js'),positive=await load('cloud/dist/managed-home-provisioning/staging-positive-ingress-driver.js');
 const profile=Object.hasOwn(binding.inputs,'positiveProfileSnapshot')?await load('cloud/dist/managed-home-provisioning/staging-codex-profile-import.js'):{};
 if(inputRaws.positiveProfileSnapshot)profile.validateStagingCodexAccessSnapshot(inputRaws.positiveProfileSnapshot,binding.positive_ingress.profileId);
 await privateDir(path.dirname(binding.credential_store_path));const credentialStat=await fs.lstat(binding.credential_store_path);need((credentialStat.mode&0o777)===0o600);const credential=parseStrict((await readStable(binding.credential_store_path)).toString());need(credential.token===process.env.FLY_API_TOKEN);
 const files=operatorData(binding,values,{...types,...protocol,...catalog,...positive,...profile},process.env.FLY_API_TOKEN);
 await checkRetained();await fs.mkdir(output,{mode:0o700});await syncDir(path.dirname(output));await fs.mkdir(path.join(output,'receipts'),{mode:0o700});await syncDir(output);
 for(const [name,raw]of Object.entries(files)){await checkRetained();const p=path.join(output,name),fd=await fs.open(p,C.O_WRONLY|C.O_CREAT|C.O_EXCL|C.O_NOFOLLOW,0o600);await fd.writeFile(raw);await fd.sync();await syncDir(path.dirname(p));retained.push([p,fd,await fd.stat({bigint:true})]);}
 await checkRetained();console.log(JSON.stringify({scope:'private staging templates prepared; original boot and completion gates still required',output,image:binding.worker_image,secretValuesPrinted:false}));
}
if(process.argv[1]&&pathToFileURL(path.resolve(process.argv[1])).href===import.meta.url){
 try{need(process.argv.length===5);await prepare(...process.argv.slice(2));}catch{console.error('STAGING_PRIVATE_PREPARATION_REFUSED_RETAIN_PARTIALS');process.exitCode=1;}
}
