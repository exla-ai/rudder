import type {Server} from "node:http";
import {inspect} from "node:util";
import {check,fail,snapshot} from "./types.js";
/** Owns ONLY restriction of new listener admission. It does not destroy
 * upgraded sockets, cancel their requests or certify worker/TLS/server drain. */
export class StagingListenerAdmissionClosure {
 #attempted=false;#stopped=false;#callback=false;#error=false;#promise?:Promise<void>;
 constructor(private readonly server:Server){}
 [inspect.custom](){return "StagingListenerAdmissionClosure [live server and sockets retained]";}
 toJSON():never{fail("invalid_request");}
 closeRetaining():Promise<void>{
  check(!this.#attempted&&this.server.listening,"effect_unknown");this.#attempted=true;
  this.#promise=new Promise<void>((resolve,reject)=>{
   try{this.server.close(error=>{this.#callback=true;if(error)this.#error=true;});}
   catch{this.#error=true;reject(new Error("Listener restriction unknown"));return;}
   // Node closes the listening handle synchronously. The close callback can
   // await old connections, so it must NOT gate the next actual bind attempt.
   setImmediate(()=>{if(this.server.listening||this.#error){this.#error=true;reject(new Error("Listener still accepting"));}
    else{this.#stopped=true;resolve();}});
  });return this.#promise;
 }
 statusData(){return snapshot({new_listener_admission_stopped:this.#stopped,close_callback_observed:this.#callback,error:this.#error,existing_socket_drain_claim:false});}
}
