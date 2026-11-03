// Pure JS transport/transaction models only. No physical/PG/native authority is
// manufactured or qualified by these fixtures. All tests WRITTEN and UNRUN.
import test from "node:test";
import assert from "node:assert/strict";
import {createHmac,randomUUID} from "node:crypto";
import {canonical,hash,nativeStatusReply,validateOperation} from "../cloud/dist/managed-home-provisioning/types.js";
import {RootEngineOperations} from "../cloud/dist/managed-home-provisioning/operations.js";
import {OPERATIONS_MIGRATION_SHA256} from "../cloud/dist/managed-home-provisioning/operations-schema.js";
import {originalTime} from "../cloud/dist/managed-home-provisioning/fly.js";
import {ManagedHomeRegistry} from "../cloud/dist/managed-home-provisioning/registry.js";

const op={op:"StartTaskV1",project_alias:"mp-fixture",backend:"codex",model:"fixture",profile_id:"fixture",text:"same task"};
function receipt(state="accepted"){return {command_key:"original-task",command_id:randomUUID(),project_alias:op.project_alias,payload_sha256:hash(canonical(op)),state,result:null};}
test("status separately accepts exact same-job App observations without changing original ACK",()=>{const r=receipt(),original=structuredClone(r);const observation={command_id:r.command_id,run_id:"run",run_epoch:"18446744073709551615",attempt_id:randomUUID(),row_status:"running",preparing_observed:false,backend_ready_last_observed:true,stop_requested_observed:null,uncertain_observed:false,validated_checkpoint_observed:null};
  const result=nativeStatusReply({receipt:r,task_observation:observation},r.command_key,op);assert.equal(result.task_observation.run_epoch,observation.run_epoch);assert.deepEqual(r,original);
  assert.throws(()=>nativeStatusReply({receipt:r,task_observation:{...observation,command_id:randomUUID()}},r.command_key,op),{code:"identity_changed"});assert.throws(()=>nativeStatusReply({receipt:r,task_observation:{...observation,run_epoch:"01"}},r.command_key,op),{code:"identity_changed"});});
test("null status is unavailable DATA and cannot carry an invented observation",()=>{assert.deepEqual(nativeStatusReply({receipt:null,task_observation:null},"original-task",op),{receipt:null,task_observation:null});assert.throws(()=>nativeStatusReply({receipt:null,task_observation:{}},"original-task",op),{code:"identity_changed"});});
test("explicit StopTask requires the original command and canonical fence",()=>{
 const stop={op:"StopTaskV1",project_alias:op.project_alias,lookup_command_key:"original-task",command_id:randomUUID(),run_id:"run",run_epoch:"4",attempt_id:randomUUID()};
 assert.deepEqual(validateOperation(stop),stop);
 for(const change of [s=>delete s.command_id,s=>s.run_epoch="04",s=>s.run_epoch="0",s=>s.run_epoch="18446744073709551616",s=>s.pid=1234,s=>s.backend="codex"]){const wrong={...stop};change(wrong);assert.throws(()=>validateOperation(wrong));}
});
test("lifecycle data keeps a missing physical join unknown and refuses invented boolean completion",()=>{
 const r=receipt(),o={command_id:r.command_id,run_id:"run",run_epoch:"4",attempt_id:randomUUID(),row_status:"stopped",preparing_observed:false,
  backend_ready_last_observed:false,stop_requested_observed:true,uncertain_observed:false,validated_checkpoint_observed:true,
  enrollment_submitted_observed:true,enrollment_released_observed:true,helper_joined_observed:null};
 const data={receipt:r,task_observation:o};assert.equal(nativeStatusReply(data,r.command_key,op).task_observation.helper_joined_observed,null);
 assert.equal(nativeStatusReply({...data,task_observation:{...o,helper_joined_observed:true}},r.command_key,op).task_observation.helper_joined_observed,true);
 for(const change of [s=>s.helper_joined_observed=false,s=>s.enrollment_submitted_observed=false,s=>delete s.enrollment_released_observed,s=>s.helper_joined_observed="true"]){const wrong=structuredClone(data);change(wrong.task_observation);assert.throws(()=>nativeStatusReply(wrong,r.command_key,op));}
});
test("ReceiptV1 always queries original native key despite cached accepted/completed dispatch DATA",async()=>{const r=receipt("completed"),i={authorityId:randomUUID(),accountId:"account",homeId:"mh-model",storeId:randomUUID(),incarnation:randomUUID(),nonce:randomUUID(),epoch:"1",appName:"app",networkName:"network",policy:{},spec:{projects:[{alias:op.project_alias}]}};
  const bound={intent:i,acks:{app:{id:"app"},volume:{id:"volume"},start:{id:"machine",instanceId:"original"}},phase:"routable",tokenHash:"1".repeat(64)};const assignment={projectId:randomUUID(),ownerEpoch:"2",engineInstance:randomUUID(),bootId:randomUUID(),ownerReceipt:{model:true}};
  let queries=0;const db={bound:async()=>structuredClone(bound),repo:async()=>({home:{id:i.homeId},repo:{id:op.project_alias}}),currentAssignment:async()=>structuredClone(assignment),command:async()=>({phase:"replied",operation:op,reply:{receipt:r}})};
  const provider={observeApp:async()=>{},observeMachine:async()=>{}};const registry=new ManagedHomeRegistry(db,provider,{},Buffer.alloc(32,1));const route=await registry.resolveManagedHomeRoute(i.accountId,i.homeId,op.project_alias);
  for(let n=0;n<2;n++){const status=await registry.dispatchManagedHomeCommand(route,"query-key",{op:"ReceiptV1",lookup_command_key:r.command_key},async(method,params,remaining)=>{queries++;assert.equal(method,"managed.receiptV1");assert.equal(params.lookup_command_key,r.command_key);assert(remaining>0&&remaining<=20000);return {receipt:r,task_observation:null};});assert.equal(status.receipt.command_id,r.command_id);}
  assert.equal(queries,2);assert.equal(r.state,"completed");});
function operationFixture(kind="FreshProjectStatusV1"){
  const rootKey=Buffer.alloc(32,7),engine=randomUUID(),credentialId=randomUUID(),rootNonce=randomUUID(),challengeId=randomUUID(),boot=randomUUID();const i={accountId:"account",homeId:"mh-model",storeId:randomUUID(),incarnation:randomUUID(),nonce:randomUUID()};
  const challenge={challenge_id:challengeId,engine_instance_id:engine,credential_id:credentialId,root_nonce:rootNonce};const sign=(key,domain,body)=>createHmac("sha256",key).update(domain).update(canonical(body)).digest("hex");
  const credential=Buffer.from("rdre_"+sign(rootKey,"rudder_engine_registration_credential_v1\0",challenge));const operation={op:kind,workspace_id:"mp-model"};const body={version:1,kind:"engine_operation",account_id:i.accountId,home_id:i.homeId,store_id:i.storeId,incarnation:i.incarnation,engine_instance_id:engine,boot_id:boot,credential_id:credentialId,epoch:"1",root_nonce:rootNonce,registration_proof_sha256:"a".repeat(64),request_nonce:randomUUID(),command_key:"stable",operation,payload_sha256:hash(canonical(operation)),constraint:{format_version:1,maximum_milliseconds:30000,boottime_deadline_nanoseconds:"999999999"},remaining_milliseconds:30000};
  const signed={body,mac:sign(credential,"rudder_engine_operation_request_v1\0",body)};const row={challenge_json:{body:challenge},proof_sha256:body.registration_proof_sha256,principal_id:"engine",token_sha256:hash(credential),credential_id:credentialId,engine_instance_id:engine,machine_boot_id:boot};return {rootKey,credential,home:{intent:i},signed,row};
}
function authPool(f,onAfterAuth){let runs=0;return {run:async(account,body)=>{runs++;if(runs>1&&onAfterAuth)return onAfterAuth(body);const tx={query:async sql=>sql.includes("managed_home_operations_migrations")?{rows:[{sha256:OPERATIONS_MIGRATION_SHA256}]}:{rows:[f.row]}};return {status:"committed",value:await body(tx)};}};}
test("wrong Root operation MAC cannot invoke a real preparation/assignment step",async()=>{const f=operationFixture("PrepareFreshProjectV1");let called=0;const store={prepareFresh:async()=>{called++;}};const api=new RootEngineOperations(authPool(f),randomUUID(),store);await assert.rejects(api.perform(f.home,f.rootKey,{...f.signed,mac:"f".repeat(64)},originalTime(1000)),{code:"effect_unknown"});assert.equal(called,0);});
test("authenticated status has no mutation/outbox/admission row",async()=>{const f=operationFixture();let reads=0;const store={freshStatus:async()=>{reads++;return {scope:"fresh-project-status-only",state:"reserved"};}};const api=new RootEngineOperations(authPool(f,()=>{throw new Error("status cannot start mutation transaction");}),randomUUID(),store);const result=await api.perform(f.home,f.rootKey,f.signed,originalTime(1000));assert.equal(result.body.result.scope,"fresh-project-status-only");assert.equal(result.body.error,null);assert.equal(reads,1);assert(!JSON.stringify(result).includes(f.credential.toString()));});
test("unknown Started commit retains original request and never dispatches source preparation",async()=>{const f=operationFixture("PrepareFreshProjectV1");let calls=0;const pool=authPool(f,async()=>({status:"uncertain",reconcile:"retry_same_operation"}));const api=new RootEngineOperations(pool,randomUUID(),{prepareFresh:async()=>{calls++;return {};}});await assert.rejects(api.perform(f.home,f.rootKey,f.signed,originalTime(1000)),{code:"effect_unknown"});assert.equal(calls,0);});
