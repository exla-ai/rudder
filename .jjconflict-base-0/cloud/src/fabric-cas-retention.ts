/** Batch-only IO custody, never storage/tenant/Root authority. Legacy CAS is
 * unchanged outside run(). A timeout abort cannot mark an SDK request joined. */
import {AsyncLocalStorage} from "node:async_hooks";
const context=new AsyncLocalStorage<RetainedFabricIO>();
export const currentRetainedFabricIO=()=>context.getStore();
export class RetainedFabricIO {
 #dispatchChecks:(()=>void)[]=[];
 #requests=new Map<Promise<unknown>,boolean>();#streams:{body:unknown;closed:boolean}[]=[];#notify:()=>void=()=>{};#controllers=new Set<AbortController>();#cancelled=false;
 run<T>(action:()=>Promise<T>):Promise<T>{return context.run(this,action);}
 onProgress(notify:()=>void){this.#notify=notify;}
 get cancelled(){return this.#cancelled;}
 /** Tightening only: checks accumulate and cannot replace/remove an older gate.
  * They do not qualify storage or construct a staging/admission owner. */
 tightenDispatch(check:()=>void){if(this.#dispatchChecks.length>=4)throw Error("Retained dispatch guards are full");this.#dispatchChecks.push(check);}
 assertDispatch(){if(this.#cancelled)throw Error("Retained dispatch is cancelled");for(const check of this.#dispatchChecks)check();}
 controller(value:AbortController){this.#controllers.add(value);if(this.#cancelled)value.abort();}
 cancel(){this.#cancelled=true;for(const controller of this.#controllers)controller.abort();}
 request<T>(promise:Promise<T>){this.#requests.set(promise,false);promise.then(()=>this.#done(promise),()=>this.#done(promise));}
 #done(p:Promise<unknown>){this.#requests.set(p,true);this.#notify();}
 stream(body:unknown){const b=body as {closed?:boolean;once?:(event:string,listener:()=>void)=>unknown};const owner={body,closed:b?.closed===true};this.#streams.push(owner);
  if(!owner.closed&&typeof b?.once==="function")b.once("close",()=>{owner.closed=true;this.#notify();});
  // An unsupported stream stays actually held and unresolved. Absence of a
  // close API/event is not cleanup, successful abort or a synthetic join.
 }
 get settled(){return [...this.#requests.values()].every(Boolean)&&this.#streams.every(s=>s.closed);}
}
