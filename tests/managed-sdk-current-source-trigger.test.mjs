// Constructed public policy/wire DATA only. No provider/PG/Root/source/SDK grant.
// Definitions UNRUN by the private author.
import test from 'node:test';import assert from 'node:assert/strict';import {randomUUID} from 'node:crypto';
import {parseManagedMachineInput} from '../cloud/dist/managed-machine-api.js';
import {validateOperation,nativeReply,nativeStatusReply,validatePolicy,canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {managedHomeProtectedConfigData} from '../cloud/dist/managed-home-provisioning/protected-config.js';
import {C1_COMPILER_SDK} from '../cloud/dist/managed-home-provisioning/compiler-sdk-catalog.js';
import {COMPILER_QUALIFICATION_SCOPE} from '../cloud/dist/managed-home-provisioning/compiler-qualification-protocol.js';
import {policy,scope} from './fixtures/managed-runtime-qualification/data.mjs';
import {reserveFreshProjects,freshSpec} from '../cloud/dist/managed-home-provisioning/fresh.js';
const operation=()=>({op:'BuildCurrentSourceSdkV1',project_alias:'actual-repo-alias',case:'accepted',repeat_identical:true});
const selected=()=>({...policy(),compilerSdk:structuredClone(C1_COMPILER_SDK),sdkInitialSource:'sdk-fixed-suite-v1'});
function intent(p=selected()){const freshProjects=reserveFreshProjects(['Fixed SDK source']),storeId=randomUUID(),nonce=randomUUID();return {authorityId:randomUUID(),accountId:'account',homeId:'home',commandKey:'provision',requestHash:'a'.repeat(64),epoch:'1',nonce,appName:'rudder-mh-'+nonce.replaceAll('-',''),networkName:'rudder-mhn-'+randomUUID().replaceAll('-',''),storeId,incarnation:randomUUID(),processOwner:randomUUID(),policy:p,spec:freshSpec(freshProjects,storeId,p),freshProjects};}
const receipt=()=>({command_key:'original-key',command_id:randomUUID(),project_alias:operation().project_alias,payload_sha256:hash(canonical(operation())),state:'accepted',result:null});
test('SDK HTTP input has explicit case/key/repeat and carries no model or arbitrary source',()=>{
 for(const value of ['accepted','proc-deny','job-deny']){const input={workspace:'workspace-1',command_key:'original-key',case:value,repeat_identical:false};const parsed=parseManagedMachineInput('build-current-source-sdk',input);assert.equal(parsed.sdkCase,value);assert.equal(parsed.selection,undefined);assert.equal(parsed.commandKey,input.command_key);}
 for(const change of [i=>delete i.case,i=>delete i.repeat_identical,i=>i.repeat_identical='true',i=>i.case='unknown',i=>i.model='fake',i=>i.source_root={kind:'blob'},i=>i.backend='codex']){const input={workspace:'workspace-1',command_key:'original-key',case:'accepted',repeat_identical:true};change(input);assert.throws(()=>parseManagedMachineInput('build-current-source-sdk',input));}
 assert.throws(()=>parseManagedMachineInput('start-task',{workspace:'workspace-1',command_key:'k',case:'accepted',repeat_identical:true}));
});
test('new stable canonical operation excludes transport IDs and refuses arbitrary build instructions',()=>{
 assert.deepEqual(validateOperation(operation()),operation());
 for(const value of [{...operation(),case:'unknown'},{...operation(),repeat_identical:1},{...operation(),backend:'codex'},{...operation(),source_root:'data'},{...operation(),command:'cargo build'}])assert.throws(()=>validateOperation(value));
 const wrong={...operation(),case:'proc-deny'};assert.notEqual(hash(canonical(operation())),hash(canonical(wrong)));
});
test('operator fixed profile is explicit and canonical protected producer omits it for BOTH qualification purposes',()=>{
 const p=selected();validatePolicy(p);const i=intent(p);const ordinary=JSON.parse(managedHomeProtectedConfigData(i,{id:'machine'}));assert.equal(ordinary.sdk_initial_source,'sdk-fixed-suite-v1');
 const runtime={...i,runtimeQualification:scope(p)};assert.equal(JSON.parse(managedHomeProtectedConfigData(runtime,{id:'machine'})).sdk_initial_source,undefined);
 const compiler={...i,compilerQualification:{...scope(p),scope:COMPILER_QUALIFICATION_SCOPE,sdk:p.compilerSdk}};assert.equal(JSON.parse(managedHomeProtectedConfigData(compiler,{id:'machine'})).sdk_initial_source,undefined);
 const old=structuredClone(i);delete old.policy.sdkInitialSource;assert.equal(JSON.parse(managedHomeProtectedConfigData(old,{id:'machine'})).sdk_initial_source,undefined);assert.notEqual(hash(managedHomeProtectedConfigData(i,{id:'machine'})),hash(managedHomeProtectedConfigData(old,{id:'machine'})));
 const absent=intent(policy()),without=structuredClone(absent);delete without.policy.sdkInitialSource;assert(managedHomeProtectedConfigData(absent,{id:'machine'}).equals(managedHomeProtectedConfigData(without,{id:'machine'})));
});
test('fixed source policy refuses missing SDK/runtime and unknown profile before any provider intent',()=>{
 for(const change of [p=>delete p.compilerSdk,p=>delete p.imagePreseed.runtimeMount,p=>p.sdkInitialSource='any-path']){const p=selected();change(p);assert.throws(()=>validatePolicy(p));}
});
test('SDK query observation is separate from immutable accepted reply and cannot replace two genuine joins with booleans',()=>{
 const row=receipt(),original={receipt:row};assert.deepEqual(nativeReply(original,'original-key',operation()),original);
 const observation={command_key:'original-key',command_id:row.command_id,project_alias:row.project_alias,build_worker_join_observed:false,cleanup_worker_join_observed:false,job_disposal:'pending',source_authority:false,shared_floor_retirement:false};
 const query={receipt:row,task_observation:null,sdk_observation:observation};assert.deepEqual(nativeStatusReply(query,'original-key',operation()),query);assert.throws(()=>nativeReply(query,'original-key',operation()));
 for(const change of [s=>s.command_key='another',s=>s.command_id=randomUUID(),s=>s.project_alias='another',s=>s.job_disposal='settled',s=>s.source_authority=true,s=>s.shared_floor_retirement=true,s=>s.cleanup_worker_join_observed='true']){const wrong=structuredClone(query);change(wrong.sdk_observation);assert.throws(()=>nativeStatusReply(wrong,'original-key',operation()));}
 const complete=structuredClone(query);Object.assign(complete.sdk_observation,{build_worker_join_observed:true,cleanup_worker_join_observed:true,job_disposal:'settled'});assert.deepEqual(nativeStatusReply(complete,'original-key',operation()),complete);
 // These are authenticated status DATA only; the consumer cannot issue Native
 // JobSettled or worker/source/retirement owners from this snapshot.
 assert.deepEqual(original,{receipt:row});
});
