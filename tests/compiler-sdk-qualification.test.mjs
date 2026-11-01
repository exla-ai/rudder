// Pure DATA/HMAC/refusal definitions only. No genuine Root/SDK role or provider
// owner is manufactured; fixture keys/stat values are explicitly synthetic.
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {compilerSdkSemanticMembersData} from '../cloud/dist/managed-home-provisioning/compiler-sdk-semantic-data.js';
import {A2_COMPILER_SDK,validateCompilerSdkCatalog} from '../cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
import {COMPILER_QUALIFICATION_SCOPE,compilerQualificationMac,compilerRoleBindingData,authenticateCompilerQualificationReady,validateCompilerQualificationScope} from '../cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import {assertOriginalRuntimeQualificationPurpose} from '../cloud/dist/managed-home-provisioning/runtime-qualification.js';
import {RootCompilerSdkQualifications} from '../cloud/dist/managed-home-provisioning/compiler-qualification.js';
import {originalTime} from '../cloud/dist/managed-home-provisioning/fly.js';
import {runtimeQualificationMac} from '../cloud/dist/managed-home-provisioning/runtime-qualification-protocol.js';
import {RUNTIME_QUALIFICATION_SQL} from '../cloud/dist/managed-home-provisioning/runtime-qualification-schema.js';
import {COMPILER_QUALIFICATION_SQL} from '../cloud/dist/managed-home-provisioning/compiler-qualification-schema.js';
import {COMPILER_ROLE_COMPLETION_SQL} from '../cloud/dist/managed-home-provisioning/compiler-qualification-completion-schema.js';
import {hash,canonical} from '../cloud/dist/managed-home-provisioning/types.js';
import {policy,ready} from './fixtures/managed-runtime-qualification/data.mjs';
const key=Buffer.alloc(32,23); // TEST ONLY, never a Root key.
function data(){
 const p={...policy(),compilerSdk:structuredClone(A2_COMPILER_SDK)},base=ready(p),s={...base.scope,scope:COMPILER_QUALIFICATION_SCOPE,sdk:structuredClone(A2_COMPILER_SDK)};
 const semantic=compilerSdkSemanticMembersData({kind:'directory',algorithm:'sha256',digest:'a'.repeat(64),size_bytes:230},{kind:'blob',algorithm:'sha256',digest:'1'.repeat(64),size_bytes:1},1,0o555);
 const tools={};for(const[name,path]of Object.entries({buck2:'buck2',rustc:'prepared/tools/rust/bin/rustc',node:'prepared/tools/node',typescript:'prepared/tools/node',python:'prepared/tools/python/bin/python3'})){
  tools[name]={version:semantic.directory.digest,closure:semantic.directory,executable:{kind:'blob',algorithm:'sha256',digest:'1'.repeat(64),size_bytes:1},executable_path:path,
   configuration:{executable_raw_sha256:'2'.repeat(64),...(name==='typescript'?{compiler_entrypoint:'prepared/tools/typescript/lib/tsc.js'}:{})}};
 }
 const step=out=>({exit_code:0,stdout_sha256:hash(out),stderr_sha256:hash(''),stdout_bytes:Buffer.byteLength(out),stderr_bytes:0});
 const end=BigInt(base.observation.root_ack_observed_boottime_nanoseconds)-1n;
 const workload={format_version:1,kind:'actual-fixed-rust-library-typescript-sdk-workload-v1',network_namespace:'net:[2]',user_namespace:'user:[3]',mount_namespace:'mnt:[4]',pid_namespace:'pid:[5]',
  started_boottime_nanoseconds:String(end-1_000_000n),completed_boottime_nanoseconds:String(end),
  sdk_python:{device:'10',inode:'9007199254740993333',sha256:'2'.repeat(64),size_bytes:1},sdk_root_device:'10',sdk_root_readonly:true,
  compiler_capabilities:{CapEff:'0000000000000000',CapPrm:'0000000000000000',CapInh:'0000000000000000',CapAmb:'0000000000000000',CapBnd:'0000000000000000'},buck2_static:true,
  buck2:step('buck2 fixture DATA'),rust:step(''),rust_result:step(''),typescript:step(''),typescript_result:step('rudder-sdk-typescript-ok\n'),
  rust_artifact_sha256:'3'.repeat(64),typescript_artifact_sha256:'4'.repeat(64),models:false,source_authority:false,normal_retirement_authority:false};
 const b={...base,kind:'compiler_sdk_qualification_ready',scope:s,sdk_proof:{mount:{semantic,floor:{device:10,inode:1,mount_id:11,directory:true},toolchains:tools},
  probe:{format_version:1,kind:'original-sdk-fixed-helper-complete-v1',helper_pid:104,helper_start_ticks:'1',original_constraint:base.observation.original_enable_constraint,workload}}};
 return{p,s,b,packet:()=>({body:b,mac:compilerQualificationMac(key,'ready',b)})};
}
test('SDK role identity remains full catalogue admission rather than the compile key',()=>{
 const a=data(),b=structuredClone(a.s);b.image='registry.fly.io/rudder-workers-exla@sha256:'+'5'.repeat(64);b.native_sha256='6'.repeat(64);b.source_manifest_sha256='7'.repeat(64);b.qualification_id=randomUUID();
 assert.deepEqual(compilerRoleBindingData(a.s),compilerRoleBindingData(b));
 const value=compilerRoleBindingData(a.s);assert(!JSON.stringify(value).includes('cas_image'));assert(!JSON.stringify(value).includes('native_sha256'));
 const bad=structuredClone(A2_COMPILER_SDK);bad.installer_input.digest='8'.repeat(64);assert.throws(()=>validateCompilerSdkCatalog(bad));
});
test('SDK canonical scope requires the independent actual SDK pins and refuses inferred C1',()=>{
 const f=data();assert.deepEqual(validateCompilerQualificationScope(f.s,f.p),f.s);
 for(const change of[s=>s.sdk.profile_id='rudder-linux-c1-guessed',s=>s.sdk.inventory=s.inventory,s=>s.sdk.cas_image=s.image]){
  const s=structuredClone(f.s);change(s);assert.throws(()=>validateCompilerQualificationScope(s,f.p));
 }
});
test('SDK DATA parser binds actual SDK helper/interpreter/outputs and preserves bigint kernel DATA',()=>{
 const f=data();const parsed=authenticateCompilerQualificationReady(f.packet(),key,f.s,f.b.registration_binding,f.b.registration_proof_sha256);
 assert.equal(parsed.body.sdk_proof.probe.workload.sdk_python.inode,'9007199254740993333');
 for(const change of[b=>b.sdk_proof.mount.semantic.prepared=b.scope.installer_input,b=>b.sdk_proof.mount.toolchains.rustc.closure=b.scope.installer_input,b=>b.sdk_proof.probe.workload.sdk_python.sha256='9'.repeat(64),b=>b.sdk_proof.probe.workload.buck2_static=false,
  b=>b.sdk_proof.probe.workload.compiler_capabilities.CapEff='0000000000000001',b=>b.sdk_proof.probe.workload.typescript_result.stdout_sha256=hash('wrong'),b=>b.sdk_proof.probe.workload.completed_boottime_nanoseconds=b.observation.original_enable_constraint.boottime_deadline_nanoseconds]){
  const b=structuredClone(f.b);change(b);assert.throws(()=>authenticateCompilerQualificationReady({body:b,mac:compilerQualificationMac(key,'ready',b)},key,f.s,b.registration_binding,b.registration_proof_sha256));
 }
});
test('Root SDK and runtime signing domains remain disjoint; no DATA purpose before database effects',async()=>{
 const f=data();assert.throws(()=>authenticateCompilerQualificationReady({body:f.b,mac:runtimeQualificationMac(key,'ready',f.b)},key,f.s,f.b.registration_binding,f.b.registration_proof_sha256));
 assert.throws(()=>assertOriginalRuntimeQualificationPurpose({assertOriginal(){}}));
 let effects=0;const pool={run:async()=>{effects++;throw Error('must not reach SQL')}};
 const source=new RootCompilerSdkQualifications(pool,pool,'fixture-authority',f.p);
 await assert.rejects(source.reserveRetaining({authorityId:'fixture-authority',compilerQualification:f.s},originalTime(30000),{assertOriginal(){}}));assert.equal(effects,0);
});
test('SDK schema is additive, immutable, and prepared-only rows cannot stand in for completion',()=>{
 assert(!RUNTIME_QUALIFICATION_SQL.includes('managed_compiler_'));
 assert(COMPILER_QUALIFICATION_SQL.includes('managed_compiler_sdk_roles'));assert(COMPILER_QUALIFICATION_SQL.includes('FORCE ROW LEVEL SECURITY'));
 assert(COMPILER_ROLE_COMPLETION_SQL.includes('managed_compiler_role_completions'));assert(COMPILER_ROLE_COMPLETION_SQL.includes("q.state='published'"));
 assert(!COMPILER_ROLE_COMPLETION_SQL.includes('GRANT UPDATE'));assert(!COMPILER_ROLE_COMPLETION_SQL.includes('GRANT DELETE'));
});

test('prepared SDK row without a completed original event cannot satisfy production admission',async()=>{
 const f=data();const {COMPILER_QUALIFICATION_MIGRATION_SHA256}=await import('../cloud/dist/managed-home-provisioning/compiler-qualification-schema.js');
 const {COMPILER_ROLE_COMPLETION_MIGRATION_SHA256}=await import('../cloud/dist/managed-home-provisioning/compiler-qualification-completion-schema.js');
 let joined=false;const pool={run:async(_account,work)=>({status:'committed',value:await work({query:async(sql)=>{
  if(sql.includes('SELECT i.state,i.policy_sha256'))return{rows:[{state:'ready',authority_state:'ready',home_state:'ready',home_policy:hash(canonical(f.p)),policy_sha256:hash(canonical(f.p.compilerSdk)),migration_sha256:COMPILER_QUALIFICATION_MIGRATION_SHA256,version_sha256:COMPILER_QUALIFICATION_MIGRATION_SHA256}]};
  if(sql.includes('SELECT sha256'))return{rows:[{sha256:COMPILER_ROLE_COMPLETION_MIGRATION_SHA256}]};
  if(sql.includes('managed_compiler_role_completions')){joined=true;return{rows:[]};}
  throw Error('unexpected modeled SQL');
 }})})};
 const source=new RootCompilerSdkQualifications(pool,pool,'fixture-authority',f.p);await assert.rejects(source.assertPublishedRole(f.p));assert(joined);
});
