import type {ManagedHomeRegistry,ManagedHomeWorker} from "./registry.js";
import type {CommittedHostTransport} from "./transport.js";
import {check,key,canonical,hash,snapshot} from "./types.js";
export type RegistrationWireType="engine_registration_begin_v1"|"engine_registration_proof_v1"|"engine_operation_v1"|"host_transport_begin_v1"|"host_transport_commit_v1"|"host_transport_confirm_v1"|"host_transport_reconcile_v1";
export interface RegistrationWireMessage {type:RegistrationWireType;rpc_id:string;packet:unknown;}
/** New managed-only branch of the EXISTING home control websocket. An actual
 * authenticated worker capability is required; no legacy SQLite row creates it.
 * Replies/owners stay retained across a failed or unknown socket send. */
export class ManagedHomeRegistrationWire {
  #pending=new Map<string,{requestHash:string;reply?:unknown;sendAttempted:boolean;unknown:boolean}>();
  constructor(private readonly registry:ManagedHomeRegistry){}
  async receive(worker:ManagedHomeWorker,message:unknown,send:(reply:unknown)=>Promise<void>,socket?:object,onCommitted?:(owner:CommittedHostTransport)=>Promise<void>):Promise<boolean>{
    if(message===null||typeof message!=="object"||!("type" in message)||!["engine_registration_begin_v1","engine_registration_proof_v1","engine_operation_v1","host_transport_begin_v1","host_transport_commit_v1","host_transport_confirm_v1","host_transport_reconcile_v1"].includes(String(message.type)))return false;
    const data=snapshot(message,24*1024) as RegistrationWireMessage;check(Object.keys(data).every(k=>["type","rpc_id","packet"].includes(k)));key(data.rpc_id);
    const id=worker.homeId+":"+data.type+":"+data.rpc_id;check(!this.#pending.has(id)&&this.#pending.size<32,"effect_unknown");const held={requestHash:hash(canonical(data,24*1024)),reply:undefined as unknown,sendAttempted:false,unknown:false};this.#pending.set(id,held);
    try{await worker.assertCredentialsCurrent();
      if(data.type.startsWith("host_transport_")){check(socket,"identity_changed");
        check((data.packet as {body?:{attempt_nonce?:unknown}})?.body?.attempt_nonce===data.rpc_id,"identity_changed");
        if(data.type==="host_transport_confirm_v1"){await this.registry.confirmManagedHostTransport(worker,socket,data.packet);this.#pending.delete(id);return true;}
        if(data.type==="host_transport_reconcile_v1"){const replies=await this.registry.reconcileManagedHostTransport(worker,socket,data.packet);held.reply=replies;for(const packet of replies){const nonce=(packet as {body:{request_nonce:string}}).body.request_nonce;held.sendAttempted=true;await send({type:"engine_operation_ack_v1",rpc_id:nonce,packet});}this.#pending.delete(id);return true;}
        if(data.type==="host_transport_begin_v1"){const packet=await this.registry.beginManagedHostTransport(worker,socket,data.packet);held.reply={type:"host_transport_challenge_v1",rpc_id:data.rpc_id,packet};}
        else {const committed=await this.registry.commitManagedHostTransport(worker,socket,data.packet);check(onCommitted,"unconfigured");await onCommitted(committed.owner);held.reply={type:"host_transport_ack_v1",rpc_id:data.rpc_id,packet:committed.packet};}
        held.sendAttempted=true;await send(held.reply);this.#pending.delete(id);return true;
      }
      if(data.type==="engine_operation_v1"){if(socket)await this.registry.assertManagedHostSocketCurrent(worker,socket);check((data.packet as {body?:{request_nonce?:unknown}})?.body?.request_nonce===data.rpc_id,"identity_changed");}const packet=data.type==="engine_registration_begin_v1"?await this.registry.beginOriginalEngineRegistration(worker,data.packet):data.type==="engine_operation_v1"?await this.registry.performOriginalEngineOperation(worker,data.packet):(await this.registry.finishOriginalEngineRegistration(worker,data.packet)).acknowledgementData();
      held.reply={type:data.type==="engine_registration_begin_v1"?"engine_registration_challenge_v1":data.type==="engine_operation_v1"?"engine_operation_ack_v1":"engine_registration_ack_v1",rpc_id:data.rpc_id,packet};
      held.sendAttempted=true;await send(held.reply);this.#pending.delete(id);
    }catch{held.unknown=true;/* original mutation result/unknown send stays retained; no replay */
      // These two fixed methods perform ONLY receipt bookkeeping/unchanged
      // ACK reads. They own no effect/capability and cannot refresh its clock.
      // Their failure must not prevent a later read of an original operation.
      if(data.type==="host_transport_confirm_v1"||data.type==="host_transport_reconcile_v1")this.#pending.delete(id);
    }
    return true;
  }
}
