// Same-capture operator DATA. No provider/owner/role constructor.
import {createHash} from 'node:crypto';
export function need(v){if(!v)throw Error('STAGING_HOST_DATA_REFUSED');}
export function exact(v,keys){need(v!==null&&typeof v==='object'&&!Array.isArray(v)&&Object.getPrototypeOf(v)===Object.prototype&&Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k)));}
export function sha(v){need(typeof v==='string'&&/^[a-f0-9]{64}$/.test(v));}
export function uuid(v){need(typeof v==='string'&&/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(v));}
export function canonical(v){if(Array.isArray(v))return '['+v.map(canonical).join(',')+']';if(v&&typeof v==='object')return '{'+Object.keys(v).sort().map(k=>JSON.stringify(k)+':'+canonical(v[k])).join(',')+'}';return JSON.stringify(v);}
export const hash=v=>createHash('sha256').update(canonical(v)+'\n').digest('hex');
export function validatePositive(v){
 need(v&&typeof v==='object');const keys=['openCommandKey','startCommandKey','model','profileId','text'];exact(v,Object.hasOwn(v,'stopCommandKey')?[...keys,'stopCommandKey']:keys);
 const commands=[v.openCommandKey,v.startCommandKey,...(Object.hasOwn(v,'stopCommandKey')?[v.stopCommandKey]:[])];need(new Set(commands).size===commands.length);
 for(const k of commands)need(typeof k==='string'&&/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(k));
 for(const k of ['model','profileId'])need(typeof v[k]==='string'&&v[k].trim().length>0&&Buffer.byteLength(v[k])<=256&&!/[\x00-\x1f\x7f]/.test(v[k]));
 need(typeof v.text==='string'&&v.text.trim().length>0&&Buffer.byteLength(v.text)<=24*1024&&!v.text.includes('\0'));return structuredClone(v);
}
export const validateGenuine=validatePositive; // Only the derived positive binder imports this compatibility name.
const paths='bundle_directory fresh_volume_receipt_path controller_source_path control_output_path control_build_receipt_path cleanup_output_path credential_store_path binder_completion_path boot_binding_path source_directory'.split(' ');
export function validateLifecycle(b){
 exact(b,['formatVersion','capture','controller','worker_image','positive_ingress','control_app_id','controller_source_sha256','binder_source_sha256','inputs',...paths]);need(b.formatVersion===1);validatePositive(b.positive_ingress);
 exact(b.capture,['sourceRevision','sourceArchiveSha256','generatedJsSha256']);need(/^[a-f0-9]{40}$/.test(b.capture.sourceRevision));sha(b.capture.sourceArchiveSha256);sha(b.capture.generatedJsSha256);
 need(/^registry.fly.io\/rudder-workers-exla@sha256:[a-f0-9]{64}$/.test(b.worker_image));
 for(const k of paths)need(typeof b[k]==='string'&&b[k].startsWith('/')&&!b[k].split('/').some(v=>v==='..'||v==='.')&&!b[k].includes('\0'));
 need(new Set(paths.map(k=>b[k])).size===paths.length);sha(b.controller_source_sha256);sha(b.binder_source_sha256);
 exact(b.inputs,['workerBuildReceipt','controllerBuildReceipt','volumeReceipt','pgEvidence','s3Evidence',...(Object.hasOwn(b.inputs,'positiveProfileSnapshot')?['positiveProfileSnapshot']:[])]);for(const r of Object.values(b.inputs)){exact(r,['path','sha256']);sha(r.sha256);}
 need(b.fresh_volume_receipt_path===b.inputs.volumeReceipt.path&&b.control_build_receipt_path===b.inputs.controllerBuildReceipt.path);
 exact(b.controller,['formatVersion','kind','sourceRevision','sourceArchiveSha256','generatedJsSha256','contextManifestSha256','image','digest','originalBuild','receiptSha256']);
 need(b.controller.formatVersion===1&&b.controller.kind==='SAME_CAPTURE_CONTROLLER_BUILD_DATA');sha(b.controller.receiptSha256);sha(b.controller.contextManifestSha256);need(/^registry.fly.io\/rudder-workers-exla:[A-Za-z0-9_.-]+$/.test(b.controller.image)&&/^sha256:[a-f0-9]{64}$/.test(b.controller.digest));
 const o=b.controller.originalBuild;exact(o,['actualWait','exitCode','pidfdHeld','pipeEofClaimed','logSha256']);need(o.actualWait===true&&o.exitCode===0&&o.pidfdHeld===true&&o.pipeEofClaimed===false);sha(o.logSha256);
 need(typeof b.control_app_id==='string'&&/^[A-Za-z0-9_-]{1,128}$/.test(b.control_app_id));for(const k of Object.keys(b.capture))need(b.controller[k]===b.capture[k]);return b;
}
export function validateBoot(v,b){exact(v,['bundle_directory','original_boot_nonce','root_observed_original_ready_identity_sha256']);need(v.bundle_directory===b.bundle_directory);uuid(v.original_boot_nonce);sha(v.root_observed_original_ready_identity_sha256);return v;}
function parseInternal(raw,reportNumbers=false){need(typeof raw==='string'&&Buffer.byteLength(raw)>0&&Buffer.byteLength(raw)<=4194304);let i=0,nodes=0;const ws=()=>{while(i<raw.length&&[' ','\t','\r','\n'].includes(raw[i]))i++;};function val(depth){need(depth<=32&&++nodes<=131072);ws();const c=raw[i];if(c==='{'){i++;const out={};ws();if(raw[i]==='}'){i++;return out;}while(true){ws();need(raw[i]==='"');const k=str();need(!Object.hasOwn(out,k)&&k!=='__proto__'&&k!=='constructor'&&k!=='prototype');ws();need(raw[i++]===':');out[k]=val(depth+1);ws();const end=raw[i++];if(end==='}')return out;need(end===',');}}if(c==='['){i++;const out=[];ws();if(raw[i]===']'){i++;return out;}while(true){out.push(val(depth+1));ws();const end=raw[i++];if(end===']')return out;need(end===',');}}if(c==='"')return str();for(const [lit,v]of [['true',true],['false',false],['null',null]])if(raw.startsWith(lit,i)){i+=lit.length;return v;}const m=(reportNumbers?/-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/ : /-?(?:0|[1-9][0-9]*)/).exec(raw.slice(i));need(m&&m.index===0);i+=m[0].length;const v=Number(m[0]);need(reportNumbers?Number.isFinite(v):Number.isSafeInteger(v));return v;}function str(){const start=i++;while(i<raw.length){const c=raw[i++];if(c==='"'){const v=JSON.parse(raw.slice(start,i));need(!/[\uD800-\uDFFF]/u.test(v));return v;}if(c==='\\')i++;}need(false);}const result=val(0);ws();need(i===raw.length);return result;}
export function parseStrict(raw){try{return parseInternal(raw);}catch{throw Error('GENUINE_LIFECYCLE_JSON_REFUSED');}}

// Physical observation reports include fractional timing measurements. Protocol
// receipts still use parseStrict and retain their integer-only number rule.
export function parseReportStrict(raw){try{return parseInternal(raw,true);}catch{throw Error('STAGING_REPORT_JSON_REFUSED');}}
