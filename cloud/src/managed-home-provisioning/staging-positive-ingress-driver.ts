/** Explicit staging task observation. The listener retains this driver and its
 * original promises through settlement or uncertainty. No completion authority. */
import {inspect} from "node:util";
import type {HomeControlSessions} from "../home-control-sessions.js";
import {originalTime,type OriginalTime} from "./fly.js";
import type {ManagedHomeRegistry,ManagedHomeRoute} from "./registry.js";
import type {CommittedHostTransport} from "./transport.js";
import type {StagingSetupRestriction} from "./staging-role-activation.js";
import {check,exact,snapshot,type HostOperation} from "./types.js";

export type PositiveIngressInput=Readonly<{openCommandKey:string;startCommandKey:string;stopCommandKey?:string;model:string;profileId:string;text:string}>;
type Original<S extends object>=Readonly<{registry:ManagedHomeRegistry;route:ManagedHomeRoute;
  sessions:HomeControlSessions<S,unknown>;session:ReturnType<HomeControlSessions<S,unknown>["attach"]>;
  socket:S;owner:CommittedHostTransport;setup:StagingSetupRestriction;driver:OriginalTime}>;
type Carrier={key:string;operation:HostOperation;dispatch?:Promise<unknown>;acceptance?:unknown;
  queries:Promise<unknown>[];last?:unknown;unknown:boolean};
type Fence=Readonly<{run_id:string;run_epoch:string;attempt_id:string}>;

/** This constructor issues no route/session/bootstrap capability. All of those
 * must be the actual already-authenticated objects retained by the listener.
 * No JSON, bearer string, provider receipt or alias can replace them.
 */
export function validatePositiveIngressInput(value:PositiveIngressInput):PositiveIngressInput {
  const input=snapshot(value,32*1024) as PositiveIngressInput;
  exact(input,["openCommandKey","startCommandKey","stopCommandKey","model","profileId","text"]);
  const keys=[input.openCommandKey,input.startCommandKey,...(input.stopCommandKey===undefined?[]:[input.stopCommandKey])];
  for(const key of keys)
    check(typeof key==="string"&&/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(key));
  check(new Set(keys).size===keys.length);
  for(const value of [input.model,input.profileId])check(typeof value==="string"&&value.trim().length>0&&Buffer.byteLength(value)<=256&&!/[\x00-\x1f\x7f]/.test(value));
  check(typeof input.text==="string"&&input.text.trim().length>0&&Buffer.byteLength(input.text)<=24*1024&&!input.text.includes("\0"));
  return Object.freeze({...input});
}
export function preparePositiveIngressRetaining<S extends object>(actual:Original<S>,input:PositiveIngressInput) {
  return new PositiveIngressDriver(Object.freeze({...actual}),validatePositiveIngressInput(input));
}

export class PositiveIngressDriver<S extends object> {
  #commands:Carrier[]=[];#run?:Promise<void>;#fence?:Fence;#readyObserved=false;#refused=false;
  #lifecycleComplete=false;#lifecycle?:Record<string,unknown>;
  constructor(private readonly actual:Original<S>,private readonly input:PositiveIngressInput){}
  [inspect.custom](){return "PositiveIngressDriver [original owners retained; no completion authority]";}
  toJSON():never{throw new Error("positive ingress owner cannot be serialized");}
  #current(){const a=this.actual;a.setup.remaining();a.driver.remaining();
    check(a.owner.socket===a.socket&&a.owner.homeId===a.route.homeId
      &&a.session.homeId===a.route.homeId&&a.session.socket===a.socket
      &&a.sessions.isCurrent(a.session),"identity_changed");a.owner.assertLocalCurrent();}
  #send=async(method:string,params:Record<string,unknown>,remaining:number)=>{
    // Capture the dispatcher remainder BEFORE async currentness checks. It is
    // a countdown of that same command, not a renewed preparation clock.
    const original=originalTime(remaining);const a=this.actual;this.#current();
    await a.owner.assertCurrent();await a.registry.assertManagedHomeSocketMatchesRoute(a.socket,a.route);this.#current();
    return a.sessions.request(a.route.homeId,a.socket,method,params,
      Math.min(original.remaining(),a.setup.remaining(),a.driver.remaining()));
  };
  #dispatch(operation:HostOperation,key:string):Carrier {
    this.#current();const a=this.actual;
    const held:Carrier={key,operation,queries:[],unknown:false};this.#commands.push(held); // BEFORE dispatch.
    held.dispatch=a.registry.dispatchManagedHomeCommand(a.route,key,operation,this.#send);
    return held;
  }
  async #accepted(held:Carrier){try{held.acceptance=await held.dispatch;}catch{held.unknown=true;throw new Error("original dispatch unknown; retained without replay");}}
  async #observe(held:Carrier):Promise<any>{this.#current();const a=this.actual;
    check(held.queries.length<160,"expired"); // Same existing staging query ceiling.
    // ReceiptV1 observes this original mutation key. It cannot resubmit it.
    const pending=a.registry.dispatchManagedHomeCommand(a.route,held.key,{op:"ReceiptV1",lookup_command_key:held.key},this.#send);
    held.queries.push(pending);try{held.last=await pending;this.#current();return held.last;}
    catch{held.unknown=true;throw new Error("original observation unavailable; original owners retained");}}
  async #wait(){this.#current();await new Promise<void>(resolve=>setTimeout(resolve,500));this.#current();}
  async #stopAndObserve(start:Carrier,commandId:string):Promise<void>{
    const a=this.actual,fence=this.#fence!;
    check(typeof commandId==="string"&&/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(commandId),"identity_changed");
    const stop=this.#dispatch({op:"StopTaskV1",project_alias:a.route.projectAlias,lookup_command_key:start.key,command_id:commandId,...fence},this.input.stopCommandKey!);
    await this.#accepted(stop);
    for(;;){const d=await this.#observe(stop),r=d.receipt?.result;
      if(d.receipt?.state==="completed"){
        check(r?.phase==="stop_requested"&&r.project_alias===a.route.projectAlias&&r.run_id===fence.run_id&&r.run_epoch===fence.run_epoch&&r.attempt_id===fence.attempt_id&&r.stop_requested_observed===true,"identity_changed");break;
      }await this.#wait();
    }
    for(;;){const d=await this.#observe(start),o=d.task_observation;
      if(o){check(o.command_id===commandId&&o.run_id===fence.run_id&&o.run_epoch===fence.run_epoch&&o.attempt_id===fence.attempt_id,"identity_changed");
        check(o.uncertain_observed!==true,"effect_unknown");
        this.#lifecycle={enrollment_submitted_observed:o.enrollment_submitted_observed??false,
          enrollment_released_observed:o.enrollment_released_observed??false,helper_joined_observed:o.helper_joined_observed??null,
          stop_requested_observed:o.stop_requested_observed??null,validated_checkpoint_observed:o.validated_checkpoint_observed??null};
        if(o.enrollment_submitted_observed===true&&o.enrollment_released_observed===true&&o.helper_joined_observed===true&&o.stop_requested_observed===true&&o.validated_checkpoint_observed===true&&o.uncertain_observed===false){this.#lifecycleComplete=true;return;}
      }await this.#wait();
    }
  }
  runRetaining():Promise<void>{
    check(!this.#run,"effect_unknown");
    // Assignment precedes work. The original listener must also retain this
    // SAME promise and driver through success, refusal, timeout and cleanup.
    this.#run=Promise.resolve().then(async()=>{const a=this.actual,i=this.input;this.#current();
      await a.route.assertRoutableCurrent();this.#current();
      const selection={project_alias:a.route.projectAlias,backend:"codex" as const,model:i.model,profile_id:i.profileId};
      const open=this.#dispatch({op:"OpenPreparedProjectV1",...selection},i.openCommandKey);await this.#accepted(open);
      for(;;){const d=await this.#observe(open);if(d.receipt?.state==="completed"){
        check(d.receipt.result?.phase==="project_ready"&&d.receipt.result.project_alias===a.route.projectAlias,"identity_changed");break;}await this.#wait();}
      const start=this.#dispatch({op:"StartTaskV1",...selection,text:i.text},i.startCommandKey);await this.#accepted(start);
      for(;;){const d=await this.#observe(start),r=d.receipt?.result;
        if(d.receipt?.state==="completed"){
          check(r?.phase==="task_queued"&&r.project_alias===a.route.projectAlias,"identity_changed");
          check(typeof r.run_id==="string"&&/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(r.run_id)
            &&typeof r.run_epoch==="string"&&/^[1-9][0-9]{0,19}$/.test(r.run_epoch)&&BigInt(r.run_epoch)<=18446744073709551615n
            &&typeof r.attempt_id==="string"&&/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/.test(r.attempt_id),"identity_changed");
          const fence={run_id:r.run_id,run_epoch:r.run_epoch,attempt_id:r.attempt_id};
          if(this.#fence)check(JSON.stringify(this.#fence)===JSON.stringify(fence),"identity_changed");
          else this.#fence=Object.freeze(fence);
          const o=d.task_observation;
          if(o){check(o.run_id===fence.run_id&&o.run_epoch===fence.run_epoch&&o.attempt_id===fence.attempt_id,"identity_changed");
            check(o.uncertain_observed!==true,"effect_unknown");
            if(o.backend_ready_last_observed===true&&o.uncertain_observed===false){this.#readyObserved=true;
              if(!i.stopCommandKey)return;
              if(o.enrollment_submitted_observed===true){await this.#stopAndObserve(start,d.receipt.command_id);return;}
            }}
        }
        await this.#wait();
      }
    }).catch(()=>{this.#refused=true;throw new Error("positive ingress incomplete; same original owners retained");});
    return this.#run;
  }
  statusData(){return {scope:"positive-ingress-observation-only",commands:this.#commands.map(c=>({command_key:c.key,dispatch_unknown:c.unknown,receipt_queries:c.queries.length})),
    fence:this.#fence??null,backend_ready_last_observed:this.#readyObserved,refused:this.#refused,owners_retained:true,
    lifecycle_observation:this.#lifecycle??null,lifecycle_complete:this.#lifecycleComplete,
    enrollment_release_proved:this.#lifecycle?.enrollment_released_observed===true,
    original_helper_join_proved:this.#lifecycle?.helper_joined_observed===true,checkpoint_accepted:this.#lifecycle?.validated_checkpoint_observed===true,
    retirement_complete:false,qualified:false};}
}
