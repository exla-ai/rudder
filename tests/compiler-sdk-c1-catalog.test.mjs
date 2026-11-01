// Pure catalogue/constructed HMAC DATA tests. Fixture keys/kernel values grant
// no Root/SDK role, provider custody, preparation, execution or warm result.
import test from 'node:test';import assert from 'node:assert/strict';import {readFileSync} from 'node:fs';import {createHash} from 'node:crypto';
import {A2_COMPILER_SDK,C1_COMPILER_SDK,validateCompilerSdkCatalog,compilerSdkProfileData} from '../cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
import {compilerSdkSemanticMembersData} from '../cloud/dist/managed-home-provisioning/compiler-sdk-semantic-data.js';
import {COMPILER_QUALIFICATION_SCOPE,compilerRoleBindingData,compilerQualificationMac,authenticateCompilerQualificationReady} from '../cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import {hash,canonical} from '../cloud/dist/managed-home-provisioning/types.js';
import {policy,ready} from './fixtures/managed-runtime-qualification/data.mjs';
const fixture=name=>JSON.parse(readFileSync(new URL(`../engine/src/execution/fixtures/${name}`,import.meta.url),'utf8'));
const key=Buffer.alloc(32,23); // TEST ONLY.
test('C1 exact catalogue matches genuine A6 and separately baked CAS receipts; neither is a role',()=>{
 const artifact=fixture('compiler_c1_artifact_a6.json'),cas=fixture('compiler_c1_cas_a2.json'),functional=fixture('compiler_c1_functional_a5.json'),marker=cas.results[0].marker;
 assert.deepEqual(validateCompilerSdkCatalog(C1_COMPILER_SDK),C1_COMPILER_SDK);assert.equal(C1_COMPILER_SDK.artifact_image.split('@')[1],artifact.digest);assert.equal(C1_COMPILER_SDK.cas_image.split('@')[1],cas.digest);
 for(const[key,value]of Object.entries({artifact_manifest:marker.compiler.artifact_manifest,inventory:marker.inventory,manifest:marker.roots.manifest,installer_input:marker.roots.installer_input}))assert.deepEqual(C1_COMPILER_SDK[key],value);
 const raw=readFileSync(new URL('../engine/src/execution/fixtures/compiler_c1_source_binding.json',import.meta.url));assert.equal(createHash('sha256').update(raw).digest('hex'),C1_COMPILER_SDK.source_binding_sha256);
 assert.equal(artifact.runtimeQualified,false);assert.equal(cas.runtimeQualified,false);assert.equal(functional.sdkRoleQualified,false);assert.equal(functional.guest.functional.physical_Root_Native_SDK_grant,false);assert.equal(functional.guest.functional.Rudder_repository_build,false);
});
test('known catalogue values are immutable; unknown, superseded and mixed pins never infer C1',()=>{
 assert.throws(()=>{C1_COMPILER_SDK.inventory.digest='0'.repeat(64)});assert.throws(()=>{A2_COMPILER_SDK.installer_input.size_bytes=0});
 for(const key of ['artifact_image','cas_image','artifact_manifest','inventory','manifest','installer_input','source_binding_sha256']){const wrong=structuredClone(C1_COMPILER_SDK);wrong[key]=structuredClone(A2_COMPILER_SDK[key]);assert.throws(()=>validateCompilerSdkCatalog(wrong));}
 for(const change of [s=>s.profile_id='rudder-linux-rust-cxx-typescript-c2',s=>s.artifact_image='registry.fly.io/rudder-workers-exla@sha256:b5d7681a9caf7527425e372628f54893ea26d50a6f01b522a5accc9a24661ffe',s=>s.manifest.size_bytes++,s=>s.installer_input.kind='blob',s=>s.runtime_mount=true]){const wrong=structuredClone(C1_COMPILER_SDK);change(wrong);assert.throws(()=>validateCompilerSdkCatalog(wrong));}
});
test('A2 overlay and five identities remain unchanged; C1 adds metadata paths only',()=>{
 assert.deepEqual(validateCompilerSdkCatalog(A2_COMPILER_SDK),A2_COMPILER_SDK);const a=compilerSdkProfileData(A2_COMPILER_SDK),c=compilerSdkProfileData(C1_COMPILER_SDK);
 assert.equal(a.cas_directory,'/var/lib/rudder-compiler-cas-v1');assert.equal(a.artifact_directory,'/rudder-compiler-v1');assert.equal(c.cas_directory,'/var/lib/rudder-compiler-c1-cas-v1');assert.equal(c.artifact_directory,'/rudder-compiler-c1');
 assert.equal(a.source_mount,c.source_mount);assert.equal(a.guest_mount,c.guest_mount);assert.deepEqual(a.tools.map(t=>t.name),['buck2','rustc','node','typescript','python']);assert.equal(c.tools.length,9);
 assert.deepEqual(c.tools.slice(5),[{name:'cargo',relative_path:'prepared/tools/rust/bin/cargo'},{name:'cc',relative_path:'prepared/usr/bin/cc'},{name:'cxx',relative_path:'prepared/usr/bin/c++'},{name:'ar',relative_path:'prepared/usr/bin/ar'}]);
 assert.notDeepEqual(compilerRoleBindingData({sdk:A2_COMPILER_SDK}),compilerRoleBindingData({sdk:C1_COMPILER_SDK}));
});
function packetData(sdk){
 const p={...policy(),compilerSdk:structuredClone(sdk)},base=ready(p),scope={...base.scope,scope:COMPILER_QUALIFICATION_SCOPE,sdk:structuredClone(sdk)};
 const semantic=compilerSdkSemanticMembersData({kind:'directory',algorithm:'sha256',digest:'a'.repeat(64),size_bytes:230},{kind:'blob',algorithm:'sha256',digest:'1'.repeat(64),size_bytes:1},1,0o555);
 const tools={};for(const spec of compilerSdkProfileData(sdk).tools){tools[spec.name]={version:semantic.directory.digest,closure:semantic.directory,executable:{kind:'blob',algorithm:'sha256',digest:'1'.repeat(64),size_bytes:1},executable_path:spec.relative_path,configuration:{executable_raw_sha256:'2'.repeat(64),...(spec.compiler_entrypoint?{compiler_entrypoint:spec.compiler_entrypoint}:{})}};}
 const step=out=>({exit_code:0,stdout_sha256:hash(out),stderr_sha256:hash(''),stdout_bytes:Buffer.byteLength(out),stderr_bytes:0});const end=BigInt(base.observation.root_ack_observed_boottime_nanoseconds)-1n;
 const workload={format_version:1,kind:'actual-fixed-rust-library-typescript-sdk-workload-v1',network_namespace:'net:[2]',user_namespace:'user:[3]',mount_namespace:'mnt:[4]',pid_namespace:'pid:[5]',started_boottime_nanoseconds:String(end-1_000_000n),completed_boottime_nanoseconds:String(end),sdk_python:{device:'10',inode:'9007199254740993333',sha256:'2'.repeat(64),size_bytes:1},sdk_root_device:'10',sdk_root_readonly:true,compiler_capabilities:{CapEff:'0000000000000000',CapPrm:'0000000000000000',CapInh:'0000000000000000',CapAmb:'0000000000000000',CapBnd:'0000000000000000'},buck2_static:true,buck2:step('buck2 fixture DATA'),rust:step(''),rust_result:step(''),typescript:step(''),typescript_result:step('rudder-sdk-typescript-ok\n'),rust_artifact_sha256:'3'.repeat(64),typescript_artifact_sha256:'4'.repeat(64),models:false,source_authority:false,normal_retirement_authority:false};
 const body={...base,kind:'compiler_sdk_qualification_ready',scope,sdk_proof:{mount:{semantic,floor:{device:10,inode:1,mount_id:11,directory:true},toolchains:tools},probe:{format_version:1,kind:'original-sdk-fixed-helper-complete-v1',helper_pid:104,helper_start_ticks:'1',original_constraint:base.observation.original_enable_constraint,workload}}};return {scope,body};
}
test('C1 constructed signed DATA requires all nine exact members; old Root helper/rules facts unchanged',()=>{
 const f=packetData(C1_COMPILER_SDK);const auth=body=>authenticateCompilerQualificationReady({body,mac:compilerQualificationMac(key,'ready',body)},key,f.scope,body.registration_binding,body.registration_proof_sha256);
 assert.deepEqual(auth(f.body).body,f.body);assert(canonical(f.body,24*1024).length<=24*1024);assert.equal(f.body.sdk_proof.probe.workload.kind,'actual-fixed-rust-library-typescript-sdk-workload-v1');
 for(const change of [b=>delete b.sdk_proof.mount.toolchains.cc,b=>b.sdk_proof.mount.toolchains.cc.executable_path='/usr/bin/cc',b=>b.sdk_proof.mount.toolchains.cxx.executable_path='prepared/usr/bin/g++',b=>b.sdk_proof.mount.toolchains.cargo.configuration.command='cargo build',b=>b.sdk_proof.mount.toolchains.ar.closure=b.scope.installer_input]){const wrong=structuredClone(f.body);change(wrong);assert.throws(()=>auth(wrong));}
 const a=packetData(A2_COMPILER_SDK);const wrong=structuredClone(a.body);wrong.sdk_proof.mount.toolchains.cc=f.body.sdk_proof.mount.toolchains.cc;assert.throws(()=>authenticateCompilerQualificationReady({body:wrong,mac:compilerQualificationMac(key,'ready',wrong)},key,a.scope,wrong.registration_binding,wrong.registration_proof_sha256));
});
