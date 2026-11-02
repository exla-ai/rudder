import {ChildProcess} from "node:child_process";
import {createHash} from "node:crypto";
import {MessageChannel} from "node:worker_threads";
import {openSync,fstatSync,readFileSync,constants} from "node:fs";
import {fileURLToPath} from "node:url";
import {inspect} from "node:util";
import {assertStagingGenuineIngressExperiment,type StagingGenuineIngressExperiment} from "./staging-genuine-ingress-experiment.js";
import {check,fail,snapshot} from "./types.js";

const COLLECTOR_EFFECTS_ALLOWED=false;
const WORKER_SHA256="77bfde8dc867b869ac2971f3973c8ec91e7c1f0bfcf7a46420208f79fa011b8a";
const PROJECT_SHA256="df3e53242dfbb0b360caf6b69df363e0467e00fc50ad9f33cf5b893088312d18";
const PREFIX=new URL("./",import.meta.url);
const MAX_LINE=65536,MAX_PROTOCOL=8*1024*1024,MAX_REQUESTS=65536;
const OPS=new Set(["directory_lstat","directory_open","directory_fstat","stdout_open","stderr_open","selector_allocate","logs_spawn","pidfd_open","stdout_nonblocking","stderr_nonblocking","stdout_register","stderr_register","select","stdout_read","stderr_read","stdout_write","stderr_write","child_wait","stdout_fsync","stderr_fsync","directory_fsync"]);
const HELD=new Map<object,OriginalGenuineIngressCollector>();
const ISSUER=Symbol("same original holder collector");
const externalKeys=["feature","app","machine","instance_id","image_digest","integrated_revision","source_manifest_sha256","root_receipt_manifest_sha256","fixture_manifest_sha256","native_elf_sha256","engine_elf_sha256"] as const;
export type GenuineIngressExternalData=Record<typeof externalKeys[number],string>;
const exact=(v:unknown,keys:readonly string[]):v is Record<string,unknown>=>!!v&&typeof v==="object"&&!Array.isArray(v)&&Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
const sha=(v:Buffer|string)=>createHash("sha256").update(v).digest("hex");
const CAPTURE_ERRORS=new Set(["constructor","pidfd","original_pipe_unavailable","original_pipe_observer","original_selector","original_pipe_read","unregister","raw_write_stdout","raw_write_stderr","wait_type","wait_interrupted","fsync_stdout","fsync_stderr","directory_fsync"]);
const closedCapture=["schema","constructor_completed","actual_child_wait","exit_code","pidfd_held","stdout_eof","stderr_eof","errors","expired","overflow","original_budget_requests","original_budget_refused","raw_files_fsynced","raw_directory_fsynced","stdout_bytes","stderr_bytes","stdout_sha256","stderr_sha256","stdout_stored_bytes","stderr_stored_bytes"];
function captureSchema(v:unknown):v is Record<string,unknown>{
 if(!exact(v,closedCapture)||v.schema!==2)return false;
 const booleans=["constructor_completed","actual_child_wait","pidfd_held","stdout_eof","stderr_eof","expired","overflow","original_budget_refused","raw_files_fsynced","raw_directory_fsynced"];
 return booleans.every(k=>typeof v[k]==="boolean")&&(v.exit_code===null||Number.isSafeInteger(v.exit_code))&&Array.isArray(v.errors)&&v.errors.length<=CAPTURE_ERRORS.size&&new Set(v.errors).size===v.errors.length&&v.errors.every(e=>typeof e==="string"&&CAPTURE_ERRORS.has(e))&&Number.isSafeInteger(v.original_budget_requests)&&(v.original_budget_requests as number)>0&&(v.original_budget_requests as number)<=MAX_REQUESTS&&["stdout","stderr"].every(n=>Number.isSafeInteger(v[n+"_bytes"])&&(v[n+"_bytes"] as number)>=0&&Number.isSafeInteger(v[n+"_stored_bytes"])&&(v[n+"_stored_bytes"] as number)>=0&&(v[n+"_stored_bytes"] as number)<=MAX_PROTOCOL&&typeof v[n+"_sha256"]==="string"&&/^[a-f0-9]{64}$/.test(v[n+"_sha256"] as string));
}
function captureValid(v:unknown,requests:number):boolean{
 if(!captureSchema(v))return false;
 return v.schema===2&&v.constructor_completed===true&&v.actual_child_wait===true&&v.exit_code===0&&v.pidfd_held===true&&v.stdout_eof===true&&v.stderr_eof===true&&Array.isArray(v.errors)&&v.errors.length===0&&v.expired===false&&v.overflow===false&&v.original_budget_requests===requests&&requests>0&&v.original_budget_refused===false&&v.raw_files_fsynced===true&&v.raw_directory_fsynced===true&&["stdout","stderr"].every(n=>Number.isSafeInteger(v[n+"_bytes"])&&(v[n+"_bytes"] as number)>=0&&v[n+"_bytes"]===v[n+"_stored_bytes"]&&typeof v[n+"_sha256"]==="string"&&/^[a-f0-9]{64}$/.test(v[n+"_sha256"] as string))&&(v.stdout_bytes as number)+(v.stderr_bytes as number)<=MAX_PROTOCOL&&v.stderr_bytes===0;
}
const FALSE_FLAGS=["source_accepted","runtime_ready","retirement_complete","root_acknowledgment","serving","current_claim","qualified"];
function projectionValid(v:unknown,data:GenuineIngressExternalData,original:StagingGenuineIngressExperiment):boolean{
 const hashes=["rpc_wire_sha256","canonical_command_sha256","root_frame_sha256","native_frame_sha256","whole_stdout_sha256","whole_stderr_sha256"];
 if(!exact(v,["schema","kind","case","experiment_nonce","feature","externally_supplied_lifecycle_data",...hashes,...FALSE_FLAGS]))return false;
 const raw=original.originalPrivateCommandData(),external=v.externally_supplied_lifecycle_data,keys=externalKeys.filter(k=>k!=="feature");
 return v.schema===1&&v.kind==="HISTORICAL_GENUINE_INGRESS_DATA"&&v.case===raw.case&&v.experiment_nonce===raw.experiment_nonce&&v.feature===data.feature&&v.rpc_wire_sha256===raw.rpc_wire_sha256&&exact(external,keys)&&keys.every(k=>external[k]===data[k])&&hashes.every(k=>typeof v[k]==="string"&&/^[a-f0-9]{64}$/.test(v[k] as string))&&FALSE_FLAGS.every(k=>v[k]===false);
}
/** Source-only opaque owner. Neither protocol JSON nor a sample boolean can
 * construct this class or a live command owner. All originals stay in HELD. */
class OriginalGenuineIngressCollector {
 #child?:ChildProcess;#workerFd?:number;#projectFd?:number;#unknown?:MessageChannel;
 #streams:{stdout?:NodeJS.ReadableStream;stderr?:NodeJS.ReadableStream}={};
 #run?:Promise<void>;#originalRun?:Promise<void>;#resolve?:()=>void;
 #constructorCompleted=false;#actualExit=false;#exitCode:number|null=null;#exitSignal:string|null=null;
 #eof={stdout:false,stderr:false};#errors:string[]=[];#requests=0;#refused=false;
 #capture?:Record<string,unknown>;#result?:Record<string,unknown>;#finalizing=false;
 #line=Buffer.alloc(0);#protocolBytes=0;#stderrBytes=0;#stdoutHash=createHash("sha256");#stderrHash=createHash("sha256");
 #stdoutSha?:string;#stderrSha?:string;#settlementReported=false;#sourceFds:number[]=[];
 private constructor(issuer:symbol,private readonly original:StagingGenuineIngressExperiment,private readonly data:GenuineIngressExternalData,private readonly output:string){check(issuer===ISSUER,"identity_changed");}
 static issue(issuer:symbol,original:StagingGenuineIngressExperiment,data:GenuineIngressExternalData,output:string){return new OriginalGenuineIngressCollector(issuer,original,data,output);}
 [inspect.custom](){return "OriginalGenuineIngressCollector [same original worker/logs custody retained]";}
 toJSON():never{fail("invalid_request");}
 #latch(label:string){if(!this.#errors.includes(label))this.#errors.push(label);this.#refused=true;}
 #sample():boolean{try{this.original.remainingForOriginalLogObservation();return !this.#refused;}catch{if(!this.#capture)this.#refused=true;return false;}}
 #before(){check(this.#sample(),"expired");}
 #source(name:string,expected:string){
  this.#before();const fd=openSync(fileURLToPath(new URL(name,PREFIX)),constants.O_RDONLY|constants.O_NOFOLLOW);this.#sourceFds.push(fd);
  this.#before();const st=fstatSync(fd);check(st.isFile()&&st.uid===process.geteuid?.()&&(st.mode&0o022)===0&&st.size>0&&st.size<=65536,"identity_changed");
  this.#before();const bytes=readFileSync(fd);check(sha(bytes)===expected,"identity_changed");return fd;
 }
 #write(value:unknown){
  // Control-channel continuation, never a logs-operation grant by itself.
  // Each actual budget reply is freshly sampled immediately before writing.
  const live=this.#sample();if(exact(value,["schema","type","experiment_nonce","sequence","live"])&&value.type==="budget_reply"&&!live){value.live=false;this.#refused=true;}const raw=JSON.stringify(value)+"\n";check(Buffer.byteLength(raw)<=MAX_LINE,"invalid_request");
  try{check(this.#child?.stdin,"effect_unknown");this.#child.stdin.write(raw,error=>{if(error)this.#latch("worker_stdin");});}
  catch{this.#latch("worker_stdin");}
 }
 #frame(raw:Buffer){
  try{
   const line=new TextDecoder("utf-8",{fatal:true}).decode(raw);const parsed:unknown=JSON.parse(line);
   check(JSON.stringify(parsed)===line,"invalid_request");check(parsed&&typeof parsed==="object"&&!Array.isArray(parsed)&&"type" in parsed,"invalid_request");const v=parsed as Record<string,unknown>;
   const nonce=this.original.originalPrivateCommandData().experiment_nonce;
   if(v.type==="budget"){
    check(exact(v,["schema","type","experiment_nonce","sequence","operation"])&&v.schema===1&&v.experiment_nonce===nonce&&v.sequence===this.#requests+1&&Number.isSafeInteger(v.sequence)&&this.#requests<MAX_REQUESTS&&typeof v.operation==="string"&&OPS.has(v.operation)&&!this.#capture,"invalid_request");
    this.#requests++;const live=this.#sample();
    // A second fresh check, not reuse of the prior positive sample, controls
    // the actual response write. Refusal remains absorbing for future effects.
    const current=live&&this.#sample();if(!current)this.#refused=true;
    this.#write({schema:1,type:"budget_reply",experiment_nonce:nonce,sequence:this.#requests,live:current});return;
   }
   if(v.type==="captured"){
    check(exact(v,["schema","type","experiment_nonce","capture","qualified"])&&v.schema===1&&v.experiment_nonce===nonce&&v.qualified===false&&!this.#capture&&captureSchema(v.capture),"invalid_request");
    this.#capture=v.capture;void this.#finalize();return;
   }
   if(v.type==="result"){
    check(exact(v,["schema","type","experiment_nonce","capture","projection","qualified"])&&v.schema===1&&v.experiment_nonce===nonce&&v.qualified===false&&this.#capture&&!this.#result&&JSON.stringify(v.capture)===JSON.stringify(this.#capture)&&(v.projection===null||projectionValid(v.projection,this.data,this.original)),"invalid_request");
    this.#result=v;return;
   }
   check(v.type==="refused"&&exact(v,["schema","type","experiment_nonce","requests","qualified"])&&v.schema===1&&v.experiment_nonce===nonce&&v.requests===this.#requests&&v.qualified===false&&!this.#result,"invalid_request");this.#result=v;this.#latch("worker_refused");
  }catch{this.#latch("worker_protocol");} // unknown original worker is retained, never signaled/replaced
 }
 async #finalize(){
  check(!this.#finalizing,"effect_unknown");this.#finalizing=true;
  try{
   if(captureValid(this.#capture,this.#requests)&&!this.#refused&&!this.#errors.length){try{await this.#originalRun;}catch{this.#latch("original_run_refused");}}
   // Pure historical projection is allowed after live collection has settled.
   // No further logs operation is accepted after the captured notice.
   const privateData=this.original.originalPrivateCommandData(),status=this.original.statusData();
   const expected={schema:1,case:privateData.case,...this.data,experiment_nonce:privateData.experiment_nonce,
    rpc_wire:privateData.rpc_wire,rpc_wire_sha256:privateData.rpc_wire_sha256,session_generation:privateData.session_generation,command_key:privateData.command_key,
    dispatch_disposition:status.dispatch_disposition,dispatch_attempts:status.dispatch_attempts,rpc_send_observations:status.rpc_send_observations,original_session_retained:status.original_session_retained};
   this.#write({schema:1,type:"finalize",experiment_nonce:privateData.experiment_nonce,expected});
  }catch{this.#latch("finalize");} // no fabricated expected data; same waiting worker remains retained
 }
 #read(name:"stdout"|"stderr",stream:NodeJS.ReadableStream){
  try{for(;;){this.#sample();const chunk=stream.read() as Buffer|null;if(chunk===null)break;
   check(Buffer.isBuffer(chunk),"invalid_request");
   if(name==="stderr"){this.#stderrBytes+=chunk.length;this.#stderrHash.update(chunk);if(chunk.length)this.#latch("worker_stderr");continue;}
   this.#stdoutHash.update(chunk);this.#protocolBytes+=chunk.length;
   if(this.#protocolBytes>MAX_PROTOCOL){this.#latch("worker_protocol_bound");continue;}
   this.#line=Buffer.concat([this.#line,chunk]);
   for(;;){const at=this.#line.indexOf(10);if(at<0)break;if(at>MAX_LINE){this.#latch("worker_line_bound");this.#line=this.#line.subarray(at+1);continue;}
    const frame=this.#line.subarray(0,at);this.#line=this.#line.subarray(at+1);this.#frame(frame);}
   if(this.#line.length>MAX_LINE){this.#latch("worker_line_bound");this.#line=Buffer.alloc(0);}
  }}catch{this.#latch("worker_read");}
 }
 #settled(){if(!this.#settlementReported&&this.#actualExit&&this.#eof.stdout&&this.#eof.stderr){this.#settlementReported=true;if(this.#line.length)this.#latch("worker_partial_line");try{this.#stdoutSha=this.#stdoutHash.digest("hex");this.#stderrSha=this.#stderrHash.digest("hex");}catch{this.#latch("worker_hash");}this.#unknown?.port1.close();this.#unknown?.port2.close();this.#resolve?.();this.#resolve=undefined;}}
 startRetaining():Promise<void>{
  check(!this.#run,"effect_unknown");this.#run=new Promise(resolve=>{this.#resolve=resolve;});void this.#start();return this.#run;
 }
 async #start(){
  try{
   const notice=await this.original.waitForOriginalCommandObservationRetaining();check(notice==="captured","effect_unknown");
   this.#originalRun=this.original.originalDispatchPromiseRetaining();this.#before();
   this.#workerFd=this.#source("genuine-ingress-log-worker.py",WORKER_SHA256);this.#projectFd=this.#source("genuine-ingress-log-project.py",PROJECT_SHA256);
   this.#before();this.#unknown=new MessageChannel();this.#unknown.port1.on("message",()=>{});this.#unknown.port1.ref();this.#unknown.port2.ref();
   this.#before();const child=new ChildProcess();this.#child=child; // retained BEFORE original spawn method entry
   child.on("error",()=>this.#latch("worker_error"));
   child.once("exit",(code,signal)=>{if(Number.isInteger(code)||typeof signal==="string"){this.#actualExit=true;this.#exitCode=code;this.#exitSignal=signal;this.#settled();}else this.#latch("worker_exit_type");});
   const nonce=this.original.originalPrivateCommandData().experiment_nonce;
   this.#before();
   // Exported ChildProcess's retained spawn method is runtime-checked; no
   // public spawn() call can lose its returned original object on an exception.
   const spawn=(child as unknown as {spawn(options:unknown):unknown}).spawn;check(typeof spawn==="function","unconfigured");
   const envPairs=this.original.originalLogWorkerEnvironmentRetaining();
   check(envPairs.length<=2048&&envPairs.reduce((n,v)=>n+Buffer.byteLength(v),0)<=1024*1024,"invalid_request");
   this.#before();spawn.call(child,{file:"/usr/bin/python3",args:["/usr/bin/python3","-I","-B","/proc/self/fd/3","/proc/self/fd/4"],stdio:["pipe","pipe","pipe",this.#workerFd,this.#projectFd],envPairs});this.#constructorCompleted=true;
   this.#streams={stdout:child.stdout??undefined,stderr:child.stderr??undefined};
   for(const name of ["stdout","stderr"] as const){const stream=this.#streams[name];check(stream,"effect_unknown");stream.on("readable",()=>this.#read(name,stream));stream.once("end",()=>{this.#eof[name]=true;this.#settled();});stream.on("error",()=>this.#latch("worker_stream_error"));this.#read(name,stream);}
   this.#write({schema:1,type:"start",experiment_nonce:nonce,target:{app:this.data.app,machine:this.data.machine},output:this.output});
  }catch{this.#latch("worker_start");
   // If no ChildProcess was allocated, no original spawn method was entered.
   // After allocation ANY uncertainty holds the same owner and unknown latch;
   // no missing PID/exception is upgraded to absence or a replacement worker.
   if(!this.#child){this.#unknown?.port1.close();this.#unknown?.port2.close();this.#resolve?.();this.#resolve=undefined;}
  }
 }
 statusData(){return snapshot({scope:"original-node-python-log-bridge-data-only",worker_constructor_completed:this.#constructorCompleted,
  actual_worker_exit_observed:this.#actualExit,worker_exit_code:this.#exitCode,worker_exit_signal:this.#exitSignal,
  worker_stdout_eof:this.#eof.stdout,worker_stderr_eof:this.#eof.stderr,worker_stdout_bytes:this.#protocolBytes,worker_stderr_bytes:this.#stderrBytes,
  worker_stdout_sha256:this.#stdoutSha??null,worker_stderr_sha256:this.#stderrSha??null,worker_observer_errors:this.#errors,
  original_budget_requests:this.#requests,original_budget_refused:this.#refused,logs_capture:this.#capture??null,
  historical_projection:this.#result?.type==="result"?this.#result.projection:null,
  bridge_capture_complete:this.#constructorCompleted&&this.#actualExit&&this.#exitCode===0&&this.#exitSignal===null&&this.#eof.stdout&&this.#eof.stderr&&!this.#errors.length&&!this.#refused&&this.#stderrBytes===0&&captureValid(this.#capture,this.#requests)&&this.#result?.type==="result"&&!!this.#result.projection,
  owners_retained:true,qualified:false,source_accepted:false,runtime_ready:false,retirement_complete:false,root_acknowledgment:false,serving:false,current_claim:false});}
}
export type {OriginalGenuineIngressCollector};
export function createOriginalGenuineIngressCollector(original:StagingGenuineIngressExperiment,data:GenuineIngressExternalData,output:string):OriginalGenuineIngressCollector{
 check(COLLECTOR_EFFECTS_ALLOWED,"unconfigured"); // before input/owner/env/file/clock/process access
 assertStagingGenuineIngressExperiment(original);check(HELD.size<1&&!HELD.has(original),"effect_unknown");
 check(exact(data,externalKeys),"invalid_request");for(const key of ["app","machine","instance_id"] as const)check(/^[A-Za-z0-9][A-Za-z0-9_-]{0,79}$/.test(data[key]),"invalid_request");
 for(const key of ["source_manifest_sha256","root_receipt_manifest_sha256","fixture_manifest_sha256","native_elf_sha256","engine_elf_sha256"] as const)check(/^[a-f0-9]{64}$/.test(data[key])&&data[key]!=="0".repeat(64),"invalid_request");
 check(/^[a-f0-9]{40}$/.test(data.integrated_revision)&&/^sha256:[a-f0-9]{64}$/.test(data.image_digest)&&data.image_digest!=="sha256:"+"0".repeat(64),"invalid_request");
 check(data.root_receipt_manifest_sha256==="08ec75f53153899d855c53621a86d2781e89ce7bb12c77f97a55cd386dd0f264"&&data.fixture_manifest_sha256==="7a5a41afc55a7569a709d1d2224c3a5b1368ac16731f52f6b0f3411c6d7ecacf","invalid_request");
 const leaf={CloseWins:"managed-ingress-fixture-close-wins",AdmissionWins:"managed-ingress-fixture-admission-wins",CarrierContention:"managed-ingress-fixture-carrier-contention"};check(data.feature===leaf[original.statusData().case],"invalid_request");
 check(typeof output==="string"&&output.length>0&&output.length<=4096&&!output.includes("\0"),"invalid_request");
 const owner=OriginalGenuineIngressCollector.issue(ISSUER,original,snapshot(data),output);HELD.set(original,owner);return owner;
}
