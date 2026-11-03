/** One original stdin-only staging credential transfer. No route, task, profile
 * freshness or provider authority is issued by the acknowledgement. */
import {ChildProcess} from "node:child_process";
import {performance} from "node:perf_hooks";
import {inspect} from "node:util";
import type {HomeControlSessions} from "../home-control-sessions.js";
import type {OriginalTime} from "./fly.js";
import type {ManagedHomeRegistry,ManagedHomeRoute} from "./registry.js";
import type {CommittedHostTransport} from "./transport.js";
import type {StagingSetupRestriction} from "./staging-role-activation.js";
import {check,fail,snapshot,uuid} from "./types.js";

const ACK=Buffer.from("CODEX_OAUTH_ACCESS_SNAPSHOT_IMPORTED_V1\n");
const HELD=new Set<OriginalStagingCodexProfileImport<any>>();
export type CodexImportTarget=Readonly<{app:string;machine:string;instanceId:string;projectId:string;home:string}>;
export type CodexImportOriginal<S extends object>=Readonly<{
 registry:ManagedHomeRegistry;route:ManagedHomeRoute;sessions:HomeControlSessions<S,unknown>;
 session:ReturnType<HomeControlSessions<S,unknown>["attach"]>;socket:S;owner:CommittedHostTransport;
 setup:StagingSetupRestriction;driver:OriginalTime;
 /** The caller joins these exact identifiers to its original ordinary creation,
  * configuration and started readback, under the same registry/route owner. */
 assertTargetCurrent(target:CodexImportTarget):void|Promise<void>;
}>;
function text(v:unknown,n:number):v is string{return typeof v==="string"&&v.length>0&&Buffer.byteLength(v)<=n&&!/[\p{Cc}\uD800-\uDFFF]/u.test(v);}
/** Unverified JWT claims bound selection/deadline DATA only. The original
 * Codex server remains the bearer authenticator. No refresh or store access. */
export function validateStagingCodexAccessSnapshot(value:unknown,profileId:string,now=Date.now()):Buffer{
 let p:any,claims:any,decoded:Buffer|undefined,raw:Buffer|undefined;
 try{
  raw=Buffer.isBuffer(value)?Buffer.from(value):Buffer.from(JSON.stringify(value)??"null");
  check(raw.length>0&&raw.length<=65532&&text(profileId,256));
  check(Number.isSafeInteger(now)&&now>=0);
  p=JSON.parse(new TextDecoder("utf-8",{fatal:true}).decode(raw));
  check(p&&typeof p==="object"&&!Array.isArray(p));
  const required=["formatVersion","kind","profileId","backend","provider","accessToken","chatgptAccountId","expiresAtMs"];
  check(Object.keys(p).every(k=>required.includes(k)||k==="chatgptPlanType")&&required.every(k=>Object.hasOwn(p,k)));

  check(p.formatVersion===1&&p.kind==="CodexOAuthAccessSnapshotV1"&&p.profileId===profileId&&p.backend==="codex"&&p.provider==="openai-codex");
  check(text(p.accessToken,48*1024)&&text(p.chatgptAccountId,256));
  check(!Object.hasOwn(p,"chatgptPlanType")||text(p.chatgptPlanType,128));
  const parts=p.accessToken.split(".");check(parts.length===3&&parts.every((x:string)=>/^[A-Za-z0-9_-]+$/.test(x)));
  decoded=Buffer.from(parts[1],"base64url");check(decoded.length<=16*1024&&decoded.toString("base64url")===parts[1]);
  const claimText=new TextDecoder("utf-8",{fatal:true}).decode(decoded);
  claims=JSON.parse(claimText);
  // Duplicate keys in the JWT cannot select different identities in different
  // parsers. Canonicalizing a parsed object alone would silently discard them.
  const tokens=claimText.match(/"(?:\\.|[^"\\])*"|[{}\[\]:,]|[^\s{}\[\]:,]+/g)??[],stack:(Set<string>|null)[]=[];
  for(let i=0;i<tokens.length;i++){const t=tokens[i];if(t==="{")stack.push(new Set());else if(t==="[")stack.push(null);else if(t==="}"||t==="]")stack.pop();else if(t.startsWith('"')&&tokens[i+1]===":"){const keys=stack.at(-1),k=JSON.parse(t);check(keys&&!keys.has(k));keys.add(k);}check(stack.length<=32);}
  check(claims&&typeof claims==="object"&&!Array.isArray(claims));
  check(Number.isSafeInteger(claims.exp)&&claims.exp>0&&Number.isSafeInteger(claims.exp*1000));
  const auth=claims["https://api.openai.com/auth"];
  check(auth&&typeof auth==="object"&&!Array.isArray(auth)&&auth.chatgpt_account_id===p.chatgptAccountId);
  const plan=auth.chatgpt_plan_type;
  check(plan===undefined||plan===null||text(plan,128));
  check(p.chatgptPlanType===(typeof plan==="string"?plan:undefined));
  check(p.expiresAtMs===claims.exp*1000&&p.expiresAtMs>now&&p.expiresAtMs-now<=14*86400000);
  const wire=Buffer.from(JSON.stringify({formatVersion:p.formatVersion,kind:p.kind,profileId:p.profileId,backend:p.backend,provider:p.provider,accessToken:p.accessToken,chatgptAccountId:p.chatgptAccountId,expiresAtMs:p.expiresAtMs,...(Object.hasOwn(p,"chatgptPlanType")?{chatgptPlanType:p.chatgptPlanType}:{})}));
  if(Buffer.isBuffer(value))check(wire.equals(raw));
  return wire;
 }catch{return fail("invalid_request");}finally{decoded?.fill(0);raw?.fill(0);}
}
export class OriginalStagingCodexProfileImport<S extends object>{
 #actual:CodexImportOriginal<S>;#target:CodexImportTarget;#raw:Buffer;#expires:number;#token:string;
 #child?:ChildProcess;#run?:Promise<void>;#resolve?:()=>void;#reject?:(e:Error)=>void;#timer?:NodeJS.Timeout;
 #started=0;#constructorCompleted=false;#spawnAttempted=false;#stdinComplete=false;#stdout:Buffer[]=[];#stdoutBytes=0;#stderrBytes=0;
 #eof={stdout:false,stderr:false};#exitObserved=false;#closed=false;#exit:number|null=null;#signal:string|null=null;
 #errors:string[]=[];#terminated=false;#finishing=false;#imported=false;
 constructor(actual:CodexImportOriginal<S>,raw:Buffer,readonly profileId:string,target:CodexImportTarget,token:string,
  private readonly executable="/usr/local/bin/rudder-staging-fly",private readonly allocate:()=>ChildProcess=()=>new ChildProcess()){
  this.#raw=validateStagingCodexAccessSnapshot(raw,profileId);this.#expires=JSON.parse(this.#raw.toString()).expiresAtMs;this.#actual=Object.freeze({...actual});
  uuid(target.projectId);check(/^[a-z][a-z0-9-]{0,62}$/.test(target.app)&&/^[a-f0-9]{14,16}$/.test(target.machine));
  check(/^[A-Za-z0-9]{26}$/.test(target.instanceId)&&target.home===`/workspace/fresh-v4/${target.projectId}/home`);
  check(Object.keys(target).length===5&&Object.keys(target).every(k=>["app","machine","instanceId","projectId","home"].includes(k)));
  check(/^\/[A-Za-z0-9_./-]+$/.test(executable)&&typeof token==="string"&&token.length>0&&!/[\r\n\0]/.test(token),"unconfigured");
  this.#target=Object.freeze({...target});this.#token=token;
 }
 [inspect.custom](){return "OriginalStagingCodexProfileImport [stdin-only original child retained]";}
 toJSON():never{fail("invalid_request");}
 #error(code:string){if(!this.#errors.includes(code))this.#errors.push(code);}
 #remaining(){const a=this.#actual;const n=Math.floor(Math.min(20000-(performance.now()-this.#started),a.setup.remaining(),a.driver.remaining(),this.#expires-Date.now()));check(n>0,"expired");return n;}
 #local(){const a=this.#actual;this.#remaining();check(a.owner.socket===a.socket&&a.owner.homeId===a.route.homeId&&a.session.homeId===a.route.homeId&&a.session.socket===a.socket&&a.sessions.isCurrent(a.session),"identity_changed");a.owner.assertLocalCurrent();}
 async #current(){const a=this.#actual;this.#local();await a.owner.assertCurrent();this.#local();await a.registry.assertManagedHomeSocketMatchesRoute(a.socket,a.route);this.#local();await a.assertTargetCurrent(this.#target);this.#local();}
 #stop(code:string){this.#error(code);if(this.#terminated)return;this.#terminated=true;try{this.#child?.kill("SIGKILL");}catch{this.#error("termination_unknown");}}
 #finish(){
  if(this.#finishing||!this.#exitObserved||!this.#closed||!this.#eof.stdout||!this.#eof.stderr)return;
  this.#finishing=true;if(this.#timer)clearTimeout(this.#timer);
  void (async()=>{
   try{check(this.#constructorCompleted&&this.#stdinComplete&&this.#exit===0&&this.#signal===null&&!this.#errors.length,"effect_unknown");
    check(this.#stderrBytes===0&&Buffer.concat(this.#stdout).equals(ACK),"effect_unknown");await this.#current();this.#imported=true;this.#resolve?.();
   }catch{this.#error("original_import_unconfirmed");this.#reject?.(new Error("Staging Codex profile import unconfirmed; original owner retained."));}
   finally{this.#raw.fill(0);this.#stdout=[];this.#token="";}
  })();
 }
 startRetaining():Promise<void>{
  check(!this.#run,"effect_unknown");this.#started=performance.now();HELD.add(this);
  this.#run=new Promise<void>((resolve,reject)=>{this.#resolve=resolve;this.#reject=reject;});
  // Attach a rejection observer even when the caller has not awaited yet.
  void this.#run.catch(()=>{});
  void this.#begin().catch(()=>{this.#error("original_import_start_unknown");this.#raw.fill(0);
   if(!this.#child){if(this.#timer)clearTimeout(this.#timer);this.#token="";this.#reject?.(new Error("Staging Codex profile import unavailable; original owner retained."));}
   else this.#stop("original_import_start_unknown");
  });return this.#run;
 }
 async #begin(){
  this.#timer=setTimeout(()=>this.#stop("original_import_deadline"),this.#remaining());await this.#current();check(!this.#errors.length,"expired");
  const child=this.allocate();this.#child=child; // retained BEFORE its constructor effect.
  child.on("error",()=>this.#stop("original_child_error"));
  child.once("exit",(code,signal)=>{this.#exit=code;this.#signal=signal;this.#exitObserved=typeof code==="number"&&Number.isInteger(code)||typeof signal==="string"&&signal.length>0;if(!this.#exitObserved)this.#error("invalid_original_exit");this.#finish();});
  child.once("close",()=>{this.#closed=true;this.#finish();});
  const command=`/usr/bin/env RUDDER_HOME=${this.#target.home} /usr/local/bin/node /opt/rudder/dist/managed-task-profile-cli.js import-codex-oauth-access-snapshot`;
  const args=[this.executable,"ssh","console","--app",this.#target.app,"--machine",this.#target.machine,"--user","rudder","--quiet","-C",command];
  const env:NodeJS.ProcessEnv={...process.env,FLY_API_TOKEN:this.#token};delete env.FLY_ACCESS_TOKEN;
  this.#local();check(!this.#errors.length,"expired");this.#spawnAttempted=true;
  (child as unknown as {spawn(o:unknown):unknown}).spawn({file:this.executable,args,stdio:["pipe","pipe","pipe"],envPairs:Object.entries(env).filter((x):x is [string,string]=>typeof x[1]==="string").map(([k,v])=>`${k}=${v}`)});this.#constructorCompleted=true;
  for(const name of ["stdout","stderr"]as const){const stream=child[name];check(stream,"effect_unknown");stream.on("data",(part:Buffer)=>{try{
   check(Buffer.isBuffer(part));if(name==="stdout"){this.#stdoutBytes+=part.length;if(this.#stdoutBytes<=ACK.length)this.#stdout.push(Buffer.from(part));}else this.#stderrBytes+=part.length;
   if(this.#stdoutBytes>ACK.length||this.#stderrBytes>4096)this.#stop("original_output_bound");
  }catch{this.#stop("original_capture_unknown");}});stream.once("end",()=>{this.#eof[name]=true;this.#finish();});stream.on("error",()=>this.#stop("original_stream_error"));}
  check(child.stdin,"effect_unknown");child.stdin.on("error",()=>this.#stop("original_stdin_error"));
  this.#local();check(!this.#errors.length,"expired");
  child.stdin.end(this.#raw,()=>{this.#stdinComplete=true;this.#raw.fill(0);this.#finish();});
 }
 statusData(){return snapshot({scope:"one-original-staging-profile-import-data-only",target:this.#target,profile_id:this.profileId,spawn_attempted:this.#spawnAttempted,constructor_completed:this.#constructorCompleted,stdin_write_completed:this.#stdinComplete,actual_exit_observed:this.#exitObserved,close_observed:this.#closed,stdout_eof:this.#eof.stdout,stderr_eof:this.#eof.stderr,exit_code:this.#exit,exit_signal:this.#signal,stdout_bytes:this.#stdoutBytes,stderr_bytes:this.#stderrBytes,imported:this.#imported,errors:this.#errors,owners_retained:true,task_started:false,qualification_authority:false});}
}
