// ROOT-RUN ONLY hash-bound real-PG fixture. Tiny synthetic image and memory S3,
// fixture Root/provider/clock; never physical native/S3/300s production evidence.
import assert from 'node:assert/strict';import {randomUUID} from 'node:crypto';import {createRequire} from 'node:module';
import {createManagedImageBatchMigrator} from '../cloud/dist/managed-home-provisioning/index.js';
import {canonical,hash} from '../cloud/dist/managed-home-provisioning/types.js';
import {prepareFixturePolicy as ownerFixture} from './managed-owner-renewal-pg-followup.mjs';
const S='rudder_project_authority',require=createRequire(new URL('../cloud/package.json',import.meta.url)),pg=require('pg');
let authority,dbConfig,evidence;
const key=r=>`${r.kind}/${r.algorithm}/${r.digest}/${r.size_bytes}`;
function header(transfer,nonce,ordinal,items){return {format_version:1,transfer_id:transfer,request_nonce:nonce,batch_id:randomUUID(),ordinal,objects:items.map(i=>i.ref).sort((a,b)=>Buffer.compare(Buffer.from(key(a)),Buffer.from(key(b)))),total_bytes:items.reduce((n,i)=>n+i.ref.size_bytes,0)};}
export async function prepareFixturePolicy(input){authority=input.authorityId;dbConfig=input.config;const previous=await ownerFixture(input),policy=previous.policy,receipt=hash('SYNTHETIC cold batch PG fixture qualification ONLY');policy.imagePreseed.batchTransport='batch-v1';policy.imagePreseed.batchReceiptSha256=receipt;
 return {policy,async installAfterMigrations(){await previous.installAfterMigrations();const m=createManagedImageBatchMigrator(dbConfig('postgres'),receipt);try{assert.equal((await m.migrate(['rudder_managed_runtime'])).status,'committed');}finally{await m.close();}
  const row=(await input.query(`SELECT state FROM ${S}.managed_image_batch_installation WHERE authority_id=$1`,[authority])).rows[0];assert.equal(row.state,'frozen');
  // Explicit fixture administrator. No production activation API is added.
 }};
}
export async function qualifyColdImage({account,active,concurrent,operation,perform,budget,query,backend,graph,batch}){
 assert.equal(backend.evidence.actualS3,false);assert(backend.stats);const original=await budget(300000);
 const begin=(workspace,key)=>operation({op:'BeginImagePreseedV2',workspace_id:workspace,manifest:graph.image.manifest,installer_input:graph.image.installer,image_receipt_sha256:graph.policy.runtimeImageReceiptSha256},key,original);
 const frozen=await perform(begin(active.workspaceId,'batch-frozen'));assert.equal(frozen.body.error,'unconfigured');assert.equal(Number((await query(`SELECT count(*) AS n FROM ${S}.managed_image_batches`)).rows[0].n),0);
 await query(`UPDATE ${S}.managed_image_batch_installation SET state='ready' WHERE authority_id=$1`,[authority]);
 const start=begin(active.workspaceId,'batch-cold-begin'),ack=await perform(start);assert.equal(ack.body.error,null);assert.equal(ack.body.result.scope,'image-preseed-batch-transfer-only');const transfer=ack.body.result.transfer_id;
 const ordered=[...graph.image.objects].sort((a,b)=>Buffer.compare(Buffer.from(key(a.ref)),Buffer.from(key(b.ref))));let ordinal=0,offset=0,lostAckSeen=false;
 while(offset<ordered.length){const items=ordered.slice(offset,offset+2),h=header(transfer,start.body.request_nonce,ordinal++,items);let response;
  if(offset===0){const lost=await batch.lostCommit('ack',async()=>{response=await batch.put(transfer,h,items);assert.notEqual(response.status,200);});assert.equal(lost.injected,true);const inspect=await batch.reconcile(transfer,h);assert.equal(inspect.status,200);assert.equal(inspect.body.scope,'stored-original-image-batch-ack-data-only');response={status:200,body:inspect.body.ack};lostAckSeen=true;}
  else response=await batch.put(transfer,h,items);
  assert.equal(response.status,200);assert.equal(response.body.header_sha256,hash(canonical(h,262144)));assert.equal(response.body.object_count,items.length);
  const replay=await batch.reconcile(transfer,h);assert.equal(replay.status,200);assert.deepEqual(replay.body.ack,response.body);offset+=items.length;
 }
 const rows=(await query(`SELECT ordinal,state,ack_json FROM ${S}.managed_image_batches WHERE authority_id=$1 AND account_id=$2 AND transfer_id=$3 ORDER BY ordinal`,[authority,account,transfer])).rows;assert.equal(rows.length,ordinal);assert(rows.every(r=>r.state==='replied'&&r.ack_json));
 const quota=(await query(`SELECT objects_reserved,bytes_reserved FROM ${S}.managed_storage_transfers WHERE authority_id=$1 AND account_id=$2 AND transfer_id=$3`,[authority,account,transfer])).rows[0];assert.equal(Number(quota.objects_reserved),ordered.length);assert.equal(Number(quota.bytes_reserved),ordered.reduce((n,i)=>n+i.ref.size_bytes,0));
 // A separate actual prepared workspace remains unassigned. Unknown Started
 // COMMIT cannot invoke even an idempotent S3 PUT or fabricate completion.
 const failedBegin=begin(concurrent.workspaceId,'batch-unknown-begin'),failed=await perform(failedBegin);assert.equal(failed.body.error,null);const failedTransfer=failed.body.result.transfer_id,h=header(failedTransfer,failedBegin.body.request_nonce,0,[ordered[0]]),before=backend.stats.put;
 const lost=await batch.lostCommit('started',async()=>{assert.notEqual((await batch.put(failedTransfer,h,[ordered[0]])).status,200);});assert(lost.injected);assert.equal(backend.stats.put,before);
 const unknown=await batch.reconcile(failedTransfer,h);assert.equal(unknown.status,200);assert.equal(unknown.body.scope,'image-batch-unknown-data-only');assert.equal(unknown.body.actual_known_objects,0);assert.equal(unknown.body.actual_unsettled_objects,0);
 const prior=(await query(`SELECT state,ack_json FROM ${S}.managed_image_batches WHERE transfer_id=$1`,[failedTransfer])).rows[0];assert.equal(prior.state,'started');assert.equal(prior.ack_json,null);
 // A separately captured short ORIGINAL budget qualifies the expiry boundary,
 // not whole-image throughput. Historical ACK DATA cannot rebase this clock.
 const short=await budget(2000),shortBegin=operation({op:'BeginImagePreseedV2',workspace_id:concurrent.workspaceId,manifest:graph.image.manifest,installer_input:graph.image.installer,image_receipt_sha256:graph.policy.runtimeImageReceiptSha256},'batch-short-begin',short),shortAck=await perform(shortBegin);assert.equal(shortAck.body.error,null);
 const shortTransfer=shortAck.body.result.transfer_id,shortHeader=header(shortTransfer,shortBegin.body.request_nonce,0,[ordered[0]]),shortPut=await batch.put(shortTransfer,shortHeader,[ordered[0]]);assert.equal(shortPut.status,200);
 const latePublish=operation({op:'PublishImagePreseedV1',workspace_id:concurrent.workspaceId,transfer_id:shortTransfer},'batch-short-publish',short),lateHeader=header(shortTransfer,shortBegin.body.request_nonce,1,[ordered.at(-1)]);
 await new Promise(resolve=>setTimeout(resolve,2100));const lateBefore=backend.stats.put;assert.notEqual((await batch.put(shortTransfer,lateHeader,[ordered.at(-1)])).status,200);assert.equal(backend.stats.put,lateBefore);
 const historical=await batch.reconcile(shortTransfer,shortHeader);assert.equal(historical.status,200);assert.deepEqual(historical.body.ack,shortPut.body);assert.equal((await perform(latePublish)).body.error,'expired');
 const published=await perform(operation({op:'PublishImagePreseedV1',workspace_id:active.workspaceId,transfer_id:transfer},'batch-image-publish',original));assert.equal(published.body.error,null);assert.deepEqual(published.body.result.installer_input,graph.image.installer);assert.equal(published.body.result.scope,'image-preseed-publication-only');
 const raw=new pg.Client({...dbConfig('rudder_managed_runtime').connection,ssl:{rejectUnauthorized:true,servername:'localhost',ca:dbConfig('rudder_managed_runtime').connection.ca}});raw.on('error',()=>{});await raw.connect();
 try{assert.equal((await raw.query(`SELECT count(*)::text AS n FROM ${S}.managed_image_batches`)).rows[0].n,'0');for(const sql of [`UPDATE ${S}.managed_image_batch_installation SET state='ready'`,`UPDATE ${S}.authority_credentials SET expires_at=clock_timestamp()+interval '1 day'`,`UPDATE ${S}.engine_instances SET revoked_at=NULL`,`DELETE FROM ${S}.managed_image_batches`])await assert.rejects(raw.query(sql),{code:'42501'});}finally{await raw.end();}
 evidence={scope:'real-PG-cold-image-batch-component-only',actualPostgres:true,frozenMigration:true,oneStartedAndAckPerBatch:true,storedAckReconciliation:lostAckSeen,unknownStartedNoDispatch:true,expiredOriginalRefused:true,historicalAckNoClockRenewal:true,batchPublication:true,runtimeRlsAndRestrictedGrants:true,actualS3:false,physicalRoot:false,physicalNative:false,productionEnablement:false,models:false,wholeImage300sQualified:false,fixtureRoot:true,fixtureProvider:true,fixtureClock:true,batches:rows.length,uniqueObjects:ordered.length,uniqueBytes:Number(quota.bytes_reserved),transfer_id:transfer};return evidence;
}
export async function qualify(){assert(evidence);return evidence;}
