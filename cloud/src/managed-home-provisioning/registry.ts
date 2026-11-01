import {RootCompilerSdkQualifications,assertCompilerSdkQualificationService,type CompilerQualificationPurpose} from "./compiler-qualification.js";
import {authenticateCompilerQualificationReady,validateCompilerQualificationScope,COMPILER_QUALIFICATION_SCOPE,type CompilerQualificationCandidate,type CompilerQualificationScope} from "./compiler-qualification-protocol.js";
import {RootRuntimeImageQualifications,RuntimeQualificationCutoff,assertRuntimeImageQualificationService,type RuntimeQualificationPurpose} from "./runtime-qualification.js";
import {authenticateRuntimeQualificationReady,validateRuntimeQualificationCandidate,RUNTIME_QUALIFICATION_SCOPE,type RuntimeQualificationCandidate,type RuntimeQualificationScope} from "./runtime-qualification-protocol.js";
import type {OriginalDisposableMachineCustody} from "./fly.js";
import {managedHomeProtectedConfigData} from "./protected-config.js";
import {RootHostTransports} from "./transport.js";
import {isOwnerRenew} from "./renewals.js";
import {isProviderHeartbeat} from "./provider-owner-heartbeats.js";
import {isImageOperation} from "./storage-protocol.js";
import type {ManagedFreshStorage} from "./storage.js";
import type {IncomingMessage} from "node:http";
import {storageReference} from "./storage-protocol.js";
import { createCipheriv, createDecipheriv, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import { inspect } from "node:util";
import { ManagedHomePgStore } from "./store.js";
import { FlyManagedHomeProvider, originalTime, type OriginalTime } from "./fly.js";
import {reserveFreshProjects,freshSpec,type FreshAssignment} from "./fresh.js";
import {RootEngineRegistration,type Envelope,type RegistrationChallenge,type RegistrationAck,type CommittedRegistration} from "./registration.js";
import {RootEngineOperations,type EngineOperationAck} from "./operations.js";
import { check, fail, id, key, canonical, hash, snapshot, validateSpec, validateOperation, controlOrigin, runtimeMountConfigData, EFFECTS,
  nativeReply,nativeStatusReply,type Intent,type HomeSpec,type ManagedHomePolicy,type Effect,type EffectAck,type BoundHome,type ProvisionReceipt,type MachineAck,type VolumeAck,type AppAck,type HostOperation } from "./types.js";

export interface RuntimeQualificationProvisionReceipt {provision:ProvisionReceipt;scope:RuntimeQualificationScope;compilerScope?:CompilerQualificationScope;}
export type ManagedCommandSend=(method:string,params:Record<string,unknown>,remainingMs:number)=>Promise<unknown>;
interface ProvisionCarrier {qualification?:{sdkReservationAttempted?:boolean;sdkPurpose?:CompilerQualificationPurpose;sdkReservation?:Promise<CompilerQualificationPurpose>;registrationCutoff?:RuntimeQualificationCutoff;reservationAttempted:boolean;purpose?:RuntimeQualificationPurpose;reservation?:Promise<RuntimeQualificationPurpose>;custody?:OriginalDisposableMachineCustody};intent:Intent;time:OriginalTime;cipher:Buffer;token:string;registrationKey:string;registrationCipher:Buffer;busy:boolean;held?:{effect:Effect;promise:Promise<EffectAck>;ack?:EffectAck;failed:boolean};}
interface CommandCarrier {intent:Intent;time:OriginalTime;key:string;operation:HostOperation;busy:boolean;started:boolean;promise?:Promise<unknown>;rawReply?:unknown;reply?:unknown;replied:boolean;failed:boolean;}
const ISSUER=Symbol("actual managed-home registry");
class ManagedHomeRoute {
  #current:()=>Promise<void>;#intent:Intent;#machine:MachineAck;
  private constructor(issuer:symbol,intent:Intent,machine:MachineAck,readonly projectAlias:string,current:()=>Promise<void>){check(issuer===ISSUER);this.#intent=intent;this.#machine=machine;this.#current=current;}
  static issue(issuer:symbol,i:Intent,m:MachineAck,alias:string,current:()=>Promise<void>){return new ManagedHomeRoute(issuer,i,m,alias,current);}
  get accountId(){return this.#intent.accountId;}get homeId(){return this.#intent.homeId;}get appName(){return this.#intent.appName;}
  get networkName(){return this.#intent.networkName;}get machineId(){return this.#machine.id;}get incarnation(){return this.#intent.incarnation;}
  get storeId(){return this.#intent.storeId;}get epoch(){return this.#intent.epoch;}
  async assertRoutableCurrent():Promise<void>{await this.#current();}
  [inspect.custom](){return "ManagedHomeRoute [registry-bound routing only]";}
  toJSON():never{fail("invalid_request");}
}
export type {ManagedHomeRoute};
class ManagedHomeWorker {
  #bound:BoundHome;#current:()=>Promise<void>;
  private constructor(issuer:symbol,bound:BoundHome,current:()=>Promise<void>){check(issuer===ISSUER);this.#bound=bound;this.#current=current;}
  static issue(issuer:symbol,bound:BoundHome,current:()=>Promise<void>){return new ManagedHomeWorker(issuer,bound,current);}
  get accountId(){return this.#bound.intent.accountId;}get homeId(){return this.#bound.intent.homeId;}
  get machineId(){return (this.#bound.acks.configure as MachineAck).id;}get storeId(){return this.#bound.intent.storeId;}
  get incarnation(){return this.#bound.intent.incarnation;}get epoch(){return this.#bound.intent.epoch;}
  async assertCredentialsCurrent(){await this.#current();}
  [inspect.custom](){return "ManagedHomeWorker [credential binding only]";}
  toJSON():never{fail("invalid_request");}
}
export type {ManagedHomeWorker};
class FreshManagedProjectGrant {
  #assignment:FreshAssignment;#current:()=>Promise<void>;
  private constructor(issuer:symbol,a:FreshAssignment,current:()=>Promise<void>){check(issuer===ISSUER);this.#assignment=a;this.#current=current;}
  static issue(issuer:symbol,a:FreshAssignment,current:()=>Promise<void>){return new FreshManagedProjectGrant(issuer,a,current);}
  get projectId(){return this.#assignment.projectId;}get ownerEpoch(){return this.#assignment.ownerEpoch;}
  get engineInstance(){return this.#assignment.engineInstance;}get bootId(){return this.#assignment.bootId;}
  async assertOriginalCurrent(){await this.#current();}
  [inspect.custom](){return "FreshManagedProjectGrant [actual PG assignment]";}
  toJSON():never{fail("invalid_request");}
}
export type {FreshManagedProjectGrant};
class RegisteredEngineAdmission {
  #ack:Envelope<RegistrationAck>;#current:()=>Promise<void>;
  private constructor(issuer:symbol,ack:Envelope<RegistrationAck>,current:()=>Promise<void>){check(issuer===ISSUER);this.#ack=ack;this.#current=current;}
  static issue(issuer:symbol,ack:Envelope<RegistrationAck>,current:()=>Promise<void>){return new RegisteredEngineAdmission(issuer,ack,current);}
  acknowledgementData(){return snapshot(this.#ack);}
  async assertOriginalCurrent(){await this.#current();}
  [inspect.custom](){return "RegisteredEngineAdmission [engine registration only]";}
  toJSON():never{fail("invalid_request");}
}
export type {RegisteredEngineAdmission};
export class ManagedHomeRegistry {
  #processOwner=randomUUID();#provisions=new Map<string,ProvisionCarrier>();#commands=new Map<string,CommandCarrier>();
  #preparing=new Set<string>();
  #routes=new WeakMap<ManagedHomeRoute,BoundHome>();#key:Buffer;
  #workers=new WeakMap<ManagedHomeWorker,BoundHome>();
  #sockets=new WeakMap<object,ManagedHomeWorker>();
  #grants=new WeakMap<ManagedHomeRoute,FreshManagedProjectGrant>();
  #registrations=new Map<string,{owner:RegisteredEngineAdmission;original:CommittedRegistration}>();
  #registrationOps=new Map<string,{time:OriginalTime;busy:boolean}>();
  constructor(private readonly store?:ManagedHomePgStore,private readonly provider?:FlyManagedHomeProvider,private readonly policy?:ManagedHomePolicy,keyMaterial?:Buffer,private readonly registration?:RootEngineRegistration,private readonly engineOperations?:RootEngineOperations,private readonly freshStorage?:ManagedFreshStorage,private readonly transports?:RootHostTransports,private readonly runtimeQualifications?:RootRuntimeImageQualifications,private readonly qualificationCandidate?:RuntimeQualificationCandidate,private readonly compilerQualifications?:RootCompilerSdkQualifications,private readonly compilerCandidate?:CompilerQualificationCandidate){this.#key=keyMaterial?Buffer.from(keyMaterial):Buffer.alloc(0);}
  [inspect.custom](){return "ManagedHomeRegistry [private retained provisioning/command owners]";}
  toJSON():never{fail("invalid_request");}
  #configured(){check(this.store&&this.provider&&this.policy&&this.#key.length===32,"unconfigured");controlOrigin(this.policy);return {store:this.store,provider:this.provider,policy:this.policy};}
  #receipt(home:BoundHome):ProvisionReceipt{return {homeId:home.intent.homeId,commandKey:home.intent.commandKey,epoch:home.intent.epoch,phase:home.phase,projects:home.intent.freshProjects.map(p=>({projectId:p.projectId,workspaceId:p.workspaceId,title:p.title,state:"reserved"})),nativeIngressReady:false,taskReady:false};}
  #cipher(value:string,i:Intent,purpose:"token"|"registration"):Buffer{const nonce=randomBytes(12);const cipher=createCipheriv("aes-256-gcm",this.#key,nonce);cipher.setAAD(canonical({account:i.accountId,home:i.homeId,nonce:i.nonce,purpose}));return Buffer.concat([nonce,cipher.update(value,"utf8"),cipher.final(),cipher.getAuthTag()]);}
  #registrationKey(cipher:Buffer,i:Intent):Buffer{try{check(cipher.length>=29&&cipher.length<=512);const d=createDecipheriv("aes-256-gcm",this.#key,cipher.subarray(0,12));d.setAAD(canonical({account:i.accountId,home:i.homeId,nonce:i.nonce,purpose:"registration"}));d.setAuthTag(cipher.subarray(-16));const clear=Buffer.concat([d.update(cipher.subarray(12,-16)),d.final()]);try{check(clear.length===65&&/^[a-f0-9]{64}\n$/.test(clear.toString("utf8")),"identity_changed");return Buffer.from(clear.subarray(0,64).toString("utf8"),"hex");}finally{clear.fill(0);}}catch{fail("identity_changed");}}
  #hostConfig(i:Intent,machine:MachineAck):Buffer{return managedHomeProtectedConfigData(i,machine);}
  /** Ordinary provision uses only a published exact-inventory role. */
  async provisionManagedHome(accountId:string,commandKey:string,projectKeys:readonly string[]):Promise<ProvisionReceipt>{check(!this.qualificationCandidate,"unconfigured");return this.#provision(accountId,commandKey,projectKeys,false);}
  /** Operator-only isolated factory. No caller paths/project IDs, live home
   * adoption or task route. One inert NEW namespace exists for config schema
   * compatibility; all source/task/general operations independently refuse. */
  async provisionRuntimeImageQualification(accountId:string,commandKey:string):Promise<RuntimeQualificationProvisionReceipt>{
    check(this.qualificationCandidate,"unconfigured");assertRuntimeImageQualificationService(this.runtimeQualifications);const provision=await this.#provision(accountId,commandKey,["Runtime floor qualification"],true);
    const c=this.#provisions.get(accountId+"\0"+commandKey);check(c?.qualification?.purpose&&c.intent.runtimeQualification,"effect_unknown");
    return {provision,scope:snapshot(c.intent.runtimeQualification),...(c.intent.compilerQualification?{compilerScope:snapshot(c.intent.compilerQualification)}:{})};
  }
  #qualificationCarrier(i:Intent){const c=this.#provisions.get(i.accountId+"\0"+i.commandKey);check(c?.intent.nonce===i.nonce&&c.qualification?.purpose,"identity_changed");return c;}
  async #admitIntent(i:Intent,time:OriginalTime){if(i.runtimeQualification){const c=this.#qualificationCarrier(i);assertRuntimeImageQualificationService(this.runtimeQualifications);await this.runtimeQualifications.assertPurposeCurrent(c.qualification!.purpose!,i,time);}
    else if(runtimeMountConfigData(i.policy)){assertRuntimeImageQualificationService(this.runtimeQualifications);await this.runtimeQualifications.assertPublishedRole(i.policy,time);}
    if(i.compilerQualification){const q=this.#qualificationCarrier(i).qualification!;assertCompilerSdkQualificationService(this.compilerQualifications);check(q.sdkPurpose,"effect_unknown");await this.compilerQualifications.assertPurposeCurrent(q.sdkPurpose,i,time);}
    else if(i.policy.compilerSdk){assertCompilerSdkQualificationService(this.compilerQualifications);await this.compilerQualifications.assertPublishedRole(i.policy,time);}}
  #ordinary(i:Intent){check(!i.runtimeQualification&&!i.compilerQualification&&!this.qualificationCandidate,"unconfigured");}
  /** Only the explicit disposable staging flow keeps known ordinary provider
   * originals past routable. This retains no new Root/Source/deletion grant. */
  #retainStagingOrdinaryOriginal(i:Intent):boolean {
    const p=this.policy;return !!p&&!this.qualificationCandidate&&!this.compilerCandidate&&!i.runtimeQualification&&!i.compilerQualification
      &&p.controlPlane?.profile==="qualification-v1"&&p.sdkInitialSource==="sdk-fixed-suite-v1"&&p.ownerMode==="provider-fenced-v1"
      &&i.processOwner===this.#processOwner&&canonical(i.policy).equals(canonical(p));
  }
  async #provision(accountId:string,commandKey:string,projectKeys:readonly string[],qualification:boolean):Promise<ProvisionReceipt>{
    const {store,policy}=this.#configured();const requestedTime=originalTime(300_000);if(!qualification&&runtimeMountConfigData(policy)){assertRuntimeImageQualificationService(this.runtimeQualifications);await this.runtimeQualifications.assertPublishedRole(policy,requestedTime);}if(!qualification&&policy.compilerSdk){assertCompilerSdkQualificationService(this.compilerQualifications);await this.compilerQualifications.assertPublishedRole(policy,requestedTime);}id(accountId);key(commandKey);check(Array.isArray(projectKeys)&&projectKeys.length>0&&projectKeys.length<=8);check(new Set(projectKeys).size===projectKeys.length);
    const requestHash=hash(canonical({accountId,commandKey,projectKeys,policy,...(qualification?{qualification_candidate:this.qualificationCandidate,...(this.compilerCandidate?{compiler_candidate:this.compilerCandidate}:{})}:{})}));const mapKey=accountId+"\0"+commandKey;
    let carrier=this.#provisions.get(mapKey);
    if(!carrier){check(!this.#preparing.has(mapKey)&&this.#provisions.size+this.#preparing.size<16,"busy");this.#preparing.add(mapKey);try{const existing=await store.lookupKey(accountId,commandKey);if(existing){check(existing.intent.requestHash===requestHash,"conflict");return this.#receipt(existing);}
      const time=requestedTime;const nonce=randomUUID();
      const identities={homeId:"mh-"+randomUUID(),storeId:randomUUID(),incarnation:randomUUID()};
      // Actual fresh-project reservation is committed WITH the home intent.
      // Config paths come only from these new UUIDs, never a caller repo/path.
      const freshProjects=reserveFreshProjects(projectKeys);const spec=freshSpec(freshProjects,identities.storeId,policy);time.remaining();
      const intent:Intent={authorityId:store.authorityId,accountId,homeId:identities.homeId,storeId:identities.storeId,incarnation:identities.incarnation,
        commandKey,requestHash,epoch:"1",nonce,appName:"rudder-mh-"+nonce.replaceAll("-",""),networkName:"rudder-mhn-"+randomUUID().replaceAll("-",""),processOwner:this.#processOwner,policy:snapshot(policy),spec,freshProjects,
        ...(qualification?{runtimeQualification:{format_version:1 as const,scope:RUNTIME_QUALIFICATION_SCOPE,qualification_id:randomUUID(),qualification_nonce:randomUUID(),...validateRuntimeQualificationCandidate(this.qualificationCandidate,policy)}}:{})};
      if(qualification&&this.compilerCandidate){check(intent.runtimeQualification,"unconfigured");intent.compilerQualification=validateCompilerQualificationScope({...intent.runtimeQualification,scope:COMPILER_QUALIFICATION_SCOPE,sdk:this.compilerCandidate.sdk},policy);}
      const token="rdrmh_"+randomBytes(32).toString("hex"),registrationKey=randomBytes(32).toString("hex")+"\n";carrier={intent,time,cipher:Buffer.alloc(0),token,registrationKey,registrationCipher:Buffer.alloc(0),busy:false,...(qualification?{qualification:{reservationAttempted:false}}:{})};carrier.cipher=this.#cipher(token,intent,"token");carrier.registrationCipher=this.#cipher(registrationKey,intent,"registration");
      // Retain before first database commit; an unknown commit cannot throw
      // away this original generated intent/token or create a replacement.
      const raced=this.#provisions.get(mapKey);if(raced){check(raced.intent.requestHash===requestHash,"conflict");carrier=raced;}else this.#provisions.set(mapKey,carrier);
      }finally{this.#preparing.delete(mapKey);}
    }
    check(carrier.intent.requestHash===requestHash,"conflict");check(!carrier.busy,"busy");carrier.busy=true;
    try{if(carrier.qualification&&!carrier.qualification.purpose){const q=carrier.qualification;check(!q.reservationAttempted&&this.runtimeQualifications,"effect_unknown");q.reservationAttempted=true;assertRuntimeImageQualificationService(this.runtimeQualifications);
        q.reservation=this.runtimeQualifications.reserveRetaining(carrier.intent,carrier.time);q.purpose=await q.reservation;}
      if(carrier.intent.compilerQualification){const q=carrier.qualification!;assertCompilerSdkQualificationService(this.compilerQualifications);
        if(!q.sdkPurpose){check(!q.sdkReservationAttempted&&q.purpose,"effect_unknown");q.sdkReservationAttempted=true;q.sdkReservation=this.compilerQualifications.reserveRetaining(carrier.intent,carrier.time,q.purpose);q.sdkPurpose=await q.sdkReservation;}}
      await this.#admitIntent(carrier.intent,carrier.time);const stored=await store.reserve(carrier.intent,carrier.cipher,hash(carrier.token),carrier.registrationCipher);
      // Another process may have won reservation. Its durable original owner
      // is returned, never adopted for new provider effects by this process.
      if(stored.intent.nonce!==carrier.intent.nonce){check(carrier.held===undefined,"effect_unknown");this.#provisions.delete(mapKey);return this.#receipt(stored);}
      const receipt=await this.#driveProvision(carrier,false);if(receipt.phase==="routable"){if(carrier.qualification){const home=await store.bound(carrier.intent.accountId,carrier.intent.homeId);if(!carrier.qualification.custody)carrier.qualification.custody=this.provider!.captureOriginalDisposableMachine(home);}else if(!this.#retainStagingOrdinaryOriginal(carrier.intent)){this.provider!.archiveKnownCompleted(carrier.intent);this.#provisions.delete(mapKey);}}return receipt;
    }finally{carrier.busy=false;}
  }
  #captureQualificationCreation(c:ProvisionCarrier,bound:BoundHome,effect:Effect,ack:EffectAck){
    if(c.qualification&&effect==="start"&&!c.qualification.custody){
      // Actual original provider ACK is held before durable routable admission.
      // This creates ONLY original-resource custody; no row/Root readiness is
      // inferred. An uncertain following COMMIT retains both originals.
      const original:BoundHome={...bound,phase:"routable",acks:{...bound.acks,start:ack}};
      c.qualification.custody=this.provider!.captureOriginalDisposableMachine(original);
    }
  }
  async #driveProvision(c:ProvisionCarrier,reconcile:boolean):Promise<ProvisionReceipt>{const {store,provider}=this.#configured();let bound=await store.bound(c.intent.accountId,c.intent.homeId);
    check(bound.intent.processOwner===this.#processOwner&&bound.intent.nonce===c.intent.nonce,"effect_unknown");
    if(c.held){if(!reconcile)fail("effect_unknown");check(!c.held.failed,"effect_unknown");
      const ack=c.held.ack??await c.held.promise;c.held.ack=ack;this.#captureQualificationCreation(c,bound,c.held.effect,ack);await store.originalAck(c.intent,c.held.effect,ack);c.held=undefined;bound=await store.bound(c.intent.accountId,c.intent.homeId);}
    if(bound.phase==="routable")return this.#receipt(bound);
    for(const effect of EFFECTS){if(bound.acks[effect])continue;c.time.remaining();await this.#admitIntent(c.intent,c.time);
      // DB Started must be KNOWN committed before calling Fly. An uncertain
      // Started commit never results in a provider call or blind replay.
      let privateValue:string|undefined;
      if(effect==="token_secret")privateValue=c.token;
      if(effect==="registration_secret")privateValue=c.registrationKey;
      if(effect==="config_secret"){const machine=bound.acks.machine as MachineAck;check(machine,"identity_changed");privateValue=this.#hostConfig(c.intent,machine).toString();}
      await store.start(c.intent,effect,provider.describe(effect,c.intent,bound.acks,privateValue));c.time.remaining();
      const retained={effect,promise:undefined as unknown as Promise<EffectAck>,ack:undefined as EffectAck|undefined,failed:false};c.held=retained;
      // Staged outside both promise and catch. No request retry, even if abort,
      // DB write, disconnect, error or original deadline creates uncertainty.
      retained.promise=provider.execute(effect,c.intent,bound.acks,privateValue,c.time).then(ack=>{retained.ack=ack;return ack;},()=>{retained.failed=true;throw new Error("Managed home provider effect unknown");});
      try{const ack=await retained.promise;this.#captureQualificationCreation(c,bound,effect,ack);await store.originalAck(c.intent,effect,ack);c.held=undefined;bound=await store.bound(c.intent.accountId,c.intent.homeId);}
      catch{await store.unknown(c.intent,effect).catch(()=>{});fail("effect_unknown");}
    }
    return this.#receipt(bound);
  }
  async reconcileProvisioning(accountId:string,commandKey:string):Promise<ProvisionReceipt>{const {store}=this.#configured();id(accountId);key(commandKey);const c=this.#provisions.get(accountId+"\0"+commandKey);
    if(!c){const saved=await store.lookupKey(accountId,commandKey);check(saved,"not_found");return this.#receipt(saved);}check(!c.busy,"busy");c.busy=true;try{const receipt=await this.#driveProvision(c,true);if(receipt.phase==="routable"){if(c.qualification){if(!c.qualification.custody)c.qualification.custody=this.provider!.captureOriginalDisposableMachine(await store.bound(c.intent.accountId,c.intent.homeId));}else{this.provider!.archiveKnownCompleted(c.intent);this.#provisions.delete(accountId+"\0"+commandKey);}}return receipt;}finally{c.busy=false;}}
  async getProvisioningReceipt(accountId:string,commandKey:string):Promise<ProvisionReceipt>{const {store}=this.#configured();id(accountId);key(commandKey);const original=await store.lookupKey(accountId,commandKey);check(original,"not_found");return this.#receipt(original);}
  async requireManagedHomeRepo(accountId:string,workspaceId:string){const {store}=this.#configured();check(!this.qualificationCandidate,"unconfigured");id(accountId);key(workspaceId);return store.repo(accountId,workspaceId);}
  async #current(original:BoundHome,time:OriginalTime=originalTime(5000)):Promise<void>{await this.#admitIntent(original.intent,time);const {store,provider}=this.#configured();const now=await store.bound(original.intent.accountId,original.intent.homeId);
    check(now.phase==="routable"&&hash(canonical(now.intent))===hash(canonical(original.intent))&&canonical(now.acks).equals(canonical(original.acks)),"identity_changed");
    time.remaining();await provider.observeApp(now.intent,now.acks.app as AppAck,time);await provider.observeMachine(now.intent,now.acks.start as MachineAck,now.acks.volume as VolumeAck,true,time);
  }
  async #grant(bound:BoundHome,workspaceId:string):Promise<FreshManagedProjectGrant>{this.#ordinary(bound.intent);const {store}=this.#configured();const assignment=await store.currentAssignment(bound.intent,workspaceId);return FreshManagedProjectGrant.issue(ISSUER,assignment,async()=>{await this.#current(bound);const now=await store.currentAssignment(bound.intent,workspaceId);check(canonical(now).equals(canonical(assignment)),"identity_changed");});}
  async #route(bound:BoundHome,workspaceId:string):Promise<ManagedHomeRoute>{check(bound.phase==="routable"&&bound.intent.spec.projects.some(p=>p.alias===workspaceId),"effect_unknown");const grant=await this.#grant(bound,workspaceId);
    const route=ManagedHomeRoute.issue(ISSUER,snapshot(bound.intent),snapshot(bound.acks.start as MachineAck),workspaceId,()=>grant.assertOriginalCurrent());this.#routes.set(route,bound);this.#grants.set(route,grant);await route.assertRoutableCurrent();return route;}
  async resolveManagedHomeRoute(accountId:string,homeId:string,workspaceId:string):Promise<ManagedHomeRoute>{const {store}=this.#configured();id(accountId);key(homeId);key(workspaceId);const identity=await store.repo(accountId,workspaceId);check(identity.home.id===homeId,"not_found");return this.#route(await store.bound(accountId,homeId),workspaceId);}
  async authenticateManagedHomeWorker(homeId:string,bearer:string):Promise<ManagedHomeWorker>{const {store}=this.#configured();key(homeId);check(typeof bearer==="string"&&/^rdrmh_[a-f0-9]{64}$/.test(bearer),"not_found");const digest=hash(bearer);const bound=await store.worker(homeId);await this.#admitIntent(bound.intent,originalTime(5000));
    check(timingSafeEqual(Buffer.from(digest,"hex"),Buffer.from(bound.tokenHash,"hex"))&&bound.acks.configure,"not_found");
    // The actual configured child can open its socket before the original
    // start HTTP ACK. Credentials may attach then; task routing stays gated.
    const worker=ManagedHomeWorker.issue(ISSUER,bound,async()=>{const now=await store.bound(bound.intent.accountId,homeId);check(now.intent.nonce===bound.intent.nonce&&now.tokenHash===bound.tokenHash&&canonical(now.acks.configure).equals(canonical(bound.acks.configure)),"identity_changed");});this.#workers.set(worker,bound);return worker;}
  async assertManagedHomeWorkerMatchesRoute(worker:ManagedHomeWorker,route:ManagedHomeRoute):Promise<void>{const held=this.#workers.get(worker),home=this.#original(route);check(held&&held.intent.nonce===home.intent.nonce&&held.intent.accountId===home.intent.accountId&&held.intent.homeId===home.intent.homeId,"identity_changed");await worker.assertCredentialsCurrent();await route.assertRoutableCurrent();}
  /** Pure binding to the ACTUAL upgrade socket after credential authentication.
   * No JSON session ID, worker-state message or legacy token can create it. */
  bindAuthenticatedManagedHomeSocket(socket:object,worker:ManagedHomeWorker):void{check(this.#workers.has(worker)&&socket!==null&&typeof socket==="object"&&!this.#sockets.has(socket),"identity_changed");this.#sockets.set(socket,worker);}
  async assertManagedHomeSocketMatchesRoute(socket:object,route:ManagedHomeRoute):Promise<void>{const worker=this.#sockets.get(socket);check(worker&&this.transports,"identity_changed");await this.transports.assertSocketCurrent(route.homeId,socket);await this.assertManagedHomeWorkerMatchesRoute(worker,route);}
  async #transportHome(worker:ManagedHomeWorker,socket:object,time:OriginalTime){check(this.transports&&this.#sockets.get(socket)===worker,"identity_changed");return this.#committedRenewalHome(worker,time);}
  async beginManagedHostTransport(worker:ManagedHomeWorker,socket:object,input:unknown){const raw=snapshot(input,16384) as {body?:{remaining_milliseconds?:unknown}};const n=raw.body?.remaining_milliseconds;check(typeof n==="number"&&Number.isInteger(n)&&n>=1&&n<=5000);const time=originalTime(n),home=await this.#transportHome(worker,socket,time);const {store}=this.#configured();const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{return await this.transports!.begin(home,key,input,socket,time);}finally{key.fill(0);}}
  async commitManagedHostTransport(worker:ManagedHomeWorker,socket:object,input:unknown){const time=originalTime(5000),home=await this.#transportHome(worker,socket,time);const {store}=this.#configured();const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{return await this.transports!.commit(home,key,input,socket);}finally{key.fill(0);}}
  async confirmManagedHostTransport(worker:ManagedHomeWorker,socket:object,input:unknown){const time=originalTime(5000),home=await this.#transportHome(worker,socket,time);const {store}=this.#configured();const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{return await this.transports!.confirm(home,key,input);}finally{key.fill(0);}}
  async reconcileManagedHostTransport(worker:ManagedHomeWorker,socket:object,input:unknown){const time=originalTime(5000),home=await this.#transportHome(worker,socket,time);const {store}=this.#configured();const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{return await this.transports!.reconcile(home,key,input,socket);}finally{key.fill(0);}}
  async assertManagedHostSocketCurrent(worker:ManagedHomeWorker,socket:object){check(this.transports&&this.#sockets.get(socket)===worker,"identity_changed");await this.transports.assertSocketCurrent(worker.homeId,socket);await worker.assertCredentialsCurrent();}
  closeManagedHostSocket(socket:object){this.transports?.closed(socket);}
  async #registrationHome(worker:ManagedHomeWorker,time:OriginalTime):Promise<BoundHome>{const {store}=this.#configured();check(this.registration,"unconfigured");const held=this.#workers.get(worker);check(held,"identity_changed");time.remaining();await worker.assertCredentialsCurrent();await this.#admitIntent(held.intent,time);
    for(;;){const current=await store.bound(held.intent.accountId,held.intent.homeId);check(current.intent.nonce===held.intent.nonce,"identity_changed");if(current.phase==="routable"){await this.#current(current,time);return current;}check(current.phase==="pending"||current.phase==="resource_provisioned","effect_unknown");await new Promise<void>(resolve=>setTimeout(resolve,Math.min(100,time.remaining())));}}
  #registrationOperation(worker:ManagedHomeWorker){check(this.#workers.has(worker),"identity_changed");let actual=this.#registrationOps.get(worker.homeId);if(!actual){check(this.#registrationOps.size<32,"busy");actual={time:originalTime(30_000),busy:false};this.#registrationOps.set(worker.homeId,actual);}check(!actual.busy,"busy");actual.time.remaining();
    const held=this.#workers.get(worker)!;if(held.intent.runtimeQualification){const q=this.#qualificationCarrier(held.intent).qualification!;
      if(!q.registrationCutoff)q.registrationCutoff=new RuntimeQualificationCutoff(actual.time);
      check(q.registrationCutoff.matchesOriginal(actual.time),"identity_changed");}
    actual.busy=true;return actual;}
  async beginOriginalEngineRegistration(worker:ManagedHomeWorker,input:unknown):Promise<Envelope<RegistrationChallenge>>{const {store}=this.#configured();const op=this.#registrationOperation(worker);try{const home=await this.#registrationHome(worker,op.time);const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{op.time.remaining();return await this.registration!.begin(home,key,input,hash(this.#hostConfig(home.intent,home.acks.machine as MachineAck)),op.time);}finally{key.fill(0);}}finally{op.busy=false;}}
  async finishOriginalEngineRegistration(worker:ManagedHomeWorker,input:unknown):Promise<RegisteredEngineAdmission>{const {store}=this.#configured();const op=this.#registrationOperation(worker);try{const home=await this.#registrationHome(worker,op.time);check(this.#registrations.size<32||this.#registrations.has(home.intent.homeId),"busy");const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);
    try{const actual=await this.registration!.finish(home,key,input,hash(this.#hostConfig(home.intent,home.acks.machine as MachineAck)),op.time);const previous=this.#registrations.get(home.intent.homeId);if(previous){check(previous.original.proofSha256===actual.proofSha256,"conflict");op.time.remaining();return previous.owner;}
      const owner=RegisteredEngineAdmission.issue(ISSUER,actual.ack,async()=>{await this.#current(home);await this.registration!.assertOriginalCurrent(home,actual.proofSha256);});this.#registrations.set(home.intent.homeId,{owner,original:actual});op.time.remaining();return owner;
    }finally{key.fill(0);}}finally{op.busy=false;}}
  async publishRuntimeImageQualification(worker:ManagedHomeWorker,socket:object,input:unknown){
    const raw=snapshot(input,24*1024) as {body?:{remaining_milliseconds?:unknown}};const n=raw.body?.remaining_milliseconds;
    check(typeof n==="number"&&Number.isInteger(n)&&n>=1&&n<=30_000);
    // Capture before ANY awaits; keep the original initial registration clock
    // as an extra conservative cap. Its challenge/ACK preceded Root enable, so
    // this can only end earlier than enable30, never renew a delayed proof.
    const knownWorker=this.#workers.get(worker);check(knownWorker,"identity_changed");
    const retained=this.#qualificationCarrier(knownWorker.intent).qualification!,registrationClock=this.#registrationOps.get(worker.homeId);
    const cutoff=retained.registrationCutoff;
    check(registrationClock&&cutoff&&cutoff.matchesOriginal(registrationClock.time),"identity_changed");
    const time=cutoff.receiptWindow(n);
    const {store}=this.#configured();assertRuntimeImageQualificationService(this.runtimeQualifications);check(this.transports&&this.#sockets.get(socket)===worker,"identity_changed");
    const transport=this.transports.retainCurrentSocketOwner(worker.homeId,socket);await transport.assertCurrent();const home=await this.#committedRenewalHome(worker,time);
    const c=this.#qualificationCarrier(home.intent),q=c.qualification!;check(q.custody&&q.purpose,"effect_unknown");
    await this.runtimeQualifications.assertPurposeCurrent(q.purpose,home.intent,time);
    const registration=this.#registrations.get(home.intent.homeId);check(registration,"identity_changed");
    await this.registration!.assertOriginalCurrent(home,registration.original.proofSha256);
    const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);
    try{time.remaining();if(home.intent.compilerQualification){assertCompilerSdkQualificationService(this.compilerQualifications);check(q.sdkPurpose,"effect_unknown");
      const request=authenticateCompilerQualificationReady(input,key,q.sdkPurpose.scope,registration.original.ack.body.binding,registration.original.proofSha256);
      check(request.body.root_nonce===registration.original.ack.body.root_nonce,"identity_changed");await this.transports.assertSocketCurrent(worker.homeId,socket);time.remaining();
      return await this.compilerQualifications.publishRetaining(q.sdkPurpose,q.custody,home,registration.original,transport,key,request,time);
    }const request=authenticateRuntimeQualificationReady(input,key,q.purpose.scope,registration.original.ack.body.binding,registration.original.proofSha256);
      check(request.body.root_nonce===registration.original.ack.body.root_nonce,"identity_changed");
      await this.transports.assertSocketCurrent(worker.homeId,socket);time.remaining();
      return await this.runtimeQualifications.publishRetaining(q.purpose,q.custody,home,registration.original,transport,key,request,time);
    }finally{key.fill(0);}
  }
  /** Private operator recovery DATA, available after deadline/unknown. Never
   * restores a creation/Root/channel owner or authorizes a numeric PID action. */
  runtimeImageQualificationCustodyData(accountId:string,commandKey:string):unknown {
    id(accountId);key(commandKey);const c=this.#provisions.get(accountId+"\0"+commandKey);check(c?.qualification&&c.intent.runtimeQualification,"not_found");const i=c.intent;
    return snapshot({scope:"retained-disposable-provider-response-data-only",home_id:i.homeId,app_name:i.appName,network_name:i.networkName,provision_nonce:i.nonce,process_owner:i.processOwner,
      qualification:i.runtimeQualification,original_creation_captured:!!c.qualification.custody,
      pending_effect:c.held?{effect:c.held.effect,failed:c.held.failed,observed_ack:c.held.ack??null}:null,
      responses:this.provider!.originalDisposableResponseData(i)});
  }
  /** Private recovery DATA, never teardown/Root/Source authority. Redacts
   * complete MachineAck.config, every token, key/cipher and protected config. */
  async stagingOrdinaryProvisionCustodyData(accountId:string,commandKey:string){
    id(accountId);key(commandKey);const c=this.#provisions.get(accountId+"\0"+commandKey);
    check(c&&!c.qualification&&this.#retainStagingOrdinaryOriginal(c.intent),"not_found");
    const {store}=this.#configured(),bound=await store.lookupKey(accountId,commandKey),i=c.intent;
    // Held original required even for known durable rows. Foreign/reopened
    // rows may provide no custody; a lookup is DATA, never an owner constructor.
    if(bound)check(canonical(bound.intent).equals(canonical(i))&&bound.tokenHash===hash(c.token),"identity_changed");
    const a=bound?.acks,app=a?.app as AppAck|undefined,volume=a?.volume as VolumeAck|undefined;
    const machine=(m:MachineAck|undefined)=>m?{id:m.id,instance_id:m.instanceId,name:m.name,region:m.region,state:m.state,config_sha256:m.configSha256}:null;
    return snapshot({scope:"retained-original-ordinary-provision-recovery-data-only",home_id:i.homeId,app_name:i.appName,network_name:i.networkName,provision_nonce:i.nonce,process_owner:i.processOwner,
      app:app?{id:app.id,name:app.name,network:app.network,organization:app.organization}:null,
      volume:volume?{id:volume.id,name:volume.name,region:volume.region}:null,
      machine:machine(a?.machine as MachineAck|undefined),configured:machine(a?.configure as MachineAck|undefined),started:machine(a?.start as MachineAck|undefined),
      pending_effect:c.held?{effect:c.held.effect,failed:c.held.failed,original_ack_observed:!!c.held.ack}:null,owners_retained:true,teardown_authority:false});
  }
  /** Opaque original publisher fact only; no receipt/SQL/JSON adoption. */
  takeOriginalRoleActivationPermit(accountId:string,commandKey:string){
    id(accountId);key(commandKey);const c=this.#provisions.get(accountId+"\0"+commandKey);
    check(c?.qualification,"not_found");
    if(this.compilerCandidate){assertCompilerSdkQualificationService(this.compilerQualifications);
      check(c.intent.compilerQualification,"identity_changed");
      return this.compilerQualifications.takeOriginalRoleActivationPermit(c.intent.compilerQualification.qualification_id);}
    assertRuntimeImageQualificationService(this.runtimeQualifications);check(c.intent.runtimeQualification,"identity_changed");
    return this.runtimeQualifications.takeOriginalRoleActivationPermit(c.intent.runtimeQualification.qualification_id);
  }
  async runtimeImageQualificationResultData(accountId:string,qualificationId:string){id(accountId);key(qualificationId);if(this.compilerCandidate){assertCompilerSdkQualificationService(this.compilerQualifications);return this.compilerQualifications.historicalResultData(accountId,qualificationId);}assertRuntimeImageQualificationService(this.runtimeQualifications);return this.runtimeQualifications.historicalResultData(accountId,qualificationId);}
  async performOriginalEngineOperation(worker:ManagedHomeWorker,input:unknown):Promise<Envelope<EngineOperationAck>>{const {store}=this.#configured();check(this.engineOperations,"unconfigured");const originalWorker=this.#workers.get(worker);check(originalWorker,"identity_changed");this.#ordinary(originalWorker.intent);const raw=snapshot(input,24*1024) as {body?:{remaining_milliseconds?:unknown;operation?:{op:string}}};const n=raw.body?.remaining_milliseconds;check(typeof n==="number"&&Number.isInteger(n)&&n>=1&&n<=(raw.body?.operation&&isImageOperation(raw.body.operation)?300000:30000));const time=originalTime(n);
    const operation=raw.body?.operation??{op:""};const home=(isOwnerRenew(operation)||isProviderHeartbeat(operation))?await this.#committedRenewalHome(worker,time):await this.#registrationHome(worker,time);time.remaining();const key=this.#registrationKey(await store.registrationCipher(home.intent),home.intent);try{return await this.engineOperations.perform(home,key,input,time);}finally{key.fill(0);}}
  /** Existing enrolled owner heartbeat, never a provider rebind/adoption. The
   * actual Root HMAC+registered credential and PG owner are checked next; no
   * new machine/source authority is inferred from this committed binding. */
  async #committedRenewalHome(worker:ManagedHomeWorker,time:OriginalTime):Promise<BoundHome>{const {store}=this.#configured();check(this.registration,"unconfigured");const held=this.#workers.get(worker);check(held,"identity_changed");time.remaining();await worker.assertCredentialsCurrent();const current=await store.bound(held.intent.accountId,held.intent.homeId);check(current.phase==="routable"&&current.intent.nonce===held.intent.nonce&&current.tokenHash===held.tokenHash&&canonical(current.acks.configure).equals(canonical(held.acks.configure)),"identity_changed");time.remaining();return current;}
  async managedStorageMissing(worker:ManagedHomeWorker,transferId:string,objects:unknown[]){this.#configured();check(this.freshStorage,"unconfigured");const home=this.#workers.get(worker);check(home,"identity_changed");this.#ordinary(home.intent);await worker.assertCredentialsCurrent();return this.freshStorage.missing(home,transferId,objects);}
  async managedStoragePut(worker:ManagedHomeWorker,transferId:string,object:unknown,req:IncomingMessage){this.#configured();check(this.freshStorage,"unconfigured");const home=this.#workers.get(worker);check(home,"identity_changed");this.#ordinary(home.intent);await worker.assertCredentialsCurrent();return this.freshStorage.put(home,transferId,storageReference(object),req);}
  async managedImageBatchPut(worker:ManagedHomeWorker,transferId:string,batchId:string,req:IncomingMessage){this.#configured();check(this.freshStorage,"unconfigured");const home=this.#workers.get(worker);check(home,"identity_changed");this.#ordinary(home.intent);await worker.assertCredentialsCurrent();return this.freshStorage.putImageBatch(home,transferId,batchId,req);}
  async reconcileManagedImageBatch(worker:ManagedHomeWorker,transferId:string,batchId:string,input:unknown){this.#configured();check(this.freshStorage,"unconfigured");const home=this.#workers.get(worker);check(home,"identity_changed");this.#ordinary(home.intent);await worker.assertCredentialsCurrent();return this.freshStorage.reconcileImageBatch(home,transferId,batchId,input);}
  #original(route:ManagedHomeRoute):BoundHome{const home=this.#routes.get(route);check(home,"identity_changed");return home;}
  async dispatchManagedHomeCommand(route:ManagedHomeRoute,commandKey:string,input:HostOperation,send:ManagedCommandSend):Promise<unknown>{const {store}=this.#configured();key(commandKey);const operation=validateOperation(input);const home=this.#original(route);this.#ordinary(home.intent);
    const grant=this.#grants.get(route);check(grant,"unconfigured");await grant.assertOriginalCurrent();
    if(operation.op==="ReceiptV1"){await route.assertRoutableCurrent();const known=await store.command(home.intent,operation.lookup_command_key);check(known&&known.operation.op!=="ReceiptV1"&&known.operation.project_alias===route.projectAlias,"not_found");
      // Receipt queries create NO accepted-command row or task/prep budget.
      // This is only a bounded transport observation of an existing key.
      const time=originalTime(20_000);try{return nativeStatusReply(await send("managed.receiptV1",{command_key:commandKey,lookup_command_key:operation.lookup_command_key},time.remaining()),operation.lookup_command_key,known.operation);}catch{fail("effect_unknown");}}
    check(operation.project_alias===route.projectAlias,"not_found");if(operation.op==="BuildCurrentSourceSdkV1")check(home.intent.policy.compilerSdk,"unconfigured");const mapKey=route.accountId+"\0"+route.homeId+"\0"+commandKey;let c=this.#commands.get(mapKey);
    if(c)check(canonical(c.operation).equals(canonical(operation)),"conflict");
    else{check(this.#commands.size<32,"busy");c={intent:home.intent,time:originalTime(20_000),key:commandKey,operation,busy:false,started:false,replied:false,failed:false};this.#commands.set(mapKey,c);}
    check(!c.busy,"busy");c.busy=true;try{await route.assertRoutableCurrent();c.time.remaining();const stored=await store.reserveCommand(c.intent,commandKey,operation,this.#processOwner);
      if(stored.phase==="replied")return snapshot(stored.reply);
      check(stored.processOwner===this.#processOwner&&stored.phase==="accepted"&&!c.started,"effect_unknown");
      await store.startCommand(c.intent,commandKey,this.#processOwner);c.started=true;c.time.remaining();
      const {op,...params}=operation;const method=op==="BuildCurrentSourceSdkV1"?"managed.buildCurrentSourceSdkV1":op==="StartTaskV1"?"managed.startTaskV1":"managed.openPreparedProjectV1";
      // The actual same homeRpc promise/reply remains in this carrier. A
      // local HTTP timeout or DB uncertainty never creates another send.
      c.promise=Promise.resolve().then(()=>send(method,{command_key:commandKey,...params},c!.time.remaining())).then(reply=>{c!.rawReply=reply;c!.reply=nativeReply(reply,commandKey,operation);c!.replied=true;return c!.reply;},()=>{c!.failed=true;throw new Error("Managed home command effect unknown");});
      try{await c.promise;await store.commandReply(c.intent,commandKey,this.#processOwner,c.reply);const reply=snapshot(c.reply);this.#commands.delete(mapKey);return reply;}
      catch{await store.commandUnknown(c.intent,commandKey,this.#processOwner).catch(()=>{});fail("effect_unknown");}
    }finally{c.busy=false;}}
  async reconcileManagedHomeCommand(route:ManagedHomeRoute,commandKey:string):Promise<unknown>{const {store}=this.#configured();key(commandKey);const home=this.#original(route);this.#ordinary(home.intent);const stored=await store.command(home.intent,commandKey);check(stored&&stored.operation.op!=="ReceiptV1"&&stored.operation.project_alias===route.projectAlias,"not_found");if(stored.phase==="replied")return snapshot(stored.reply);
    const c=this.#commands.get(route.accountId+"\0"+route.homeId+"\0"+commandKey);check(c&&c.started&&!c.failed&&c.promise,"effect_unknown");check(!c.busy,"busy");c.busy=true;
    try{await c.promise;check(c.replied,"effect_unknown");await store.commandReply(c.intent,commandKey,this.#processOwner,c.reply);const reply=snapshot(c.reply);this.#commands.delete(route.accountId+"\0"+route.homeId+"\0"+commandKey);return reply;}finally{c.busy=false;}}
  /** No stop/delete/restart path. Closing storage is refused while this process
   * owns any provisioning/dispatch carrier; supervision/recovery is explicit. */
  async close():Promise<void>{await this.freshStorage?.closeImmutableBatchAdmissionRetaining();this.freshStorage?.assertImageBatchesIdleForClose();check(!this.runtimeQualifications?.hasRetainedOwners()&&!this.compilerQualifications?.hasRetainedOwners(),"effect_unknown");check(this.#provisions.size===0&&this.#preparing.size===0&&this.#commands.size===0&&this.#registrations.size===0&&this.#registrationOps.size===0,"effect_unknown");await this.store?.close();await this.registration?.close();this.#key.fill(0);}
}
