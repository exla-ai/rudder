// ROOT-RUN ONLY through the existing hash-bound real-PG followup hook.
// Import happens before componentGraph(), immutable policy or any home intent.
import assert from 'node:assert/strict';
import {selectHardlinkImageFixture} from './managed-storage-pg-fixtures.mjs';
import {prepareFixturePolicy as prepareOwnerClockFixture} from './managed-owner-renewal-pg-followup.mjs';
selectHardlinkImageFixture();
let authority;
export async function prepareFixturePolicy(input){
 authority=input.authorityId;
 // Reuse current fixture-only v4 clock/migration admission. This is not
 // physical clock qualification or a new production credential/context path.
 return prepareOwnerClockFixture(input);
}
export async function qualify({account,active,initialAcquire,query,backend,graph}){
 assert(authority);assert.equal(backend.evidence.actualS3,false,'explicit conditional-memory S3 fixture required');
 assert.equal(typeof backend.controls.objectBodyReads,'function');
 const h=graph.image.hardlinks;assert(h);assert.equal(h.aliases,513);assert.equal(h.content.kind,'blob');assert.equal(h.content.size_bytes,4194304);
 assert(h.logicalAliasBytes>2147483648);assert(h.logicalPathBytes>2147483648);assert(Number.isSafeInteger(h.logicalPathBytes));assert(h.uniqueGraphBytes<2147483648);
 const aliases=graph.image.files.filter(f=>f.content.digest===h.content.digest);
 assert.equal(aliases.length,513);for(const f of aliases){assert.equal(f.size_bytes,4194304);assert.equal(f.sha256,h.content.digest);assert.deepEqual(f.content,h.content);}
 const rows=await query('SELECT p.*,t.phase,t.request_json FROM rudder_project_authority.managed_image_publications p JOIN rudder_project_authority.managed_storage_transfers t USING(authority_id,account_id,transfer_id) WHERE p.authority_id=$1 AND p.account_id=$2 AND p.image=$3',[authority,account,graph.policy.image]);
 assert.equal(rows.rows.length,1);const row=rows.rows[0];assert.equal(row.phase,'published');assert.equal(row.request_json.operation.op,'BeginImagePreseedV1');assert.equal(row.request_json.constraint.maximum_milliseconds,300000);assert(row.request_json.remaining_milliseconds>0&&row.request_json.remaining_milliseconds<=300000);assert.equal(row.manifest_sha256,graph.image.manifest.digest);assert.deepEqual(row.installer_input_json,graph.image.installer);
 const refs=row.objects_json,total=refs.reduce((n,r)=>n+r.size_bytes,0);
 assert(new Set(refs.map(r=>[r.kind,r.algorithm,r.digest,r.size_bytes].join(':'))).size===refs.length);assert(total<=2147483648);assert(refs.length<=100000);
 assert(refs.some(r=>r.kind==='blob'&&r.digest===graph.image.manifest.digest));assert(refs.some(r=>r.digest===h.content.digest));
 assert.deepEqual(row.publication_json.manifest.root,graph.image.manifest);assert.deepEqual(row.publication_json.installer.root,graph.image.installer);
 assert.equal(initialAcquire.body.error,null);assert.equal(initialAcquire.body.result.workspace_id,active.workspaceId);
 const reads=backend.controls.objectBodyReads(account,h.content);assert.equal(reads,1);
 // One successful backend body read is network-reuse evidence. It does not
 // count CPU SHA invocations; the completed digest memo is reviewed in source.
 return {scope:'real-PG-image-alias-publication-component-only',actualPostgres:true,actualFabricCas:true,actualS3:false,fixtureOperatorEnrollment:true,aliases:h.aliases,logicalPathBytes:h.logicalPathBytes,retainedUniqueObjectBytes:total,successfulTargetBodyReads:reads,cpuDigestInvocationCountQualified:false,qualifiedClockContinuity:false,physicalNativeWatchdog:false,actualMixedPgQualification:false,normalRootRetirement:false,physicalRoot:false,productionEnablement:false,models:false};
}
