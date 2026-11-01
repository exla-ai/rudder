import {createHmac} from "node:crypto";
import {AuthorityPool,SCHEMA,type Transaction} from "../project-authority/pool.js";
import type {TrustedContext} from "../project-authority/types.js";
import type {ManagedFreshStorage} from "./storage.js";
import type {EngineOperationAck,EngineOperationRequest,EngineOperationError} from "./operations.js";
import type {Envelope} from "./registration.js";
import type {OriginalTime} from "./fly.js";
import {RENEWAL_MIGRATION_SHA256} from "./renewal-schema.js";
import {check,exact,key,uuid,canonical,hash,snapshot,fail,ManagedHomeError,type BoundHome} from "./types.js";
export interface OwnerRenewOperation {op:"RenewFreshOwnerV1";workspace_id:string;owner_epoch:string;lease_id:string;request_id:string;expected_serial:string;}
export function isOwnerRenew(v:{op:string}):v is OwnerRenewOperation{return v.op==="RenewFreshOwnerV1";}
export function validateOwnerRenew(value:unknown):OwnerRenewOperation{exact(value,["op","workspace_id","owner_epoch","lease_id","request_id","expected_serial"]);const v=value as unknown as OwnerRenewOperation;check(v.op==="RenewFreshOwnerV1");key(v.workspace_id);uuid(v.lease_id);uuid(v.request_id);for(const [s,positive]of [[v.owner_epoch,true],[v.expected_serial,false]]as const)check(typeof s==="string"&&/^(0|[1-9][0-9]{0,18})$/.test(s)&&BigInt(s)<=9223372036854775807n&&(!positive||s!=="0"));return v;}
interface Held {request:EngineOperationRequest;context:TrustedContext;credential:Buffer;busy:boolean;started:boolean;result?:unknown;ack?:Envelope<EngineOperationAck>;}
/** Uses the actual Root authentication producer in operations.ts. Dedicated
 * bounded-active ownership prevents renewals exhausting its64 lifetime slots.
 * Unknown requests remain held/Started; no retry changes serial/key/deadline.
 */
export class RootOwnerRenewals {
 #held=new Map<string,Held>();
 constructor(private readonly pool:AuthorityPool,private readonly authorityId:string,private readonly storage:ManagedFreshStorage){}
 async #run<T>(account:string,body:(tx:Transaction)=>Promise<T>):Promise<T>{let known:ManagedHomeError|undefined;try{const r=await this.pool.run(account,async tx=>{try{return await body(tx);}catch(e){if(e instanceof ManagedHomeError)known=e;throw e;}});check(r.status==="committed","effect_unknown");return r.value;}catch{if(known)throw known;fail("effect_unknown");}}
 async assertClockCurrent(home:BoundHome):Promise<void>{const policy=home.intent.policy.ownerLeaseSafety;check(policy,"unconfigured");await this.#run(home.intent.accountId,async tx=>{const row=await tx.query(`SELECT state,policy_sha256,migration_sha256 FROM ${SCHEMA}.managed_owner_clock_installation WHERE authority_id=$1`,[this.authorityId]);check(row.rows[0]?.state==="ready"&&row.rows[0].policy_sha256===hash(canonical(policy))&&row.rows[0].migration_sha256===RENEWAL_MIGRATION_SHA256,"unconfigured");});}
 async perform(home:BoundHome,context:TrustedContext,request:EngineOperationRequest,credential:Buffer,time:OriginalTime):Promise<Envelope<EngineOperationAck>>{
  const op=validateOwnerRenew(request.operation),id=home.intent.accountId+":"+home.intent.homeId+":"+op.request_id;let held=this.#held.get(id);
  if(held){check(hash(canonical(held.request))===hash(canonical(request)),"conflict");check(!held.busy&&held.ack,"effect_unknown");await this.#save(home,held);const ack=snapshot(held.ack);held.credential.fill(0);this.#held.delete(id);return ack;}
  check(this.#held.size<32,"busy");held={request:snapshot(request),context:snapshot(context),credential:Buffer.from(credential),busy:true,started:false};this.#held.set(id,held);
  try{time.remaining();const old=await this.#run(home.intent.accountId,async tx=>{
   const policy=home.intent.policy.ownerLeaseSafety;check(policy,"unconfigured");const admission=await tx.query(`SELECT state,policy_sha256,migration_sha256 FROM ${SCHEMA}.managed_owner_clock_installation WHERE authority_id=$1`,[this.authorityId]);check(admission.rows[0]?.state==="ready"&&admission.rows[0].migration_sha256===RENEWAL_MIGRATION_SHA256&&admission.rows[0].policy_sha256===hash(canonical(policy)),"unconfigured");
   const project=home.intent.spec.projects.find(p=>p.alias===op.workspace_id);check(project,"identity_changed");const prior=await tx.query(`SELECT request_sha256,state,ack_json FROM ${SCHEMA}.managed_owner_renewals WHERE authority_id=$1 AND account_id=$2 AND home_id=$3 AND request_id=$4`,[this.authorityId,home.intent.accountId,home.intent.homeId,op.request_id]);if(prior.rows.length){check(prior.rows[0].request_sha256===hash(canonical(request)),"conflict");check(prior.rows[0].state==="replied","effect_unknown");return prior.rows[0].ack_json as Envelope<EngineOperationAck>;}
   await tx.query("SELECT pg_advisory_xact_lock(hashtextextended($1,44))",[this.authorityId+":"+home.intent.accountId+":"+project.prepare.namespace.project_id]);
   const current=await tx.query(`SELECT p.owner_epoch::text,p.lease_id::text,p.renew_serial::text,p.owner_engine_instance::text,clock_timestamp()<p.lease_until AS live FROM ${SCHEMA}.projects p JOIN ${SCHEMA}.managed_home_fresh_projects f USING(authority_id,account_id,project_id) WHERE p.authority_id=$1 AND p.account_id=$2 AND p.project_id=$3 AND f.home_id=$4 AND f.workspace_id=$5 AND f.origin='fresh-v4-empty' AND p.mode='ready' FOR UPDATE OF p`,[this.authorityId,home.intent.accountId,project.prepare.namespace.project_id,home.intent.homeId,op.workspace_id]);
   check(current.rows.length===1&&current.rows[0].live&&current.rows[0].owner_epoch===op.owner_epoch&&current.rows[0].lease_id===op.lease_id&&current.rows[0].renew_serial===op.expected_serial&&current.rows[0].owner_engine_instance===context.engine?.instanceId,"identity_changed");time.remaining();
   await tx.query(`INSERT INTO ${SCHEMA}.managed_owner_renewals VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'started',NULL)`,[this.authorityId,home.intent.accountId,home.intent.homeId,project.prepare.namespace.project_id,op.request_id,request.request_nonce,op.owner_epoch,op.lease_id,hash(canonical(request)),JSON.stringify(request)]);time.remaining();return undefined;});
   if(old){held.credential.fill(0);this.#held.delete(id);return snapshot(old);}held.started=true;time.remaining();let error:EngineOperationError|null=null;
   try{held.result=await this.storage.renewOriginalFreshOwner(home,context,op);check((held.result as {status?:string}).status==="committed","effect_unknown");time.remaining();}
   catch(e){const code=e instanceof ManagedHomeError?e.code:"effect_unknown";if(code==="effect_unknown"||code==="unavailable")fail("effect_unknown");error=code==="expired"?"expired":code==="unconfigured"?"unconfigured":code==="busy"?"busy":"rejected";}
   const {kind,remaining_milliseconds,...common}=request;const project=home.intent.spec.projects.find(p=>p.alias===op.workspace_id)!;
   const body:EngineOperationAck={...common,kind:"engine_operation_ack",request_sha256:hash(canonical(request)),error,result:error?null:{scope:"fresh-owner-renewal-only",state:"renewed",project_id:project.prepare.namespace.project_id,workspace_id:op.workspace_id,request_id:op.request_id,owner_grant:(held.result as {value:unknown}).value}};
   held.ack={body,mac:createHmac("sha256",held.credential).update("rudder_engine_operation_ack_v1\0").update(canonical(body,24*1024)).digest("hex")};await this.#save(home,held);held.credential.fill(0);this.#held.delete(id);return snapshot(held.ack);
  }finally{held.busy=false;}
 }
 async #save(home:BoundHome,held:Held){check(held.started&&held.ack,"effect_unknown");const op=held.request.operation as OwnerRenewOperation;await this.#run(home.intent.accountId,async tx=>{const r=await tx.query(`UPDATE ${SCHEMA}.managed_owner_renewals SET state='replied',ack_json=$5 WHERE authority_id=$1 AND account_id=$2 AND home_id=$3 AND request_id=$4 AND request_sha256=$6 RETURNING request_id`,[this.authorityId,home.intent.accountId,home.intent.homeId,op.request_id,JSON.stringify(held.ack),hash(canonical(held.request))]);check(r.rowCount===1,"effect_unknown");});}
}
