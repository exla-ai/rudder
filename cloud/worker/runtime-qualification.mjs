// One original opaque Root qualification exchange on the existing authenticated
// home WebSocket. This relay has no registration key or authority constructor.
import {canonicalData} from './host-control-core.mjs';
export const READY='runtime_image_qualification_ready_v1',ACK='runtime_image_qualification_ack_v1';
const LIMIT=16384;
const fields=['format_version','scope','qualification_id','qualification_nonce','image','image_receipt_sha256','source_manifest_sha256','inventory','manifest','installer_input','native_sha256','buddy_sha256','service_sha256','launcher_sha256'];
const plain=v=>v!==null&&typeof v==='object'&&!Array.isArray(v)&&[Object.prototype,null].includes(Object.getPrototypeOf(v));
const exact=(v,keys)=>plain(v)&&Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
const hex=v=>typeof v==='string'&&/^[0-9a-f]{64}$/.test(v);
const uuid=v=>typeof v==='string'&&/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(v);
const canonical=v=>JSON.stringify(canonicalData(v));
const bytes=v=>{const raw=Buffer.from(JSON.stringify(v));if(!raw.length||raw.length>LIMIT)throw Error('QUALIFICATION_BOUND');return raw;};
const end=(c,sdk)=>{if(!exact(c,['format_version','maximum_milliseconds','boottime_deadline_nanoseconds',...(sdk?['domain']:[])])||c.format_version!==1||c.maximum_milliseconds!==(sdk?240000:30000)||(sdk&&c.domain!=='compiler_sdk_qualification_v1')||typeof c.boottime_deadline_nanoseconds!=='string'||!/^[1-9][0-9]{0,19}$/.test(c.boottime_deadline_nanoseconds))throw Error('QUALIFICATION_CUTOFF');return BigInt(c.boottime_deadline_nanoseconds);};
export class RuntimeQualificationRelay {
  #scope;#scopeBytes;#sdk=false;#config;#now;#send;#emit;#attempt=null;
  constructor({scope,compilerScope,config,now,send,emit}) {
    this.#scope=compilerScope??scope??null;this.#sdk=compilerScope!==undefined&&compilerScope!==null;this.#config=config;this.#now=now;this.#send=send;this.#emit=emit;
    if(this.#scope!==null){const chosen=this.#scope;
      if(!exact(chosen,this.#sdk?[...fields,'sdk']:fields)||chosen.format_version!==1||chosen.scope!==(this.#sdk?'disposable-fixed-compiler-sdk-only':'disposable-runtime-floor-and-original-client-only')||!uuid(chosen.qualification_id)||!uuid(chosen.qualification_nonce))throw Error('QUALIFICATION_SCOPE');
      if(this.#sdk){const {sdk,...runtime}=chosen;runtime.scope='disposable-runtime-floor-and-original-client-only';if(!plain(sdk)||canonical(runtime)!==canonical(scope))throw Error('SDK_ORIGINAL_PURPOSE');}
      this.#scopeBytes=canonical(chosen);}
  }
  get scoped(){return this.#scope!==null;}
  forward(message,generation,registered){
    // Retain the original bytes BEFORE validation/send. Any second exchange or
    // unknown outcome refuses; no replacement generation can replay it.
    if(this.#attempt!==null)throw Error('QUALIFICATION_ORIGINAL_ALREADY_HELD');
    const held={raw:bytes(message),generation,sent:false,unknown:false,received:null};this.#attempt=held;
    try{
      if(!this.scoped||!registered||!generation||!generation.authorized||generation.closed||generation.lost
        ||!exact(message,['kind','rpc_id','packet'])||message.kind!==READY||!uuid(message.rpc_id)
        ||!exact(message.packet,['body','mac'])||!hex(message.packet.mac))throw Error('QUALIFICATION_ORIGINAL_SCOPE');
      const b=message.packet.body;
      if(!exact(b,['version','kind','qualification_id','qualification_nonce','registration_binding','root_nonce','registration_proof_sha256','scope','observation','selected_executables','remaining_milliseconds',...(this.#sdk?['sdk_proof']:[])])
        ||b.version!==1||b.kind!==(this.#sdk?'compiler_sdk_qualification_ready':'runtime_image_qualification_ready')||b.qualification_id!==this.#scope.qualification_id
        ||b.qualification_nonce!==this.#scope.qualification_nonce||canonical(b.scope)!==this.#scopeBytes||!uuid(b.root_nonce)
        ||!hex(b.registration_proof_sha256)||!plain(b.observation)||b.observation.request_nonce!==message.rpc_id
        ||!plain(b.registration_binding)||b.registration_binding.config_sha256!==this.#config.configSha
        ||b.registration_binding.home_id!==this.#config.home_id||b.registration_binding.incarnation!==this.#config.incarnation
        ||!Array.isArray(b.selected_executables)||b.selected_executables.length!==4
        ||!Number.isInteger(b.remaining_milliseconds)||b.remaining_milliseconds<1||b.remaining_milliseconds>(this.#sdk?240000:30000))throw Error('QUALIFICATION_BINDING');
      held.cutoff=end(b.observation.original_enable_constraint,this.#sdk);
      if(this.#now()>=held.cutoff)throw Error('QUALIFICATION_EXPIRED');
      this.#send(generation,{type:READY,rpc_id:message.rpc_id,packet:message.packet});held.sent=true;
    }catch(error){held.unknown=true;throw error;}
  }
  receive(message,generation){
    const held=this.#attempt;if(!held)throw Error('QUALIFICATION_UNSOLICITED_ACK');
    if(held.received!==null)throw Error('QUALIFICATION_DUPLICATE_ACK');
    held.received=bytes(message);
    try{
      const original=JSON.parse(held.raw.toString());
      if(held.unknown||!held.sent||generation!==held.generation||generation.closed||generation.lost
        ||!generation.authorized||this.#now()>=held.cutoff||!exact(message,['type','rpc_id','packet'])
        ||message.type!==ACK||message.rpc_id!==original.rpc_id||!exact(message.packet,['body','mac'])
        ||!plain(message.packet.body)||!hex(message.packet.mac))throw Error('QUALIFICATION_FOREIGN_OR_LATE_ACK');
      // Root, not this relay, verifies the registration-key MAC/exact ACK body.
      this.#emit({kind:ACK,rpc_id:message.rpc_id,packet:message.packet});
    }catch(error){held.unknown=true;throw error;}
  }
  retainedData(){const h=this.#attempt;return h?{attempted:true,sent:h.sent,unknown:h.unknown,
    requestBytes:h.raw.length,receivedBytes:h.received?.length??0}:null;}
}
