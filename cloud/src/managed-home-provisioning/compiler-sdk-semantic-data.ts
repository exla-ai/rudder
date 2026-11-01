/** DATA parity with engine::compiler_sdk_semantics. Deterministic metadata
 * refs are not original membership or SDK authority. Root's genuine retained
 * owner derives members; its authenticated proof binds them to the catalogue. */
import {validateFabricObjectRef,type FabricObjectRef} from '../fabric-cas.js';
import {check,exact,canonical,hash,snapshot} from './types.js';
export interface CompilerSdkSemanticData {format_version:1;prepared:FabricObjectRef;buck2:FabricObjectRef;buck2_logical_size:number;buck2_mode:number;directory:FabricObjectRef;}
const reference=(kind:FabricObjectRef['kind'],value:unknown):FabricObjectRef=>{const raw=canonical(value);return{kind,algorithm:'sha256',digest:hash(raw),size_bytes:raw.length};};
export function compilerSdkSemanticMembersData(preparedInput:unknown,buck2Input:unknown,size:number,mode:number):CompilerSdkSemanticData {
 const prepared=validateFabricObjectRef(preparedInput),buck2=validateFabricObjectRef(buck2Input);
 check(prepared.kind==='directory'&&prepared.size_bytes>0&&prepared.size_bytes<=4096);
 check((buck2.kind==='blob'||buck2.kind==='file')&&Number.isSafeInteger(size)&&size>0&&size<=2147483648&&Number.isInteger(mode)&&mode>=0&&mode<=0o777&&(mode&0o111)!==0);
 check(buck2.kind==='blob'?size===buck2.size_bytes:buck2.size_bytes>0&&buck2.size_bytes<=65536);
 const entries=[{name_hex:'6275636b32',node:{type:'file',object:buck2,logical_size:size,mode}},{name_hex:'7072657061726564',node:{type:'directory',object:prepared}}];
 const summary={entry_count:2,subdirectory_count:1,entries_bytes:entries.reduce((n,e)=>n+canonical(e).length,0)};
 const shard=reference('directory_shard',{format_version:1,prefix_hex:'',summary,node:{type:'leaf',entries}});
 const directory=reference('directory',{format_version:2,mode:0o555,entry_count:2,subdirectory_count:1,directory_shard:shard});
 return{format_version:1,prepared,buck2,buck2_logical_size:size,buck2_mode:mode,directory};
}
export function validateCompilerSdkSemanticData(input:unknown):CompilerSdkSemanticData {
 exact(input,['format_version','prepared','buck2','buck2_logical_size','buck2_mode','directory']);const data=snapshot(input);check(Object.keys(data).length===6&&data.format_version===1&&typeof data.buck2_logical_size==='number'&&typeof data.buck2_mode==='number');
 const expected=compilerSdkSemanticMembersData(data.prepared,data.buck2,data.buck2_logical_size,data.buck2_mode);check(canonical(expected).equals(canonical(data)),'identity_changed');return expected;
}
