// ROOT-RUN ONLY through the sealed real-PG qualifier's hash-bound followup hook.
// Actual PG/FabricCas paths; fixture Root/provider/clock and memory S3 remain
// explicit. This is not physical native/watchdog or production clock evidence.
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
import {performance} from 'node:perf_hooks';
import {createManagedOwnerRenewalMigrator} from '../cloud/dist/managed-home-provisioning/index.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
const S='rudder_project_authority';
const sleep=milliseconds=>new Promise(resolve=>setTimeout(resolve,milliseconds));

export async function prepareFixturePolicy({policy,authorityId,config,query}) {
 assert.equal(policy.ownerLeaseSafety,undefined);
 // This is before any intent/hash/provider effect, not a rewrite/adoption.
 policy.image='registry.fly.io/rudder-workers-exla@sha256:'+policy.image.split('@sha256:')[1];
 policy.ownerLeaseSafety={qualificationReceiptSha256:hash('SYNTHETIC real-PG component clock fixture ONLY'),marginMilliseconds:250,engineRateErrorPpm:1000,databaseRateErrorPpm:1000,maxPositiveClockStepMilliseconds:100};
 return {policy,async installAfterMigrations(){
  const migrator=createManagedOwnerRenewalMigrator(config('postgres'),hash(canonical(policy.ownerLeaseSafety)));
  try{assert.equal((await migrator.migrate('rudder_managed_runtime')).status,'committed');}finally{await migrator.close();}
  const before=(await query(`SELECT state FROM ${S}.managed_owner_clock_installation WHERE authority_id=$1`,[authorityId])).rows[0];assert.equal(before.state,'frozen');
  // Explicit fixture administrator admission; never a production ready API.
  await query(`UPDATE ${S}.managed_owner_clock_installation SET state='ready' WHERE authority_id=$1`,[authorityId]);
 }};
}

export async function qualify({active,workspace,initialAcquire,operation,perform,budget,query,backend,graph,missing,put}) {
 assert.equal(backend.evidence.actualS3,false,'this bounded mixed fixture requires the explicit held memory-S3 adapter');
 assert.equal(typeof backend.controls?.holdNext,'function');
 const original=initialAcquire.body.result.owner_receipt.data;
 let current=original,localCutoff=performance.now()+1000;
 const renewals=[];
 const renew=async first=>{
  const input=operation({op:'RenewFreshOwnerV1',workspace_id:active.workspaceId,owner_epoch:current.ownerEpoch,lease_id:current.leaseId,request_id:randomUUID(),expected_serial:current.renewalSerial},'mixed-renew-'+randomUUID(),await budget(2000));
  const sent=performance.now(),old=localCutoff;
  const ack=await perform(input),received=performance.now();assert.equal(ack.body.error,null);
  const next=ack.body.result.owner_grant;assert.equal(next.projectId,original.projectId);assert.equal(next.engineInstanceId,original.engineInstanceId);assert.equal(next.machineBootId,original.machineBootId);assert.equal(next.ownerEpoch,original.ownerEpoch);assert.equal(next.leaseId,original.leaseId);
  assert.equal(BigInt(next.renewalSerial),BigInt(current.renewalSerial)+1n);assert.equal(next.requestId,input.body.operation.request_id);
  // First call's native cutoff is intentionally not invented from a receipt.
  // Later fixture request clocks are captured before each actual Cloud send.
  if(!first)assert(received<old,'old fixture conservative deadline crossed before ACK');
  localCutoff=sent+14000;assert(received<localCutoff);current=next;renewals.push({request_id:next.requestId,serial:next.renewalSerial,elapsed_ms:Math.ceil(received-sent)});
  const retry=await perform(input);assert.deepEqual(retry,ack); // Historical ACK, no extension.
  return {input,ack};
 };
 await renew(true);
 const chain=await budget(30000);
 const prepared=await perform(operation({op:'PrepareFreshProjectV1',workspace_id:workspace.workspaceId},'mixed-prepare',chain));assert.equal(prepared.body.error,null);
 const begun=await perform(operation({op:'BeginFreshRootsV1',workspace_id:workspace.workspaceId,prepare_id:prepared.body.result.prepare.request_id,export_id:randomUUID(),head:'b'.repeat(40),source:graph.source,environment:graph.environment},'mixed-begin',chain));assert.equal(begun.body.error,null);
 const transfer=begun.body.result.transfer_id,needed=await missing(transfer,graph.all.map(item=>item.ref));assert.equal(needed.status,200);
 const required=new Set(needed.body.missing.map(ref=>ref.kind+':'+ref.digest));for(const item of graph.all)if(required.has(item.ref.kind+':'+item.ref.digest))assert.equal((await put(transfer,item)).status,200);
 const gate=backend.controls.holdNext(({operation})=>operation==='PutObjectCommand');
 const request=operation({op:'PublishFreshRootsV1',workspace_id:workspace.workspaceId,transfer_id:transfer},'mixed-publish',chain);
 const publication={settled:false,result:null,error:null,promise:null};
 publication.promise=perform(request).then(result=>{publication.settled=true;publication.result=result;},error=>{publication.settled=true;publication.error=error;});
 let entered=false,heldMs=0;
 try {
  await Promise.race([gate.entered.then(()=>{entered=true;}),sleep(4000).then(()=>{assert(entered,'actual S3 publication gate was not entered');})]);
  const heldAt=performance.now();
  for(let i=0;i<9;i++){await sleep(2000);assert.equal(publication.settled,false);await renew(false);}
  heldMs=Math.floor(performance.now()-heldAt);assert(heldMs>15000);
  const live=(await query(`SELECT owner_epoch::text,lease_id::text,renew_serial::text,clock_timestamp()<lease_until AS live,clock_timestamp()>$2::timestamptz AS original_expired FROM ${S}.projects WHERE project_id=$1`,[original.projectId,original.leaseUntil])).rows[0];
  assert.equal(live.live,true);assert.equal(live.original_expired,true);assert.equal(live.owner_epoch,original.ownerEpoch);assert.equal(live.lease_id,original.leaseId);assert.equal(live.renew_serial,current.renewalSerial);
  const pending=(await query(`SELECT state FROM ${S}.managed_home_engine_operations WHERE command_key='mixed-publish'`)).rows[0];assert.equal(pending.state,'started');
 } finally {gate.release();await publication.promise;}
 assert.equal(publication.error,null);assert.equal(publication.result.body.error,null);assert.equal(publication.result.body.result.scope,'fresh-bootstrap-only');
 const count=(await query(`SELECT count(*)::text AS count FROM ${S}.managed_owner_renewals`)).rows[0].count;assert.equal(Number(count),renewals.length);
 const bad=operation({op:'RenewFreshOwnerV1',workspace_id:active.workspaceId,owner_epoch:current.ownerEpoch,lease_id:randomUUID(),request_id:randomUUID(),expected_serial:current.renewalSerial},'mixed-wrong-owner',await budget(2000));await assert.rejects(perform(bad));
 const unchanged=(await query(`SELECT lease_id::text,renew_serial::text FROM ${S}.projects WHERE project_id=$1`,[original.projectId])).rows[0];assert.equal(unchanged.lease_id,original.leaseId);assert.equal(unchanged.renew_serial,current.renewalSerial);
 return {actualMixedPgQualification:true,scope:'actual-PG-and-FabricCas-mixed-publication-renewal-component-only',fixtureRoot:true,fixtureProvider:true,fixtureClockAdmission:true,actualS3:false,physicalNativeWatchdog:false,qualifiedClockContinuity:false,productionEnablement:false,heldPublicationMilliseconds:heldMs,renewals,originalLeaseExpiredWhileCurrentOwnerLive:true,publicationOriginalAck:true,wrongOwnerRejected:true};
}
