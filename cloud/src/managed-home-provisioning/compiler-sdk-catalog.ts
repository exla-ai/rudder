/** Fixed immutable SDK construction DATA. This never issues a role, purpose,
 * mount, preparation or execution owner. Default profiles remain absent. */
import type {FabricObjectRef} from "../fabric-cas.js";
import {check,canonical,snapshot} from "./types.js";
export interface CompilerSdkCatalogData {format_version:1;profile_id:string;artifact_image:string;cas_image:string;
 artifact_manifest:FabricObjectRef;inventory:FabricObjectRef;manifest:FabricObjectRef;installer_input:FabricObjectRef;source_binding_sha256:string;}
const ref=(kind:"blob"|"directory",digest:string,size_bytes:number):FabricObjectRef=>({kind,algorithm:"sha256",digest,size_bytes});
export const COMPILER_ARTIFACT_MANIFEST="add56ea6ade5411145b026094f63d8d5838e38b58d397d09749027090d346d0c";
export const A2_COMPILER_SDK:CompilerSdkCatalogData={format_version:1,profile_id:"rudder-linux-rust-typescript-v1",
 artifact_image:"registry.fly.io/rudder-workers-exla@sha256:88d25bcd2d359fa6c829a9126d96011b17d197b23307f997e2406e00bc5d562f",
 cas_image:"registry.fly.io/rudder-workers-exla@sha256:fdb88bde30c78585e9c932c434bf8ed5c82f47ceac41c0ff23fbe9962147860f",
 artifact_manifest:ref("blob",COMPILER_ARTIFACT_MANIFEST,190431),inventory:ref("blob","e2d9b70e1f90e56ec69cf0dd7cfd6f5ba75e0d7a71af9fd214c1369ab906ad9e",176107),
 manifest:ref("blob","914d7fa727db69b40b30060ab92216e0bebf02aa032c21c39633ae0acee708ee",315690),
 installer_input:ref("directory","ba237b8a5cbf5b20d9a0c479d82624e2a98e012e7291c32b631425185ce3ef0b",230),
 source_binding_sha256:"b8469769a78dd68ce7b730a2dc865b13c22d63e538b0166ba65b094a279bbc4c"};
export function validateCompilerSdkCatalog(input:unknown):CompilerSdkCatalogData {
 const data=snapshot(input)as CompilerSdkCatalogData;const known=PROFILES.find(p=>p.catalog.profile_id===data?.profile_id);check(known&&canonical(data).equals(canonical(known.catalog)),"identity_changed");return data;
}

/** Actual A6 construction + separate CAS DATA receipts; not an SDK role. */
export const C1_COMPILER_SDK:CompilerSdkCatalogData={format_version:1,profile_id:"rudder-linux-rust-cxx-typescript-c1",
 artifact_image:"registry.fly.io/rudder-workers-exla@sha256:469e5b3667cd673bab2674fcb30021f2c731555a8cc3326519dbb7c4cab73273",
 cas_image:"registry.fly.io/rudder-workers-exla@sha256:19ce471b0c894dcd820135d1d97d2a1f5a355938cd339acedd8373efa86e070e",
 artifact_manifest:ref("blob","bb70f8795da717a73f9d9ded535149f1fb4abff84374b12188361f1cc4a8106a",1065125),
 inventory:ref("blob","4f8d57a0ffd78335a9ca9863d26040379d65dc8942fa32d41e217f898612f832",850145),
 manifest:ref("blob","d60afb84b6e373fece5fd2a0babb1b66bd0f4c637efddf766f59a180e39ec54a",1830173),
 installer_input:ref("directory","7deff164fb058949352597c656660878bbcb8d19d2ef9b6a6c23bba9bbf261e5",230),
 source_binding_sha256:"301ec13415fa57e2623571adce9327b4e406d558698a1fa46adfb7e04f5fbbc0"};
export interface CompilerSdkToolSpecData {name:string;relative_path:string;compiler_entrypoint?:string;}
export interface CompilerSdkProfileData {catalog:CompilerSdkCatalogData;artifact_directory:string;cas_directory:string;
 source_mount:string;guest_mount:string;tools:readonly CompilerSdkToolSpecData[];}
const A2_TOOLS:CompilerSdkToolSpecData[]=[{name:"buck2",relative_path:"buck2"},{name:"rustc",relative_path:"prepared/tools/rust/bin/rustc"},
 {name:"node",relative_path:"prepared/tools/node"},{name:"typescript",relative_path:"prepared/tools/node",compiler_entrypoint:"prepared/tools/typescript/lib/tsc.js"},{name:"python",relative_path:"prepared/tools/python/bin/python3"}];
const C1_TOOLS:CompilerSdkToolSpecData[]=[...A2_TOOLS,{name:"cargo",relative_path:"prepared/tools/rust/bin/cargo"},
 {name:"cc",relative_path:"prepared/usr/bin/cc"},{name:"cxx",relative_path:"prepared/usr/bin/c++"},{name:"ar",relative_path:"prepared/usr/bin/ar"}];
function freeze<T>(value:T):T {if(value&&typeof value==="object"){for(const child of Object.values(value))freeze(child);Object.freeze(value);}return value;}
const PROFILES:readonly CompilerSdkProfileData[]=freeze([
 {catalog:A2_COMPILER_SDK,artifact_directory:"/rudder-compiler-v1",cas_directory:"/var/lib/rudder-compiler-cas-v1",source_mount:"/opt/rudder-worker/compiler-sdk-v1",guest_mount:"/opt/runtime/sdk/v1",tools:A2_TOOLS},
 {catalog:C1_COMPILER_SDK,artifact_directory:"/rudder-compiler-c1",cas_directory:"/var/lib/rudder-compiler-c1-cas-v1",source_mount:"/opt/rudder-worker/compiler-sdk-v1",guest_mount:"/opt/runtime/sdk/v1",tools:C1_TOOLS},
]);
/** Finite recipe DATA only. There is no path/image/hash override and no role
 * publisher or protected config owner is issued by choosing this descriptor. */
export function compilerSdkProfileData(input:unknown):CompilerSdkProfileData {
 const catalog=validateCompilerSdkCatalog(input);return PROFILES.find(p=>p.catalog.profile_id===catalog.profile_id)!;
}
