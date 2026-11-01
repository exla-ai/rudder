import {validateCompilerSdkSemanticData,type CompilerSdkSemanticData} from "./compiler-sdk-semantic-data.js";
import {createHmac,timingSafeEqual} from "node:crypto";
import {BINDING_FIELDS,type Envelope,type RegistrationBinding,type RootConstraint} from "./registration.js";
import {check,exact,canonical,hash,sha,uuid,snapshot,type ManagedHomePolicy} from "./types.js";
import {validateRuntimeQualificationCandidate,validateRuntimeReadyObservation,RUNTIME_QUALIFICATION_SCOPE,
 type RuntimeQualificationCandidate,type RuntimeQualificationScope,type RuntimeQualificationReady,type RuntimeReadyObservationData} from "./runtime-qualification-protocol.js";
import {validateCompilerSdkCatalog,compilerSdkProfileData,type CompilerSdkCatalogData} from "./compiler-sdk-catalog.js";
export const COMPILER_QUALIFICATION_SCOPE="disposable-fixed-compiler-sdk-only" as const;
export const COMPILER_QUALIFICATION_DOMAINS={ready:"rudder_compiler_sdk_qualification_ready_v1\0",ack:"rudder_compiler_sdk_qualification_ack_v1\0"} as const;
export interface CompilerQualificationCandidate extends RuntimeQualificationCandidate {sdk:CompilerSdkCatalogData;}
export type CompilerQualificationScope=Omit<RuntimeQualificationScope,"scope">&{scope:typeof COMPILER_QUALIFICATION_SCOPE;sdk:CompilerSdkCatalogData};
export type CompilerQualificationReady=Omit<RuntimeQualificationReady,"kind"|"scope">&{kind:"compiler_sdk_qualification_ready";scope:CompilerQualificationScope;sdk_proof:{mount:{semantic:CompilerSdkSemanticData;floor:unknown;toolchains:Record<string,unknown>};probe:unknown}};
export interface CompilerQualificationAck {version:1;kind:"compiler_sdk_qualification_ack";qualification_id:string;qualification_nonce:string;request_sha256:string;registration_proof_sha256:string;original_enable_constraint:RootConstraint;
 result:null|{scope:"immutable-compiler-sdk-role-only";state:"published";role_binding_sha256:string;image:string;inventory:CompilerSdkCatalogData["inventory"];manifest:CompilerSdkCatalogData["manifest"];installer_input:CompilerSdkCatalogData["installer_input"]};error:null|string;}
const candidateFields=["image","image_receipt_sha256","source_manifest_sha256","inventory","manifest","installer_input","native_sha256","buddy_sha256","service_sha256","launcher_sha256","sdk"] as const;
const fields=(v:unknown,names:readonly string[])=>{exact(v,names);check(Object.keys(v as object).length===names.length);};
export function runtimeScopeData(s:CompilerQualificationScope):RuntimeQualificationScope {const{sdk,...runtime}=s;return {...runtime,scope:RUNTIME_QUALIFICATION_SCOPE};}
export function validateCompilerQualificationCandidate(input:unknown,policy:ManagedHomePolicy):CompilerQualificationCandidate {
 const c=snapshot(input)as CompilerQualificationCandidate;fields(c,candidateFields);const{sdk,...runtime}=c;validateRuntimeQualificationCandidate(runtime,policy);
 validateCompilerSdkCatalog(sdk);check(policy.compilerSdk&&canonical(policy.compilerSdk).equals(canonical(sdk)),"identity_changed");return c;
}
export function validateCompilerQualificationScope(input:unknown,policy:ManagedHomePolicy,historical=false):CompilerQualificationScope {
 const s=snapshot(input)as CompilerQualificationScope;fields(s,[...candidateFields,"format_version","scope","qualification_id","qualification_nonce"]);
 check(s.format_version===1&&s.scope===COMPILER_QUALIFICATION_SCOPE);uuid(s.qualification_id);uuid(s.qualification_nonce);validateCompilerSdkCatalog(s.sdk);
 check(policy.compilerSdk&&canonical(policy.compilerSdk).equals(canonical(s.sdk)),"identity_changed");
 const{format_version,scope,qualification_id,qualification_nonce,...candidate}=s;
 if(!historical)validateCompilerQualificationCandidate(candidate,policy);else {
  check(/^registry\.fly\.io\/rudder-workers-exla@sha256:[a-f0-9]{64}$/.test(s.image));
  for(const v of[s.image_receipt_sha256,s.source_manifest_sha256,s.native_sha256,s.buddy_sha256,s.service_sha256,s.launcher_sha256])sha(v);
 }return s;
}
/** Full-catalogue role/admission identity, deliberately NOT a build-cache key. */
export function compilerRoleBindingData(c:{sdk:CompilerSdkCatalogData}){const sdk=validateCompilerSdkCatalog(c.sdk);return {format_version:1,role:"immutable-compiler-sdk-role-v1",profile_id:sdk.profile_id,artifact_manifest:snapshot(sdk.artifact_manifest),manifest:snapshot(sdk.manifest),installer_input:snapshot(sdk.installer_input)};}
export function compilerQualificationMac(key:Buffer,phase:keyof typeof COMPILER_QUALIFICATION_DOMAINS,body:unknown):string {
 check(Buffer.isBuffer(key)&&key.length===32,"identity_changed");return createHmac("sha256",key).update(COMPILER_QUALIFICATION_DOMAINS[phase]).update(canonical(body,24*1024)).digest("hex");
}
function validateSdkProof(p:CompilerQualificationReady["sdk_proof"],s:CompilerQualificationScope,o:RuntimeReadyObservationData){
 fields(p,["mount","probe"]);fields(p.mount,["semantic","floor","toolchains"]);const semantic=validateCompilerSdkSemanticData(p.mount.semantic); // exact catalog is already required in the same signed scope
 fields(p.mount.floor,["device","inode","mount_id","directory"]);const floor=p.mount.floor as Record<string,unknown>;check(floor.directory===true&&Number.isSafeInteger(floor.mount_id)&&Number(floor.mount_id)>0);
 const tools=p.mount.toolchains;const selected=compilerSdkProfileData(s.sdk).tools;fields(tools,selected.map(t=>t.name));
 for(const spec of selected){const name=spec.name,path=spec.relative_path;const t=tools[name]as Record<string,unknown>;fields(t,["version","closure","executable","executable_path","configuration"]);
  const configuration=t.configuration as Record<string,unknown>;fields(configuration,["executable_raw_sha256",...(spec.compiler_entrypoint?["compiler_entrypoint"]:[])]);
  sha(configuration.executable_raw_sha256);check(t.version===semantic.directory.digest&&t.executable_path===path&&canonical(t.closure).equals(canonical(semantic.directory))&&(!spec.compiler_entrypoint||configuration.compiler_entrypoint===spec.compiler_entrypoint),"identity_changed");
  const file=t.executable as Record<string,unknown>;fields(file,["kind","algorithm","digest","size_bytes"]);check((file.kind==="blob"||file.kind==="file")&&file.algorithm==="sha256"&&Number.isSafeInteger(file.size_bytes)&&Number(file.size_bytes)>0);sha(file.digest);if(name==="buck2")check(canonical(file).equals(canonical(semantic.buck2)),"identity_changed");
 }
 const probe=p.probe as Record<string,unknown>;fields(probe,["format_version","kind","helper_pid","helper_start_ticks","original_constraint","workload"]);
 check(probe.format_version===1&&probe.kind==="original-sdk-fixed-helper-complete-v1"&&Number.isSafeInteger(probe.helper_pid)&&Number(probe.helper_pid)>1&&typeof probe.helper_start_ticks==="string"&&/^[1-9][0-9]*$/.test(probe.helper_start_ticks)
  &&canonical(probe.original_constraint).equals(canonical(o.original_enable_constraint)),"identity_changed");
 const w=probe.workload as Record<string,any>;fields(w,["format_version","kind","network_namespace","user_namespace","mount_namespace","pid_namespace","started_boottime_nanoseconds","completed_boottime_nanoseconds","sdk_python","compiler_capabilities","buck2_static","sdk_root_device","sdk_root_readonly","buck2","rust","rust_result","typescript","typescript_result","rust_artifact_sha256","typescript_artifact_sha256","models","source_authority","normal_retirement_authority"]);
 check(w.format_version===1&&w.kind==="actual-fixed-rust-library-typescript-sdk-workload-v1"&&w.models===false&&w.source_authority===false&&w.normal_retirement_authority===false);
 const start=BigInt(w.started_boottime_nanoseconds),end=BigInt(w.completed_boottime_nanoseconds);check(start>0n&&start<=end&&end<BigInt(o.original_enable_constraint.boottime_deadline_nanoseconds)&&end<=BigInt(o.root_ack_observed_boottime_nanoseconds),"expired");
 for(const k of["buck2","rust","rust_result","typescript","typescript_result"]){fields(w[k],["exit_code","stdout_sha256","stderr_sha256","stdout_bytes","stderr_bytes"]);check(w[k].exit_code===0&&w[k].stdout_bytes<=65536&&w[k].stderr_bytes<=65536);sha(w[k].stdout_sha256);sha(w[k].stderr_sha256);}
 check(w.buck2_static===true);fields(w.compiler_capabilities,["CapEff","CapPrm","CapInh","CapAmb","CapBnd"]);for(const value of Object.values(w.compiler_capabilities))check(value==="0000000000000000","identity_changed");
 const python=w.sdk_python;fields(python,["device","inode","sha256","size_bytes"]);const pythonTool=tools.python as Record<string,any>;
 check(python.sha256===pythonTool.configuration.executable_raw_sha256&&python.device===String(floor.device)&&w.sdk_root_device===String(floor.device)&&w.sdk_root_readonly===true&&Number.isSafeInteger(python.size_bytes)&&python.size_bytes>0&&python.size_bytes<=64*1024*1024,"identity_changed");
 check(w.rust_result.stdout_sha256===hash("")&&w.typescript_result.stdout_sha256===hash("rudder-sdk-typescript-ok\n"),"identity_changed");sha(w.rust_artifact_sha256);sha(w.typescript_artifact_sha256);
 for(const key of["user_namespace","mount_namespace","network_namespace","pid_namespace"])check(typeof w[key]==="string"&&/^[a-z]+:\[[1-9][0-9]*\]$/.test(w[key]));
}
export function authenticateCompilerQualificationReady(input:unknown,key:Buffer,scope:CompilerQualificationScope,expected:RegistrationBinding,proof:string):Envelope<CompilerQualificationReady>{
 const e=snapshot(input,24*1024)as Envelope<CompilerQualificationReady>;fields(e,["body","mac"]);sha(e.mac);check(timingSafeEqual(Buffer.from(e.mac,"hex"),Buffer.from(compilerQualificationMac(key,"ready",e.body),"hex")),"identity_changed");
 const b=e.body;fields(b,["version","kind","qualification_id","qualification_nonce","registration_binding","root_nonce","registration_proof_sha256","scope","observation","selected_executables","remaining_milliseconds","sdk_proof"]);
 check(b.version===1&&b.kind==="compiler_sdk_qualification_ready"&&b.qualification_id===scope.qualification_id&&b.qualification_nonce===scope.qualification_nonce,"identity_changed");uuid(b.root_nonce);fields(b.registration_binding,BINDING_FIELDS);
 check(canonical(b.scope).equals(canonical(scope))&&canonical(b.registration_binding).equals(canonical(expected))&&b.registration_proof_sha256===proof,"identity_changed");
 validateRuntimeReadyObservation(b.observation,runtimeScopeData(scope),expected,proof);check(canonical(b.selected_executables).equals(canonical(b.observation.selected_executables)),"identity_changed");validateSdkProof(b.sdk_proof,scope,b.observation);
 check(Number.isInteger(b.remaining_milliseconds)&&b.remaining_milliseconds>=1&&b.remaining_milliseconds<=b.observation.original_enable_constraint.maximum_milliseconds,"expired");return e;
}
export function publishedCompilerQualificationAck(key:Buffer,r:CompilerQualificationReady):Envelope<CompilerQualificationAck>{
 const body:CompilerQualificationAck={version:1,kind:"compiler_sdk_qualification_ack",qualification_id:r.qualification_id,qualification_nonce:r.qualification_nonce,request_sha256:hash(canonical(r,24*1024)),registration_proof_sha256:r.registration_proof_sha256,original_enable_constraint:snapshot(r.observation.original_enable_constraint),
 result:{scope:"immutable-compiler-sdk-role-only",state:"published",role_binding_sha256:hash(canonical(compilerRoleBindingData(r.scope))),image:r.scope.sdk.cas_image,inventory:snapshot(r.scope.sdk.inventory),manifest:snapshot(r.scope.sdk.manifest),installer_input:snapshot(r.scope.sdk.installer_input)},error:null};return {body,mac:compilerQualificationMac(key,"ack",body)};
}
