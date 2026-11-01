import {createHmac} from "node:crypto";
import {AuthorityPool,SCHEMA,type Transaction} from "../project-authority/pool.js";
import {PROVIDER_OWNERSHIP_MIGRATION_SHA256} from "../project-authority/provider-ownership-schema.js";
import type {TrustedContext} from "../project-authority/types.js";
import type {ManagedFreshStorage} from "./storage.js";
import type {EngineOperationRequest,EngineOperationAck,EngineOperationError} from "./operations.js";
import type {Envelope} from "./registration.js";
import type {OriginalTime} from "./fly.js";
import {check,exact,key,uuid,canonical,hash,snapshot,fail,ManagedHomeError,type BoundHome} from "./types.js";
export interface ProviderHeartbeatOperation{op:"HeartbeatProviderOwnerV1";workspace_id:string;owner_epoch:string;ownership_id:string;request_id:string;expected_serial:string;}
export function isProviderHeartbeat(value:{op:string}):value is ProviderHeartbeatOperation{return value.op==="HeartbeatProviderOwnerV1";}
export function validateProviderHeartbeat(value:unknown):ProviderHeartbeatOperation{exact(value,["op","workspace_id","owner_epoch","ownership_id","request_id","expected_serial"]);const v=value as unknown as ProviderHeartbeatOperation;check(v.op==="HeartbeatProviderOwnerV1");key(v.workspace_id);uuid(v.ownership_id);uuid(v.request_id);for(const [s,positive]of[[v.owner_epoch,true],[v.expected_serial,false]]as const)check(typeof s==="string"&&/^(0|[1-9][0-9]{0,18})$/.test(s)&&BigInt(s)<=9223372036854775807n&&(!positive||s!=="0"));check(v.owner_epoch==="2");return v;}
interface Held{request:EngineOperationRequest;credential:Buffer;started:boolean;busy:boolean;result?:unknown;ack?:Envelope<EngineOperationAck>;}
/** Bounded SAME-owner liveness only. Neither server wall time nor missed
 * heartbeat frees ownership. The Root verifier supplies the actual context. */
export class RootProviderHeartbeats{
 #held=new Map<string,Held>();
 constructor(private readonly pool:AuthorityPool,private readonly authorityId:string,private readonly storage:ManagedFreshStorage){}
 async #run<T>(account:string,body:(tx:Transaction)=>Promise<T>):Promise<T>{let known:ManagedHomeError|undefined;try{const r=await this.pool.run(account,async tx=>{try{return await body(tx);}catch(e){if(e instanceof ManagedHomeError)known=e;throw e;}});check(r.status==="committed","effect_unknown");return r.value;}catch{if(known)throw known;fail("effect_unknown");}}
 async perform(home:BoundHome,ctx:TrustedContext,request:EngineOperationRequest,credential:Buffer,time:OriginalTime):Promise<Envelope<EngineOperationAck>>{
  check(home.intent.policy.ownerMode==="provider-fenced-v1"&&home.intent.policy.ownerLeaseSafety===undefined,"identity_changed");const op=validateProviderHeartbeat(request.operation),id=home.intent.accountId+":"+home.intent.homeId+":"+op.request_id;let held=this.#held.get(id);
  if(held){check(hash(canonical(held.request))===hash(canonical(request)),"conflict");check(!held.busy&&held.ack,"effect_unknown");await this.#save(home,held);const ack=snapshot(held.ack);held.credential.fill(0);this.#held.delete(id);return ack;}
  check(this.#held.size<32,"busy");held={request:snapshot(request),credential:Buffer.from(credential),started:false,busy:true};this.#held.set(id,held);
  try{time.remaining();const prior=await this.#run(home.intent.accountId,async tx=>{const migration=await tx.query(`SELECT authority_id,sha256 FROM ${SCHEMA}.provider_ownership_migrations WHERE version=1`);check(migration.rows[0]?.authority_id===this.authorityId&&migration.rows[0].sha256===PROVIDER_OWNERSHIP_MIGRATION_SHA256,"unconfigured");const project=home.intent.spec.projects.find(p=>p.alias===op.workspace_id);check(project,"identity_changed");
   const old=await tx.query(`SELECT request_sha256,state,ack_json FROM ${SCHEMA}.managed_provider_owner_heartbeats WHERE authority_id=$1 AND account_id=$2 AND home_id=$3 AND request_id=$4`,[this.authorityId,home.intent.accountId,home.intent.homeId,op.request_id]);if(old.rows.length){check(old.rows[0].request_sha256===hash(canonical(request)),"conflict");check(old.rows[0].state==="replied","effect_unknown");return old.rows[0].ack_json as Envelope<EngineOperationAck>;}
   const current=await tx.query(`SELECT 1 FROM ${SCHEMA}.projects WHERE authority_id=$1 AND account_id=$2 AND project_id=$3 AND ownership_mode='provider-fenced-v1' AND mode='ready' AND owner_epoch=$4 AND ownership_id=$5 AND owner_engine_instance=$6 AND provider_heartbeat_serial=$7 FOR UPDATE`,[this.authorityId,home.intent.accountId,project.prepare.namespace.project_id,op.owner_epoch,op.ownership_id,ctx.engine!.instanceId,op.expected_serial]);check(current.rows.length===1,"identity_changed");time.remaining();
   await tx.query(`INSERT INTO ${SCHEMA}.managed_provider_owner_heartbeats VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'started',NULL)`,[this.authorityId,home.intent.accountId,home.intent.homeId,project.prepare.namespace.project_id,op.request_id,request.request_nonce,op.ownership_id,op.owner_epoch,hash(canonical(request)),JSON.stringify(request)]);return undefined;});
   if(prior){held.credential.fill(0);this.#held.delete(id);return snapshot(prior);}held.started=true;time.remaining();let error:EngineOperationError|null=null;
   try{held.result=await this.storage.heartbeatOriginalProviderOwner(home,ctx,op);check((held.result as {status?:string}).status==="committed","effect_unknown");time.remaining();}catch(e){const code=e instanceof ManagedHomeError?e.code:"effect_unknown";if(code==="effect_unknown"||code==="unavailable")fail("effect_unknown");error=code==="expired"?"expired":code==="busy"?"busy":code==="unconfigured"?"unconfigured":"rejected";}
   const {kind,remaining_milliseconds,...common}=request;const project=home.intent.spec.projects.find(p=>p.alias===op.workspace_id)!;
   const body:EngineOperationAck={...common,kind:"engine_operation_ack",request_sha256:hash(canonical(request)),error,result:error?null:{scope:"provider-owner-heartbeat-only",state:"acknowledged",project_id:project.prepare.namespace.project_id,workspace_id:op.workspace_id,request_id:op.request_id,owner_grant:(held.result as {value:unknown}).value}};
   held.ack={body,mac:createHmac("sha256",held.credential).update("rudder_engine_operation_ack_v1\0").update(canonical(body,24*1024)).digest("hex")};await this.#save(home,held);const ack=snapshot(held.ack);held.credential.fill(0);this.#held.delete(id);return ack;
  }finally{held.busy=false;}
 }
 async #save(home:BoundHome,held:Held){check(held.started&&held.ack,"effect_unknown");const op=held.request.operation as ProviderHeartbeatOperation;await this.#run(home.intent.accountId,async tx=>{const r=await tx.query(`UPDATE ${SCHEMA}.managed_provider_owner_heartbeats SET state='replied',ack_json=$5 WHERE authority_id=$1 AND account_id=$2 AND home_id=$3 AND request_id=$4 AND request_sha256=$6 RETURNING request_id`,[this.authorityId,home.intent.accountId,home.intent.homeId,op.request_id,JSON.stringify(held.ack),hash(canonical(held.request))]);check(r.rowCount===1,"effect_unknown");});}
}
