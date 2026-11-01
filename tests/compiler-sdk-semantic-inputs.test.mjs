// Canonical metadata DATA only; no Root owner, SDK mount or execution ACK.
import test from 'node:test';import assert from 'node:assert/strict';
import {compilerSdkSemanticMembersData,validateCompilerSdkSemanticData} from '../cloud/dist/managed-home-provisioning/compiler-sdk-semantic-data.js';
const prepared={kind:'directory',algorithm:'sha256',digest:'a'.repeat(64),size_bytes:230},buck2={kind:'blob',algorithm:'sha256',digest:'b'.repeat(64),size_bytes:1024};
test('semantic metadata binds exactly prepared plus static frontend, without construction provenance',()=>{
 const data=compilerSdkSemanticMembersData(prepared,buck2,1024,0o555);assert.deepEqual(validateCompilerSdkSemanticData(data),data);assert.equal(data.directory.digest,'de3b8f0c240d6afa3d6c7f5b899f9f901d5c4d03e063ae5851f07985750e96ed');assert.equal(data.directory.size_bytes,229);
 const changed=compilerSdkSemanticMembersData({...prepared,digest:'c'.repeat(64)},buck2,1024,0o555);assert.notDeepEqual(changed.directory,data.directory);
 const changedBuck=compilerSdkSemanticMembersData(prepared,{...buck2,digest:'d'.repeat(64)},1024,0o555);assert.notDeepEqual(changedBuck.directory,data.directory);
 const changedMode=compilerSdkSemanticMembersData(prepared,buck2,1024,0o500);assert.notDeepEqual(changedMode.directory,data.directory);
 for(const field of ['artifact_manifest','manifest','installer_input','source_binding_sha256','oci','config_sha256','native_sha256','registration','qualification'])assert(!Object.hasOwn(data,field));
});
test('wrong subset shapes, stale root and declaration-only provenance are refused as DATA',()=>{
 const data=compilerSdkSemanticMembersData(prepared,buck2,1024,0o555);
 assert.throws(()=>validateCompilerSdkSemanticData({...data,prepared:{...prepared,digest:'e'.repeat(64)}}));
 assert.throws(()=>validateCompilerSdkSemanticData({...data,recipe:'irrelevant-but-not-allowed'}));
 assert.throws(()=>compilerSdkSemanticMembersData({...prepared,kind:'blob'},buck2,1024,0o555));
 assert.throws(()=>compilerSdkSemanticMembersData(prepared,buck2,1025,0o555));
 assert.throws(()=>compilerSdkSemanticMembersData(prepared,buck2,1024,0o444));
});
