import {inspect} from "node:util";
import {createHash} from "node:crypto";
import type {HomeControlSessions} from "../home-control-sessions.js";
import type {OriginalTime} from "./fly.js";
import type {StagingSetupRestriction} from "./staging-role-activation.js";
import type {CommittedHostTransport} from "./transport.js";
import type {ManagedHomeRegistry,ManagedHomeRoute,StagingOriginalCommandBudgetBorrow,StagingOriginalCommandObservationWindow} from "./registry.js";
import {check,fail,snapshot,type HostOperation} from "./types.js";

// NEW activated derivative + reviewed actual bindings are mandatory. This
// source package cannot dispatch a genuine experiment even if imported.
const GENUINE_INGRESS_EFFECTS_ALLOWED=false;
const ISSUER=Symbol("original staging genuine ingress experiment");
const originals=new WeakSet<object>();
type Case="CloseWins"|"AdmissionWins"|"CarrierContention";
export interface StagingGenuineIngressInput {case:Case;experimentNonce:string;model:string;profileId:string;}
type Session<S extends object>=ReturnType<HomeControlSessions<S,unknown>["attach"]>;
type ErrorLabel="committed_capture"|"rpc_capture"|"send_callback"|"known_reply"|"listener_refused";
const sha=(wire:string)=>createHash("sha256").update(wire).digest("hex");
const fields=(value:unknown,names:readonly string[]):value is Record<string,unknown>=>value!==null&&typeof value==="object"&&!Array.isArray(value)&&Object.keys(value).sort().join(",")===[...names].sort().join(",");
function text(value:unknown){check(typeof value==="string"&&Buffer.byteLength(value)>0&&Buffer.byteLength(value)<=256&&!/[\x00-\x1f\x7f]/.test(value));}
class StagingGenuineIngressExperiment<S extends object=object> {
  #input:Readonly<StagingGenuineIngressInput>;#logCredential?:string;
  #committed?:{owner:CommittedHostTransport;socket:S;session:Session<S>};
  #actual?:{registry:ManagedHomeRegistry;route:ManagedHomeRoute;sessions:HomeControlSessions<S,unknown>;setup:StagingSetupRestriction;driver:OriginalTime};
  #operation?:HostOperation;#dispatch?:Promise<void>;#request?:Promise<unknown>;
  #ready:Promise<"captured"|"refused">;#resolveReady?:((value:"captured"|"refused")=>void);
  #dispatchError?:unknown;#requestError?:unknown;#borrow?:StagingOriginalCommandBudgetBorrow;#window?:StagingOriginalCommandObservationWindow;
  #rpc?:{wire:string;id:string;sha256:string};#errors:ErrorLabel[]=[];#sendCount=0;#dispatchCount=0;
  #disposition:"not_started"|"pending"|"effect_unknown"|"refused"|"unexpected_known_reply"="not_started";
  private constructor(issuer:symbol,input:StagingGenuineIngressInput){check(issuer===ISSUER);this.#input=Object.freeze(snapshot(input));this.#ready=new Promise(resolve=>{this.#resolveReady=resolve;});originals.add(this);}
  static issue<S extends object>(issuer:symbol,input:StagingGenuineIngressInput){return new StagingGenuineIngressExperiment<S>(issuer,input);}
  [inspect.custom](){return "StagingGenuineIngressExperiment [original ordinary owners retained]";}
  toJSON():never{fail("invalid_request");}
  #latch(label:ErrorLabel){if(!this.#errors.includes(label))this.#errors.push(label);}
  /** Fixed observer called ONLY by the original authenticated wire callback,
   * after its actual owner checks and SAME HomeControlSessions.attach. */
  retainOriginalCommitted(socket:S,owner:CommittedHostTransport,session:Session<S>){
    try{check(!this.#committed,"effect_unknown");this.#committed={owner,socket,session};check(owner.socket===socket,"identity_changed");owner.assertLocalCurrent();}
    catch{this.#latch("committed_capture");}
  }
  /** Fixed, best-effort hook immediately before the SAME adapter's ws.send.
   * Never attaches/sends/replies/closes or changes the original send result. */
  observeOriginalSerializedSend(socket:S,wire:string){
    if(!this.#operation||socket!==this.#committed?.socket)return;
    try{this.#sendCount++;check(this.#sendCount===1&&!this.#rpc,"effect_unknown");
      check(typeof wire==="string"&&Buffer.byteLength(wire)<=32768);const rpc:unknown=JSON.parse(wire);
      check(fields(rpc,["type","id","method","params"])&&JSON.stringify(rpc)===wire,"identity_changed");
      check(rpc.type==="rpc"&&rpc.method==="managed.openPreparedProjectV1"&&typeof rpc.id==="string"&&/^[A-Za-z0-9_.:-]{1,128}$/.test(rpc.id),"identity_changed");
      const p=rpc.params;check(fields(p,["command_key","project_alias","backend","model","profile_id"]),"identity_changed");
      check(this.#operation.op==="OpenPreparedProjectV1"&&p.command_key===this.commandKey()&&p.project_alias===this.#operation.project_alias&&p.backend==="codex"&&p.model===this.#input.model&&p.profile_id===this.#input.profileId,"identity_changed");
      this.#rpc={wire,id:rpc.id,sha256:sha(wire)};this.#resolveReady?.("captured");this.#resolveReady=undefined;
    }catch{this.#latch("rpc_capture");}
  }
  observeOriginalSendCallback(failed:boolean){if(failed)this.#latch("send_callback");}
  commandKey(){return "genuine-ingress:"+this.#input.experimentNonce;}
  #currentSocket(){const actual=this.#actual,committed=this.#committed;check(actual&&committed&&actual.sessions.isCurrent(committed.session),"identity_changed");committed.owner.assertLocalCurrent();return committed;}
  /** Retained by the original listener BEFORE yielding this same promise. */
  runRetaining(registry:ManagedHomeRegistry,route:ManagedHomeRoute,sessions:HomeControlSessions<S,unknown>,setup:StagingSetupRestriction,driver:OriginalTime):Promise<void>{
    check(!this.#actual&&!this.#dispatch,"effect_unknown");this.#actual={registry,route,sessions,setup,driver};
    this.#dispatch=(async()=>{try{
      check(!this.#errors.length,"effect_unknown");setup.remaining();driver.remaining();await route.assertRoutableCurrent();
      const committed=this.#currentSocket();await committed.owner.assertCurrent();await registry.assertManagedHomeSocketMatchesRoute(committed.socket,route);this.#currentSocket();
      this.#operation={op:"OpenPreparedProjectV1",project_alias:route.projectAlias,backend:"codex",model:this.#input.model,profile_id:this.#input.profileId};
      this.#dispatchCount++;this.#disposition="pending";
      try{await registry.dispatchStagingGenuineIngressCommand(route,this.commandKey(),this.#operation,async(method,params,_sample,original)=>{
        // Original borrow is retained before any fallible async/current check.
        check(!this.#borrow,"effect_unknown");this.#borrow=original;
        const held=this.#currentSocket();await held.owner.assertCurrent();await registry.assertManagedHomeSocketMatchesRoute(held.socket,route);this.#currentSocket();
        setup.remaining();driver.remaining();
        // One original window armed BEFORE actual session request/send. Its
        // countdown is derived from the SAME carrier, never the stale sample.
        this.#window=original.startObservationWindowRetaining();
        const remaining=Math.min(original.remaining(),setup.remaining(),driver.remaining());
        this.#request=sessions.request(route.homeId,held.socket,method,params,remaining);
        try{return await this.#request;}catch(error){this.#requestError=error;throw error;}
      });this.#disposition="unexpected_known_reply";this.#latch("known_reply");}
      catch(error){this.#dispatchError=error;this.#disposition=error!==null&&typeof error==="object"&&"code" in error&&error.code==="effect_unknown"?"effect_unknown":"refused";}
    }finally{this.#resolveReady?.("refused");this.#resolveReady=undefined;}})();
    return this.#dispatch;
  }
  /** Fixed opt-in constructor hook: SAME original provider credential, never
   * a profile/env loader or status field. Only the retained local worker gets it. */
  retainOriginalLogCredential(token:string){check(!this.#logCredential&&!this.#actual,"effect_unknown");check(typeof token==="string"&&token.length>0&&token.length<=16384&&!/[\x00-\x1f\x7f]/.test(token),"invalid_request");this.#logCredential=token;}
  originalLogWorkerEnvironmentRetaining():string[]{check(this.#logCredential,"unconfigured");return ["PATH=/usr/bin:/bin","PYTHONIOENCODING=utf-8","FLY_API_TOKEN="+this.#logCredential];}
  originalDispatchPromiseRetaining():Promise<void>{check(this.#dispatch,"effect_unknown");return this.#dispatch;}
  /** Fixed original-capture handoff, not acceptance/current/deadline DATA.
   * Caller keeps the SAME run promise concurrently; never waits for the 20s
   * original request timeout and then opens a new collection clock. */
  waitForOriginalCommandObservationRetaining():Promise<"captured"|"refused">{
    return this.#ready;
  }
  /** Fixed listener pre-dispatch refusal; retains the SAME ready owner and
   * leaves the original run promise/error with its listener. No authority. */
  refuseOriginalObservationBeforeDispatch(){this.#latch("listener_refused");this.#resolveReady?.("refused");this.#resolveReady=undefined;}
  /** This is a live SAME-HOST borrow for an original local collector, not a
   * serialized remote end. Each call samples the same command/setup/driver.
   * Socket loss/unknown transport never creates another send or new clock. */
  remainingForOriginalLogObservation():number{
    check(this.#actual&&this.#borrow&&this.#window&&this.#rpc&&this.#sendCount===1&&!this.#errors.length,"effect_unknown");
    return Math.min(this.#window.remaining(),this.#actual.setup.remaining(),this.#actual.driver.remaining());
  }
  originalPrivateCommandData(){check(this.#rpc&&this.#committed&&this.#sendCount===1&&!this.#errors.length,"effect_unknown");
    return {schema:1,case:this.#input.case,experiment_nonce:this.#input.experimentNonce,command_key:this.commandKey(),
      rpc_wire:this.#rpc.wire,rpc_wire_sha256:this.#rpc.sha256,session_generation:this.#committed.owner.generation};}
  statusData(){return snapshot({scope:"genuine-ingress-historical-data-only",case:this.#input.case,experiment_nonce:this.#input.experimentNonce,
    dispatch_disposition:this.#disposition,dispatch_attempts:this.#dispatchCount,rpc_send_observations:this.#sendCount,
    rpc_wire_sha256:this.#rpc?.sha256??null,original_session_retained:!!this.#committed,original_command_borrow_retained:!!this.#borrow,
    original_observation_window_retained:!!this.#window,observer_errors:this.#errors,owners_retained:true,
    source_accepted:false,runtime_ready:false,retirement_complete:false,root_acknowledgment:false,serving:false,qualified:false});}
}
export type {StagingGenuineIngressExperiment};
/** Only explicit trusted staging construction can opt into the new route.
 * Default listener behavior is untouched. A JSON/HTTP bool cannot mint this.
 * FALSE guard is BEFORE input reads; a new reviewed derivative must enable it.
 */
export function createStagingGenuineIngressExperiment<S extends object=object>(input:StagingGenuineIngressInput):StagingGenuineIngressExperiment<S>{
  check(GENUINE_INGRESS_EFFECTS_ALLOWED,"unconfigured");
  check(fields(input,["case","experimentNonce","model","profileId"]));check(["CloseWins","AdmissionWins","CarrierContention"].includes(input.case));
  check(/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(input.experimentNonce));text(input.model);text(input.profileId);
  return StagingGenuineIngressExperiment.issue<S>(ISSUER,input);
}
export function assertStagingGenuineIngressExperiment<S extends object>(value:StagingGenuineIngressExperiment<S>):void{check(originals.has(value),"identity_changed");}
