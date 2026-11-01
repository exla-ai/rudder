import {createHmac,timingSafeEqual} from "node:crypto";
import {validateFabricObjectRef,type FabricObjectRef} from "../fabric-cas.js";
import {BINDING_FIELDS,type Envelope,type RegistrationBinding,type RootConstraint} from "./registration.js";
import {check,exact,canonical,hash,sha,uuid,snapshot,runtimeMountConfigData,type ManagedHomePolicy} from "./types.js";

export const RUNTIME_QUALIFICATION_SCOPE="disposable-runtime-floor-and-original-client-only" as const;
export const RUNTIME_QUALIFICATION_DOMAINS={ready:"rudder_runtime_image_qualification_ready_v1\0",ack:"rudder_runtime_image_qualification_ack_v1\0"} as const;
/** Operator/canonical-config DATA; never a ready owner, project grant or lease. */
export interface RuntimeQualificationCandidate {
 image:string;image_receipt_sha256:string;source_manifest_sha256:string;
 inventory:FabricObjectRef;manifest:FabricObjectRef;installer_input:FabricObjectRef;
 native_sha256:string;buddy_sha256:string;service_sha256:string;launcher_sha256:string;
}
export interface RuntimeQualificationScope extends RuntimeQualificationCandidate {
 format_version:1;scope:typeof RUNTIME_QUALIFICATION_SCOPE;qualification_id:string;qualification_nonce:string;
}
export interface RuntimeExecutableData {installer_path:string;sha256:string;size_bytes:number;}
export interface RuntimeReadyObservationData {
 format_version:1;kind:"runtime-image-ready-observation-data-v1";
 scope:"original-enable-ack-and-current-original-client-only";
 request_nonce:string;bootstrap_nonce:string;engine_instance_id:string;epoch:string;mount_id_data:string;
 original_enable_constraint:RootConstraint;
 root_ack_observed_boottime_nanoseconds:string;native_received_boottime_nanoseconds:string;
 native_issued_boottime_nanoseconds:string;client_current_checked_boottime_nanoseconds:string;
 registration_proof_sha256:string;config_sha256:string;boot_id:string;service_pid_data:number;
 source_authority:false;helper_enrolled:false;scheduler_ready:false;normal_retirement_authority:false;
 inventory:FabricObjectRef;manifest:FabricObjectRef;installer_input:FabricObjectRef;
 selected_executables:RuntimeExecutableData[];
}
export interface RuntimeQualificationReady {
 version:1;kind:"runtime_image_qualification_ready";qualification_id:string;qualification_nonce:string;
 registration_binding:RegistrationBinding;root_nonce:string;registration_proof_sha256:string;
 scope:RuntimeQualificationScope;observation:RuntimeReadyObservationData;
 selected_executables:RuntimeExecutableData[];remaining_milliseconds:number;
}
export interface RuntimeQualificationAck {
 version:1;kind:"runtime_image_qualification_ack";qualification_id:string;qualification_nonce:string;
 request_sha256:string;registration_proof_sha256:string;original_enable_constraint:RootConstraint;
 result:null|{scope:"immutable-image-runtime-role-only";state:"published";role_binding_sha256:string;
   image:string;inventory:FabricObjectRef;manifest:FabricObjectRef;installer_input:FabricObjectRef};
 error:null|"invalid_request"|"unconfigured"|"identity_changed"|"expired"|"conflict"|"busy"|"effect_unknown"|"unavailable";
}
const candidateFields=["image","image_receipt_sha256","source_manifest_sha256","inventory","manifest","installer_input","native_sha256","buddy_sha256","service_sha256","launcher_sha256"] as const;
function fields(v:unknown,names:readonly string[]):asserts v is Record<string,unknown>{exact(v,names);check(Object.keys(v).length===names.length);}
function ref(v:unknown,kind:"blob"|"directory",max:number):FabricObjectRef {
 fields(v,["kind","algorithm","digest","size_bytes"]);check(v.kind===kind&&v.algorithm==="sha256"&&Number.isSafeInteger(v.size_bytes)&&(v.size_bytes as number)>0&&(v.size_bytes as number)<=max);sha(v.digest);
 return validateFabricObjectRef(v,max);
}
export function validateRuntimeQualificationCandidate(input:unknown,policy:ManagedHomePolicy):RuntimeQualificationCandidate {
 const c=snapshot(input) as RuntimeQualificationCandidate;fields(c,candidateFields);
 const mount=runtimeMountConfigData(policy);check(mount&&policy.imagePreseed,"unconfigured");
 check(c.image===policy.image&&c.image_receipt_sha256===policy.runtimeImageReceiptSha256,"identity_changed");
 for(const v of [c.image_receipt_sha256,c.source_manifest_sha256,c.native_sha256,c.buddy_sha256,c.service_sha256,c.launcher_sha256])sha(v);
 check(c.native_sha256===policy.nativeSha256&&c.service_sha256===policy.serviceSha256&&c.launcher_sha256===policy.launcherSha256,"identity_changed");
 ref(c.inventory,"blob",16*1024*1024);ref(c.manifest,"blob",32*1024*1024);ref(c.installer_input,"directory",64*1024);
 check(canonical(c.inventory).equals(canonical(mount.inventory))&&c.manifest.digest===policy.imagePreseed.fullManifestSha256&&canonical(c.installer_input).equals(canonical(policy.imagePreseed.installerInput)),"identity_changed");return snapshot(c);
}
export function validateRuntimeQualificationScope(input:unknown,policy:ManagedHomePolicy):RuntimeQualificationScope {
 const s=snapshot(input) as RuntimeQualificationScope;fields(s,[...candidateFields,"format_version","scope","qualification_id","qualification_nonce"]);
 check(s.format_version===1&&s.scope===RUNTIME_QUALIFICATION_SCOPE);uuid(s.qualification_id);uuid(s.qualification_nonce);
 const {format_version,scope,qualification_id,qualification_nonce,...candidate}=s;validateRuntimeQualificationCandidate(candidate,policy);return s;
}
export function runtimeRoleBindingData(c:RuntimeQualificationCandidate){return {format_version:1,role:"immutable-image-runtime-role-v1",...snapshot(c)};}
export function runtimeQualificationMac(key:Buffer,phase:keyof typeof RUNTIME_QUALIFICATION_DOMAINS,body:unknown):string {
 check(Buffer.isBuffer(key)&&key.length===32,"identity_changed");return createHmac("sha256",key).update(RUNTIME_QUALIFICATION_DOMAINS[phase]).update(canonical(body,24*1024)).digest("hex");
}
const executablePaths=["package/dist/native/rudder-native","package/dist/native/rudder-jj","usr/local/bin/rudder-root-storage","usr/local/bin/rudder-machine-root"];
function selected(list:RuntimeExecutableData[],scope:RuntimeQualificationScope){check(Array.isArray(list)&&list.length===4);const hashes=[scope.native_sha256,scope.buddy_sha256,scope.service_sha256,scope.launcher_sha256];
 for(let i=0;i<4;i++){const e=list[i];fields(e,["installer_path","sha256","size_bytes"]);check(e.installer_path===executablePaths[i]&&e.sha256===hashes[i]&&Number.isSafeInteger(e.size_bytes)&&e.size_bytes>0&&e.size_bytes<=512*1024*1024,"identity_changed");}}
function decimal(v:unknown):bigint{check(typeof v==="string"&&/^[1-9][0-9]{0,19}$/.test(v));return BigInt(v);}
export function validateRuntimeReadyObservation(o:RuntimeReadyObservationData,s:RuntimeQualificationScope,b:RegistrationBinding,proof:string){
 fields(o,["format_version","kind","scope","request_nonce","bootstrap_nonce","engine_instance_id","epoch","mount_id_data","original_enable_constraint","root_ack_observed_boottime_nanoseconds","native_received_boottime_nanoseconds","native_issued_boottime_nanoseconds","client_current_checked_boottime_nanoseconds","registration_proof_sha256","config_sha256","boot_id","service_pid_data","source_authority","helper_enrolled","scheduler_ready","normal_retirement_authority","inventory","manifest","installer_input","selected_executables"]);
 check(o.format_version===1&&o.kind==="runtime-image-ready-observation-data-v1"&&o.scope==="original-enable-ack-and-current-original-client-only");uuid(o.request_nonce);uuid(o.bootstrap_nonce);uuid(o.engine_instance_id);
 check(o.epoch===b.epoch&&o.registration_proof_sha256===proof&&o.config_sha256===b.config_sha256,"identity_changed");decimal(o.mount_id_data);check(/^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/.test(o.boot_id));check(Number.isSafeInteger(o.service_pid_data)&&o.service_pid_data>0);
 check(o.source_authority===false&&o.helper_enrolled===false&&o.scheduler_ready===false&&o.normal_retirement_authority===false,"identity_changed");
 const c=o.original_enable_constraint;fields(c,["format_version","maximum_milliseconds","boottime_deadline_nanoseconds"]);check(c.format_version===1&&Number.isSafeInteger(c.maximum_milliseconds)&&c.maximum_milliseconds>=1&&c.maximum_milliseconds<=30_000);const end=decimal(c.boottime_deadline_nanoseconds);
 const stamps=[o.root_ack_observed_boottime_nanoseconds,o.native_received_boottime_nanoseconds,o.native_issued_boottime_nanoseconds,o.client_current_checked_boottime_nanoseconds].map(decimal);
 check(stamps.every((n,i)=>n<end&&(i===0||n>=stamps[i-1]))&&end-stamps[0]<=BigInt(c.maximum_milliseconds)*1_000_000n,"expired");
 for(const k of ["inventory","manifest","installer_input"] as const)check(canonical(o[k]).equals(canonical(s[k])),"identity_changed");selected(o.selected_executables,s);
}
/** Strict DATA parser + authentic Root proof. Does not issue a ready owner.
 * Registry additionally requires its actual disposable creation/purpose/socket
 * and original completed registration; a valid MAC alone cannot publish. */
export function authenticateRuntimeQualificationReady(input:unknown,key:Buffer,scope:RuntimeQualificationScope,expected:RegistrationBinding,proof:string):Envelope<RuntimeQualificationReady>{
 const e=snapshot(input,24*1024) as Envelope<RuntimeQualificationReady>;fields(e,["body","mac"]);sha(e.mac);
 check(timingSafeEqual(Buffer.from(e.mac,"hex"),Buffer.from(runtimeQualificationMac(key,"ready",e.body),"hex")),"identity_changed");const b=e.body;
 fields(b,["version","kind","qualification_id","qualification_nonce","registration_binding","root_nonce","registration_proof_sha256","scope","observation","selected_executables","remaining_milliseconds"]);
 check(b.version===1&&b.kind==="runtime_image_qualification_ready"&&b.qualification_id===scope.qualification_id&&b.qualification_nonce===scope.qualification_nonce,"identity_changed");uuid(b.root_nonce);fields(b.registration_binding,BINDING_FIELDS);
 check(canonical(b.scope).equals(canonical(scope))&&canonical(b.registration_binding).equals(canonical(expected))&&b.registration_proof_sha256===proof,"identity_changed");sha(b.registration_proof_sha256);
 validateRuntimeReadyObservation(b.observation,scope,expected,proof);selected(b.selected_executables,scope);check(canonical(b.selected_executables).equals(canonical(b.observation.selected_executables)),"identity_changed");
 check(Number.isInteger(b.remaining_milliseconds)&&b.remaining_milliseconds>=1&&b.remaining_milliseconds<=b.observation.original_enable_constraint.maximum_milliseconds,"expired");return e;
}
export function publishedRuntimeQualificationAck(key:Buffer,r:RuntimeQualificationReady):Envelope<RuntimeQualificationAck>{
 const {format_version,scope,qualification_id,qualification_nonce,...candidate}=r.scope;
 const body:RuntimeQualificationAck={version:1,kind:"runtime_image_qualification_ack",qualification_id,qualification_nonce,request_sha256:hash(canonical(r,24*1024)),registration_proof_sha256:r.registration_proof_sha256,original_enable_constraint:snapshot(r.observation.original_enable_constraint),result:{scope:"immutable-image-runtime-role-only",state:"published",role_binding_sha256:hash(canonical(runtimeRoleBindingData(candidate))),image:r.scope.image,inventory:snapshot(r.scope.inventory),manifest:snapshot(r.scope.manifest),installer_input:snapshot(r.scope.installer_input)},error:null};return {body,mac:runtimeQualificationMac(key,"ack",body)};
}
