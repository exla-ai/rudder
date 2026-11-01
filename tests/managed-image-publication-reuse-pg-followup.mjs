// ROOT-RUN ONLY through the EXISTING hash-bound real-PG harness hook.
// Actual PG/RLS/transactions and actual FabricCas/S3 adapter; memory service,
// synthetic operator/Root/provider attestation. Not physical VM/native/S3 proof.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {FabricCas} from '../cloud/dist/fabric-cas.js';
import {prepareFixturePolicy as prepareOwnerClockFixture} from './managed-owner-renewal-pg-followup.mjs';
let authority,retainedStats;
let phase='not-started',facts={};
const safeStats=()=>Object.fromEntries(['get','put','delete','bytes'].map(k=>[k,Number.isSafeInteger(retainedStats?.[k])&&retainedStats[k]>=0?retainedStats[k]:null]));
const warmDelta=()=>facts.warmCounters??(phase==='perform-warm-BeginImage'&&facts.beforeWarm?Object.fromEntries(Object.keys(facts.beforeWarm).map(k=>[k,Number.isSafeInteger(retainedStats?.[k])&&Number.isSafeInteger(facts.beforeWarm[k])?retainedStats[k]-facts.beforeWarm[k]:null])):null);
export function failureData(){return {scope:'real-PG-account-image-reuse-failure-data-only',failedPhase:phase,facts,backendCounters:safeStats(),warmAttemptDelta:warmDelta(),actualPostgres:true,actualS3:false,physicalNative:false,realRootRegistration:false,productionEnablement:false,normalRootRetirement:false,replayAuthorized:false};}
export async function prepareFixturePolicy(input){authority=input.authorityId;return prepareOwnerClockFixture(input);}
const refKey=r=>[r.kind,r.algorithm,r.digest,r.size_bytes].join(':');
export async function qualify({account,workspace,operation,perform,budget,query,backend,graph}){
 phase='preflight-backend';retainedStats=backend.stats;facts={};
 assert(authority);assert.equal(backend.evidence.actualS3,false,'explicit conditional-memory S3 fixture only');
 assert.equal(typeof backend.controls.objectBodyReads,'function');
 const S='rudder_project_authority';
 phase='read-original-publication';
 const prior=(await query(`SELECT * FROM ${S}.managed_image_publications WHERE authority_id=$1 AND account_id=$2 AND image=$3`,[authority,account,graph.policy.image])).rows;
 assert.equal(prior.length,1);const original=prior[0],originalJson=JSON.stringify(original);
 facts.publicationKeys=Object.keys(original.publication_json??{}).sort();
 phase='read-original-cold-transfer';
 const oldRows=(await query(`SELECT phase,scope,stage_json,request_json,publication_json FROM ${S}.managed_storage_transfers WHERE authority_id=$1 AND account_id=$2 AND transfer_id=$3`,[authority,account,original.transfer_id])).rows;
 assert.equal(oldRows.length,1);const cold=oldRows[0];facts.cold={phase:cold.phase,scope:cold.scope,stageIsNull:cold.stage_json===null,requestOperation:cold.request_json?.operation?.op,
  manifestDigestMatches:cold.request_json?.operation?.manifest?.digest===graph.image.manifest.digest,
  installerDigestMatches:cold.request_json?.operation?.installer_input?.digest===graph.image.installer.digest};
 assert.equal(cold.phase,'published');assert.equal(cold.scope,'image');assert.equal(cold.request_json.operation.op,'BeginImagePreseedV1');
 assert.deepEqual(cold.request_json.operation.manifest,graph.image.manifest);assert.deepEqual(cold.request_json.operation.installer_input,graph.image.installer);assert.deepEqual(cold.publication_json,original.publication_json.installer);
 phase='preflight-live-root-receipt-representation';
 const cas=new FabricCas({storage:backend.storage,accountId:account});const preflightBefore=safeStats();
 const manifestRoot=await cas.getRoot(original.publication_json.manifest.root_id),installerRoot=await cas.getRoot(original.publication_json.installer.root_id);
 facts.rootRepresentation={manifestExists:manifestRoot!==null,installerExists:installerRoot!==null,manifestKeys:manifestRoot?Object.keys(manifestRoot).sort():[],installerKeys:installerRoot?Object.keys(installerRoot).sort():[],preflightRootReceiptGets:safeStats().get-preflightBefore.get};
 assert.deepEqual(manifestRoot,original.publication_json.manifest);assert.deepEqual(installerRoot,original.publication_json.installer);
 // Genuine reservation via original signed registered-engine operation. This
 // fourth fixture namespace was never bootstrapped/acquired by the base test.
 phase='prepare-current-reservation';
 const preparation=await perform(operation({op:'PrepareFreshProjectV1',workspace_id:workspace.workspaceId},'reuse-preparation',await budget(30000)));
 assert.equal(preparation.body.error,null);assert.equal(preparation.body.result.scope,'fresh-preparation-only');
 const begin=()=>({op:'BeginImagePreseedV1',workspace_id:workspace.workspaceId,manifest:graph.image.manifest,installer_input:graph.image.installer,image_receipt_sha256:graph.policy.runtimeImageReceiptSha256});
 const bodyReads=()=>new Map(graph.image.objects.map(o=>[refKey(o.ref),backend.controls.objectBodyReads(account,o.ref)]));
 const before={...backend.stats},payloadBefore=bodyReads();facts.beforeWarm=safeStats();
 const request=operation(begin(),'same-account-image-reuse',await budget(300000));
 phase='perform-warm-BeginImage';
 let ack;try{ack=await perform(request);}catch(error){
  // Read only bounded state of THIS original command. No cause/SQL/credential
  // values are exposed, no retry or fresh effect is attempted.
  try{facts.failedWarmTransfer=(await query(`SELECT phase,scope,stage_json IS NULL AS no_stage,publication_json IS NULL AS no_publication,engine_instance_id::text=$4 AS same_engine,boot_id=$5 AS same_boot,home_id=$6 AS same_home,request_json->>'request_nonce'=$7 AS same_nonce,clock_timestamp()<original_end AS before_original_deadline FROM ${S}.managed_storage_transfers WHERE authority_id=$1 AND account_id=$2 AND command_key=$3`,[authority,account,request.body.command_key,request.body.engine_instance_id,request.body.boot_id,request.body.home_id,request.body.request_nonce])).rows.slice(0,2);}catch{facts.failedWarmDiagnosticQuery=true;}
  throw error;
 }
 phase='validate-warm-receipt-and-counters';assert.equal(ack.body.error,null);const result=ack.body.result;
 assert.equal(result.scope,'image-preseed-reuse-only');assert.equal(result.state,'published');
 assert.equal(result.workspace_id,workspace.workspaceId);assert.equal(result.project_id,workspace.projectId);
 assert.notEqual(result.transfer_id,original.transfer_id);assert.equal(result.source_transfer_id,original.transfer_id);
 assert.deepEqual(result.installer_input,graph.image.installer);
 assert.deepEqual(result.manifest_publication,original.publication_json.manifest);assert.deepEqual(result.publication,original.publication_json.installer);
 assert.equal(backend.stats.put,before.put,'warm path must not PUT an object, staging lease or root');
 assert.equal(backend.stats.delete,before.delete);assert.equal(backend.stats.bytes,before.bytes);
 assert.equal(backend.stats.get,before.get+2,'exactly two small live root-receipt GETs');assert.deepEqual(bodyReads(),payloadBefore,'zero CAS payload body GETs/digest rereads');
 facts.warmCounters={get:backend.stats.get-before.get,put:backend.stats.put-before.put,delete:backend.stats.delete-before.delete,bytes:backend.stats.bytes-before.bytes};facts.warmPayloadCountersUnchanged=true;
 phase='validate-new-transfer-row';
 const transfer=(await query(`SELECT * FROM ${S}.managed_storage_transfers WHERE authority_id=$1 AND account_id=$2 AND transfer_id=$3`,[authority,account,result.transfer_id])).rows[0];
 assert.equal(transfer.phase,'published');assert.equal(transfer.stage_json,null);assert.equal(transfer.objects_reserved,0);assert.equal(Number(transfer.bytes_reserved),0);
 assert.equal(transfer.publication_json.reused_from,original.transfer_id);assert.equal(transfer.request_json.request_nonce,request.body.request_nonce);
 assert.equal((await query(`SELECT count(*)::text AS n FROM ${S}.managed_storage_effects WHERE authority_id=$1 AND account_id=$2 AND transfer_id=$3`,[authority,account,result.transfer_id])).rows[0].n,'0');
 phase='original-key-replay';
 const stable={...backend.stats};assert.deepEqual(await perform(request),ack);assert.deepEqual(backend.stats,stable,'same-key original ACK replay dispatches no effects or S3 reads');
 assert.equal(JSON.stringify((await query(`SELECT * FROM ${S}.managed_image_publications WHERE authority_id=$1 AND account_id=$2 AND image=$3`,[authority,account,graph.policy.image])).rows[0]),originalJson,'original publication/ACK remains unchanged');
 // Authenticated wrong policy/root and forged tenant fail BEFORE S3.
 for(const field of ['manifest','installer_input','image_receipt_sha256']){
  phase='reject-wrong-'+field;const op=structuredClone(begin());if(field==='image_receipt_sha256')op[field]='f'.repeat(64);else op[field].digest='f'.repeat(64);
  const old={...backend.stats};const rejected=await perform(operation(op,'reuse-rejected-'+field,await budget(300000)));assert.notEqual(rejected.body.error,null);assert.deepEqual(backend.stats,old);
 }
 phase='reject-forged-tenant';
 const forged=structuredClone(request);forged.body.account_id+=':foreign';const old={...backend.stats};await assert.rejects(perform(forged));assert.deepEqual(backend.stats,old);
 // Fixture operator freeze is NOT a production permission path.
 phase='reject-frozen-storage';
 await query(`UPDATE ${S}.managed_storage_installation SET state='frozen' WHERE authority_id=$1`,[authority]);
 try {const old={...backend.stats};const rejected=await perform(operation(begin(),'reuse-frozen-storage',await budget(300000)));assert.notEqual(rejected.body.error,null);assert.deepEqual(backend.stats,old);}
 finally {await query(`UPDATE ${S}.managed_storage_installation SET state='ready' WHERE authority_id=$1`,[authority]);}
 // Last negative: actual receipt disappearance. Retained failed preparation is
 // intentionally not recycled; the original harness owns subsequent teardown.
 phase='load-original-installer-pin-for-fault';
 const prefix='fabric/v1/accounts/'+createHash('sha256').update(account).digest('hex');
 const key=prefix+'/roots/'+original.publication_json.installer.root_id+'.json';
 const saved=await backend.storage.get(key,4096);assert(saved);await backend.storage.delete(key);
 try {phase='reject-missing-installer-pin';const old={...backend.stats};
  // #preparation deliberately quarantines failed/expired reads and propagates
  // effect_unknown. A signed rejection ACK would misstate that uncertainty.
  await assert.rejects(perform(operation(begin(),'reuse-missing-installer-pin',await budget(300000))),{code:'effect_unknown'});
  assert.equal(backend.stats.put,old.put);assert.deepEqual(bodyReads(),payloadBefore);
  const rejected=(await query(`SELECT t.phase,t.publication_json,t.stage_json,o.state AS operation_state,o.ack_json FROM ${S}.managed_storage_transfers t JOIN ${S}.managed_home_engine_operations o ON o.authority_id=t.authority_id AND o.account_id=t.account_id AND o.home_id=t.home_id AND o.command_key=t.command_key WHERE t.authority_id=$1 AND t.account_id=$2 AND t.command_key=$3`,[authority,account,'reuse-missing-installer-pin'])).rows;
  assert.equal(rejected.length,1);assert.equal(rejected[0].phase,'reserved');assert.equal(rejected[0].publication_json,null);assert.equal(rejected[0].stage_json,null);assert.equal(rejected[0].operation_state,'started');assert.equal(rejected[0].ack_json,null);
  facts.missingPinRetainsUncertainty=true;facts.missingPinNoPublication=true;facts.missingPinNoAck=true;
 }
 finally {assert.equal(await backend.storage.putIfAbsent(key,saved.body),true,'restore ONLY the exact original fixture receipt bytes');}
 phase='complete';
 return {scope:'real-PG-account-image-reuse-component-only',actualPostgres:true,actualFabricCas:true,actualS3:false,physicalNative:false,realRootRegistration:false,fixtureOperatorEnrollment:true,
  sameAccount:true,differentPreparedProject:true,newVmPhysicallyQualified:false,currentOperationAck:true,originalPublicationUnchanged:true,casPayloadBodyReads:0,warmObjectPuts:0,warmRootReceiptGets:2,diagnosticPreflightRootReceiptGets:facts.rootRepresentation.preflightRootReceiptGets,missingPinRetainsUncertainty:true,missingPinNoPublication:true,missingPinNoAck:true,
  immutableOriginalReplay:true,wrongPolicyRefused:true,forgedTenantRefused:true,frozenStorageRefused:true,missingPinRefused:true,normalRootRetirement:false,productionEnablement:false,models:false};
}
