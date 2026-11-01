import {validateFabricObjectRef,type FabricObjectRef} from "../fabric-cas.js";
import {check,exact,key,uuid,sha} from "./types.js";
export type StorageOperation=
 | {op:"BeginFreshRootsV1";workspace_id:string;prepare_id:string;export_id:string;head:string;source:FabricObjectRef;environment:FabricObjectRef}
 | {op:"PublishFreshRootsV1";workspace_id:string;transfer_id:string}
 | {op:"AcquireProviderFreshOwnerV1";workspace_id:string;transfer_id:string;acquire_key:string;expected_owner_epoch:"1"}
 | {op:"AcquireFreshOwnerV1";workspace_id:string;transfer_id:string;acquire_key:string;expected_owner_epoch:"1"}
 | {op:"BeginImagePreseedV1";workspace_id:string;manifest:FabricObjectRef;installer_input:FabricObjectRef;image_receipt_sha256:string}
 | {op:"BeginImagePreseedV2";workspace_id:string;manifest:FabricObjectRef;installer_input:FabricObjectRef;image_receipt_sha256:string}
 | {op:"PublishImagePreseedV1";workspace_id:string;transfer_id:string};
const names=["BeginFreshRootsV1","PublishFreshRootsV1","AcquireFreshOwnerV1","AcquireProviderFreshOwnerV1","BeginImagePreseedV1","BeginImagePreseedV2","PublishImagePreseedV1"];
export const isStorageOperation=(v:{op:string}):v is StorageOperation=>names.includes(v.op);
export const isImageOperation=(v:{op:string})=>v.op==="BeginImagePreseedV1"||v.op==="BeginImagePreseedV2"||v.op==="PublishImagePreseedV1";
export const referenceKey=(r:FabricObjectRef)=>`${r.kind}:${r.algorithm}:${r.digest}:${r.size_bytes}`;
export function storageReference(value:unknown):FabricObjectRef{const r=validateFabricObjectRef(value);check(["blob","file","directory","directory_shard","source_root","environment_root"].includes(r.kind));return r;}
export function validateStorageOperation(value:unknown):StorageOperation{
 check(value!==null&&typeof value==="object");const v=value as StorageOperation;key(v.workspace_id);check(isStorageOperation(v));
 switch(v.op){case "BeginFreshRootsV1":exact(v,["op","workspace_id","prepare_id","export_id","head","source","environment"]);uuid(v.prepare_id);uuid(v.export_id);check(typeof v.head==="string"&&/^[a-f0-9]{40,64}$/.test(v.head));check(storageReference(v.source).kind==="source_root"&&storageReference(v.environment).kind==="environment_root");break;
  case "BeginImagePreseedV1":case "BeginImagePreseedV2":exact(v,["op","workspace_id","manifest","installer_input","image_receipt_sha256"]);check(storageReference(v.manifest).kind==="blob"&&v.manifest.size_bytes<=32*1024*1024&&storageReference(v.installer_input).kind==="directory");sha(v.image_receipt_sha256);break;
  case "AcquireFreshOwnerV1":case "AcquireProviderFreshOwnerV1":exact(v,["op","workspace_id","transfer_id","acquire_key","expected_owner_epoch"]);uuid(v.transfer_id);key(v.acquire_key);check(v.expected_owner_epoch==="1");break;
  default:exact(v,["op","workspace_id","transfer_id"]);uuid(v.transfer_id);
 }
 return v;
}
