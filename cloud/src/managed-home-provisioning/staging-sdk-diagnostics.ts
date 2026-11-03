import {ChildProcess} from "node:child_process";
import {createHash} from "node:crypto";
import {inspect} from "node:util";
import type {OriginalTime} from "./fly.js";
import {check,fail,snapshot} from "./types.js";

const LIMIT=64*1024;
const HELD=new Set<OriginalSdkDiagnosticRead>();
const OPERATIONS=new Set(["SDK private namespace mount failed","SDK retained mount reopen failed","SDK retained floor chdir failed","SDK actual isolated namespaces unavailable","SDK retained workspace chdir failed","SDK user namespace join failed","SDK actual isolated workload spawn failed","SDK actual isolated workload wait failed","SDK actual isolated workload exit failed","SDK actual workload result parse failed"]);
const FIELDS=["kind","component","stage","check","class","io_kind","errno"];
const CHECKS=new Set(("RuntimeMountSdk SdkPrepareOriginal SdkCatalog SdkPlaceholder SdkBackingDescriptors SdkSourceBinding SdkBakedInventory SdkVerifiedManifest SdkSemanticInputs SdkToolchains SdkArtifactBinding SdkStorage SdkReadFloor SdkFuse SdkMountData SdkQualification SdkReady SdkHelperEntry SdkHelperConnect SdkHelperParent SdkHelperParentImage SdkHelperCapsule SdkHelperServiceImage SdkHelperLauncher SdkHelperLauncherImage SdkHelperFloor SdkHelperWorkspace SdkHelperNamespaceWorkload SdkHelperResultSend SdkHelperExitUnknown SdkHelperExitedWithoutResult SdkProcessOriginal SdkProcessSpawn SdkProcessCapture SdkProcessPipes SdkProcessChildExit SdkProcessPeer SdkProcessHelperFloor SdkProcessHelperImage SdkProcessHelperParent SdkProcessCapsule SdkProcessResult SdkProcessWorkload SdkProcessInterpreter SdkProcessNamespaces SdkProcessCapabilities SdkProcessClock SdkProcessExitEof").split(" "));
const STAGES:Record<string,readonly string[]>={root_storage:["entry","capture_core","capture_runtime","serve_current","serve_runtime","send_reply","receive","authenticate","decode","dispatch","encode"],machine_root:["prepare_host","spawn_engine","wait_engine","spawn_service","send_service","wait_client","wait_host_serving","established","quarantined"],compiler_sdk_helper:["handshake"]};
function uniqueJson(text:string):any {
 const parsed=JSON.parse(text),tokens=text.match(/"(?:\\.|[^"\\])*"|[{}\[\]:,]|[^\s{}\[\]:,]+/g)??[],stack:(Set<string>|null)[]=[];
 for(let i=0;i<tokens.length;i++){const token=tokens[i];
  if(token==="{")stack.push(new Set());else if(token==="[")stack.push(null);else if(token==="}"||token==="]")stack.pop();
  else if(token.startsWith('"')&&tokens[i+1]===":"){const keys=stack.at(-1),key=JSON.parse(token);check(keys&&!keys.has(key),"invalid_request");keys.add(key);}
  check(stack.length<=32,"invalid_request");
 }
 return parsed;
}
const IO=new Set(["permission_denied","not_found","timed_out","would_block","interrupted","invalid_data","other"]);
export interface SdkDiagnosticTarget {readonly app:string;readonly machine:string;}
export interface SdkFailureDiagnostic {kind:"rudder-root-retained-failure-v1";component:string;stage:string;check:string|null;class:string;io_kind:string|null;errno:number|null;error_text:string;}
/** Only fixed structured Root diagnostics are exported. Adjacent compiler,
 * provider, config, environment and arbitrary Error text are never relayed. */
export function projectSdkFailureLogs(raw:Buffer,target:SdkDiagnosticTarget):SdkFailureDiagnostic[]{
 check(raw.length<=LIMIT,"invalid_request");const found:SdkFailureDiagnostic[]=[];
 for(const line of raw.toString("utf8").split("\n")){
  if(!line||Buffer.byteLength(line)>8192)continue;
  let row:any;
  try{
   const outer=uniqueJson(line);
   if(!outer||typeof outer!=="object"||outer.instance!==target.machine||typeof outer.message!=="string")continue;
   row=uniqueJson(outer.message);
  }catch{
   // Existing Fly CLI observations use this ANSI-decorated prefix. Strip
   // only colour codes, match the ORIGINAL machine, and parse the entire
   // message as a fixed frame. No arbitrary substring/log echo extraction.
   const plain=line.replace(/\x1b\[[0-9;]*m/g,""),m=/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z (?:app|runner)\[([a-f0-9]{14,16})\] (?:[a-z]{3} )?\[(?:info|error|warn|debug)\]\s*(.*)$/.exec(plain);
   if(!m||m[1]!==target.machine)continue;
   try{row=uniqueJson(m[2]);}catch{continue;}
  }
  if(!row||typeof row!=="object"||Array.isArray(row)||!([FIELDS.length,FIELDS.length+1].includes(Object.keys(row).length))||Object.keys(row).some(k=>!FIELDS.includes(k)&&k!=="error_text")||!FIELDS.every(k=>Object.hasOwn(row,k)))continue;
  if(row.kind!=="rudder-root-retained-failure-v1"||!Object.hasOwn(STAGES,row.component)||!STAGES[row.component].includes(row.stage))continue;
  if(!(row.check===null||typeof row.check==="string"&&CHECKS.has(row.check))||row.class==="panic"&&row.check!==null)continue;
  if(!["io","validation_or_other","panic"].includes(row.class))continue;
  if(row.class==="io"?!(IO.has(row.io_kind)&&(row.errno===null||Number.isInteger(row.errno)&&row.errno>0&&row.errno<=4095)):row.io_kind!==null||row.errno!==null)continue;
  if(Object.hasOwn(row,"error_text")){
   const suffix=row.io_kind===null?"":`: io_kind=${row.io_kind}${row.errno===null?"":` errno=${row.errno}`}`;
   if(typeof row.error_text!=="string"||Buffer.byteLength(row.error_text)>384||![...OPERATIONS].some(op=>row.error_text===op+suffix))continue;
  }
  const detail=`${row.error_text?`${row.error_text}; `:""}${row.component}/${row.stage}: ${row.check??row.class}${row.io_kind===null?"":`; ${row.io_kind}${row.errno===null?"":` (errno ${row.errno})`}`}`;
  found.push({...row,error_text:detail});if(found.length===8)break;
 }
 return found;
}
/** Select only the exact disposable SDK machine retained by this registry's
 * original creation/start path. Inventory alone does not issue an owner. */
export function sdkDiagnosticTarget(custody:any):SdkDiagnosticTarget {
 check(custody?.scope==="retained-disposable-provider-response-data-only"&&custody.original_creation_captured===true&&custody.pending_effect===null,"effect_unknown");
 check(typeof custody.provision_nonce==="string"&&/^[a-f0-9-]{36}$/.test(custody.provision_nonce)&&custody.app_name==="rudder-mh-"+custody.provision_nonce.replaceAll("-",""),"identity_changed");
 const original=custody.responses?.original_responses,machine=original?.configure,start=original?.start;
 check(machine&&start?.originalHttpAck===true&&start.ownedReadback?.id===machine.id&&start.ownedReadback?.instanceId===machine.instanceId&&start.ownedReadback?.configSha256===machine.configSha256&&original?.machine?.id===machine.id,"identity_changed");
 check(typeof machine.id==="string"&&/^[a-f0-9]{14,16}$/.test(machine.id),"identity_changed");
 return Object.freeze({app:custody.app_name,machine:machine.id});
}

/** One original read-only logs child. Stored before spawn; failure/termination
 * never drops/replaces the child or pretends that close means a Root join.
 * The child has at most five seconds, bounded by the SAME stage deadline.
 * Timeout/overflow kills only this local logs CLI, never the SDK machine. */
export class OriginalSdkDiagnosticRead {
 #child?:ChildProcess;#run?:Promise<void>;#resolve?:()=>void;#timer?:NodeJS.Timeout;
 #raw:Buffer[]=[];#bytes=0;#stderrBytes=0;#hash=createHash("sha256");#sha?:string;
 #eof={stdout:false,stderr:false};#exited=false;#closed=false;#exit:number|null=null;#signal:string|null=null;
 #errors:string[]=[];#terminated=false;#finished=false;#constructorCompleted=false;#diagnostics:SdkFailureDiagnostic[]=[];
 constructor(private readonly target:SdkDiagnosticTarget,private readonly time:OriginalTime,private readonly executable:string,
  private readonly token:string,private readonly onFailure:(failure:SdkFailureDiagnostic)=>void=()=>{},private readonly allocate:()=>ChildProcess=()=>new ChildProcess()){
  check(/^\/[^\0\r\n]+$/.test(executable)&&typeof token==="string"&&token.length>0,"unconfigured");
 }
 [inspect.custom](){return "OriginalSdkDiagnosticRead [original child retained; filtered DATA only]";}
 toJSON():never{fail("invalid_request");}
 #error(code:string){if(!this.#errors.includes(code))this.#errors.push(code);}
 #stop(code:string){this.#error(code);if(this.#terminated)return;this.#terminated=true;try{this.#child?.kill("SIGKILL");}catch{this.#error("termination_unknown");}}
 #observe(raw:Buffer){
  if(this.#diagnostics.length)return;
  const found=projectSdkFailureLogs(raw,this.target);
  if(found.length){this.#diagnostics=found;try{this.onFailure(found[0]);}catch{this.#error("failure_callback_error");}}
 }
 #finish(){
  if(this.#finished||!this.#closed||!this.#exited||!this.#eof.stdout||!this.#eof.stderr)return;
  this.#finished=true;if(this.#timer)clearTimeout(this.#timer);
  this.#sha=this.#hash.digest("hex");
  if(this.#exit===0&&!this.#signal&&!this.#errors.length){try{this.time.remaining();this.#observe(Buffer.concat(this.#raw));}catch{this.#error("projection_refused");}}
  this.#raw=[];this.#resolve?.();this.#resolve=undefined;
 }
 startRetaining():Promise<void>{
  check(!this.#run,"effect_unknown");this.#run=new Promise(resolve=>{this.#resolve=resolve;});HELD.add(this);
  try{
   const remaining=Math.min(5000,this.time.remaining());
   const child=this.allocate();this.#child=child;
   child.on("error",()=>{this.#error("original_child_error");});
   child.once("exit",(code,signal)=>{this.#exit=code;this.#signal=signal;this.#exited=Number.isInteger(code)||signal!==null;this.#finish();});
   child.once("close",()=>{this.#closed=true;this.#finish();});
   const env={...process.env,FLY_API_TOKEN:this.token};delete (env as NodeJS.ProcessEnv).FLY_ACCESS_TOKEN;
   const args=[this.executable,"logs","--app",this.target.app,"--machine",this.target.machine,"--no-tail"];
   const spawn=(child as unknown as {spawn(options:unknown):unknown}).spawn;check(typeof spawn==="function","unconfigured");
   this.#timer=setTimeout(()=>this.#stop("original_logs_deadline"),remaining);
   this.time.remaining();spawn.call(child,{file:this.executable,args,stdio:["ignore","pipe","pipe"],envPairs:Object.entries(env).filter((v):v is [string,string]=>typeof v[1]==="string").map(([k,v])=>`${k}=${v}`)});this.#constructorCompleted=true;
   for(const name of ["stdout","stderr"] as const){const stream=child[name];check(stream,"effect_unknown");
    stream.on("data",(part:Buffer)=>{try{if(!Buffer.isBuffer(part)){this.#stop("stream_type");return;}
     if(name==="stdout"){this.#bytes+=part.length;this.#hash.update(part);if(this.#bytes<=LIMIT){this.#raw.push(part);
       const raw=Buffer.concat(this.#raw),last=raw.lastIndexOf(10);
       if(last>=0)this.#observe(raw.subarray(0,last+1));
      }}
     else this.#stderrBytes+=part.length;
     if(this.#bytes+this.#stderrBytes>LIMIT)this.#stop("original_logs_output_bound");
    }catch{this.#stop("original_capture_error");}});
    stream.once("end",()=>{this.#eof[name]=true;this.#finish();});stream.on("error",()=>{this.#stop("original_stream_error");});
   }
  }catch{this.#error("original_start_unknown");if(!this.#child){if(this.#timer)clearTimeout(this.#timer);this.#resolve?.();}}
  return this.#run;
 }
 failures():readonly SdkFailureDiagnostic[]{return snapshot(this.#diagnostics);}
 statusData(){return snapshot({scope:"original-sdk-machine-log-diagnostics-data-only",target:this.target,constructor_completed:this.#constructorCompleted,
  actual_exit_observed:this.#exited,close_observed:this.#closed,stdout_eof:this.#eof.stdout,stderr_eof:this.#eof.stderr,exit_code:this.#exit,exit_signal:this.#signal,
  stdout_bytes:this.#bytes,stderr_bytes:this.#stderrBytes,stdout_sha256:this.#sha??null,errors:this.#errors,failures:this.#diagnostics,owners_retained:true,
  root_helper_join_proved:false,qualification_authority:false});}
}
