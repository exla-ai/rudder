import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { inspect } from "node:util";
import { FLY_MANAGED_HOME_LIFECYCLE,FLY_MANAGED_HOME_LIFECYCLE_LIMITS,type FlyManagedHomeLifecycleProfile,createFlyMachineLeaseSession, type FlyCustodyTransport, type FlyCustodyRequest, type FlyCustodyReply, type FlyMachineLeaseSession } from "../fly-custody.js";
import { check, fail, canonical, hash, snapshot, id, CONFIG_SECRET, TOKEN_SECRET, REGISTRATION_SECRET, type Intent, type Effect, type EffectAck, type AppAck, type VolumeAck, type MachineAck, type BoundHome } from "./types.js";

export interface OriginalTime {readonly end:number;remaining():number;}
export function originalTime(milliseconds:number):OriginalTime {check(Number.isInteger(milliseconds)&&milliseconds>0&&milliseconds<=300_000);const startPerf=performance.now(),startWall=Date.now();
  const boot=()=>{if(process.platform!=="linux")return undefined;const raw=readFileSync("/proc/uptime","utf8"),m=/^(\d+)\.(\d{2}) /.exec(raw);check(m,"unavailable");const n=Number(m[1])*1000+Number(m[2])*10;check(Number.isSafeInteger(n)&&n>=0,"unavailable");return n;};
  const startBoot=boot(),end=startPerf+milliseconds;let lastPerf=startPerf,lastWall=startWall,lastBoot=startBoot,fenced=false;
  return {end,remaining(){if(fenced)fail("expired");const perf=performance.now(),wall=Date.now(),nowBoot=boot();if(perf<lastPerf||wall<lastWall||nowBoot!==undefined&&lastBoot!==undefined&&nowBoot<lastBoot){fenced=true;fail("expired");}lastPerf=perf;lastWall=wall;lastBoot=nowBoot;
    // procfs truncates hundredths: start at its lower endpoint and compare the
    // later upper endpoint, conservatively including suspend/queue time.
    const n=Math.floor(Math.min(end-perf,startWall+milliseconds-wall,...(startBoot!==undefined&&nowBoot!==undefined?[startBoot+milliseconds-(nowBoot+10)]:[])));if(n<=0){fenced=true;fail("expired");}return n;}};}
export interface ProviderReply {status:number;body:unknown;}
/** Internal injection seam for pure tests. Public factory supplies only the
 * fixed authenticated HTTP implementation; no HTTP body can choose transport. */
export type ProviderRequestContext=Readonly<{signal:AbortSignal;lifecycleProfile?:FlyManagedHomeLifecycleProfile}>;
export interface ProviderTransport {request(method:"GET"|"POST"|"DELETE",path:string,body:unknown,time:OriginalTime,headers?:Record<string,string>,context?:ProviderRequestContext):Promise<ProviderReply>;}
export { FlyHttpTransport } from "./fly-http-retention.js";
import { FlyHttpTransport } from "./fly-http-retention.js";

function record(v:unknown):Record<string,any>{check(v!==null&&typeof v==="object"&&!Array.isArray(v),"identity_changed");return v as Record<string,any>;}
function text(v:unknown):string{id(v);return v;}
// File.secret_name resolves a secret containing BASE64 of the exact file
// bytes. Registry callers supply the ORIGINAL UTF8 config/token/key text.
// Share this encoder between durable describe hash and actual HTTP dispatch;
// never trim/parse/reformat or guess that an already-base64-looking key is encoded.
function secretFileBody(value:unknown):{value:string}{check(typeof value==="string"&&value.length>0);return {value:Buffer.from(value,"utf8").toString("base64")};}
// Fly volume names allow at most 30 lowercase alphanumeric/underscore bytes.
// The app is independently unique; use its nonce prefix for the volume name.
function volumeName(i:Intent):string{const hex=i.nonce.replaceAll("-","");check(/^[a-f0-9]{32}$/.test(hex));return "rudder_"+hex.slice(0,23);}
export function appAck(body:unknown,intent:Intent):AppAck{const a=record(body);check(text(a.name)===intent.appName&&text(a.network)===intent.networkName&&record(a.organization).slug===intent.policy.organization,"identity_changed");
  check(typeof a.network_cidr==="string"&&a.network_cidr.length>0&&a.network_cidr.length<=96,"identity_changed");return {id:text(a.id),name:a.name,network:a.network,networkCidr:a.network_cidr,organization:intent.policy.organization};}
export function machineConfig(i:Intent,volume:VolumeAck,configured:boolean):Record<string,unknown>{return {
  // Process.exec overrides the image ENTRYPOINT. Preserve its fixed env-clear
  // prelude: ignore_app_secrets does not remove image/platform environment.
  image:i.policy.image,env:{},processes:[{exec:["/usr/bin/env","-i","/usr/local/bin/rudder-machine-root"],env:{},ignore_app_secrets:true}],
  guest:{cpu_kind:"performance",cpus:i.policy.cpus,memory_mb:i.policy.memoryMb},restart:{policy:"no"},auto_destroy:false,
  services:[],mounts:[{volume:volume.id,path:"/workspace"}],metadata:{"rudder-managed-home":i.homeId,"rudder-provision-nonce":i.nonce,"rudder-store-id":i.storeId},
  ...(configured?{files:[{guest_path:"/run/rudder-host-control/config.json",secret_name:CONFIG_SECRET,mode:384},{guest_path:"/run/rudder-host-control/token",secret_name:TOKEN_SECRET,mode:384},{guest_path:"/run/rudder-host-control/registration-key",secret_name:REGISTRATION_SECRET,mode:384}]}:{}),
};}
// Fly's observed omitempty/default representation is compared semantically
// ONLY for the requested zero-value fields. Actual config bytes stay intact.
function emptyEnvironment(v:unknown):boolean{return v===undefined||v===null||typeof v==="object"&&!Array.isArray(v)&&Object.getPrototypeOf(v)===Object.prototype&&Object.keys(v).length===0;}
// A6 observed only OMITTED or plain empty objects for init/process.env.
// Null/arrays/scalars are deliberately not equivalent at these nested fields.
function omittedOrEmptyObject(v:unknown):boolean{return v===undefined||v!==null&&typeof v==="object"&&!Array.isArray(v)&&Object.getPrototypeOf(v)===Object.prototype&&Object.keys(v).length===0;}
function boundProcesses(actual:unknown,expected:unknown,i:Intent,volume:VolumeAck):void{
  const fixed=machineConfig(i,volume,false).processes;
  for(const value of [expected,actual]){check(Array.isArray(value)&&value.length===1,"identity_changed");const process=record(value[0]);
    check(Object.getPrototypeOf(process)===Object.prototype&&omittedOrEmptyObject(process.env),"identity_changed");
    // Comparison projection only. Never mutate/hash/publish a reconstructed config.
    check(canonical([{...process,env:{}}]).equals(canonical(fixed)),"identity_changed");
  }
}
function emptyServices(v:unknown):boolean{return v===undefined||v===null||Array.isArray(v)&&v.length===0;}
function noAutoDestroy(v:unknown):boolean{return v===undefined||v===null||v===false;}
function boundMounts(actual:unknown,expected:unknown,i:Intent,volume:VolumeAck):void {
  check(Array.isArray(actual)&&actual.length===1&&Array.isArray(expected)&&expected.length===1,"identity_changed");
  check(volume.region===i.policy.region&&volume.sizeGb===i.policy.volumeGb,"identity_changed");text(volume.id);text(volume.name);
  for(const value of [expected[0],actual[0]]){
    const mount=record(value),keys=Object.keys(mount);
    // Either the unchanged request or exactly the expansion observed in both
    // original Fly runs. Partial/unobserved mount options are not normalized.
    check(keys.length===2&&keys.every(k=>["volume","path"].includes(k))
      ||keys.length===5&&keys.every(k=>["volume","path","encrypted","size_gb","name"].includes(k)),"identity_changed");
    check(mount.volume===volume.id&&mount.path==="/workspace","identity_changed");
    if(keys.length===5)check(mount.encrypted===true&&mount.size_gb===volume.sizeGb&&mount.name===volume.name,"identity_changed");
  }
}
export function machineAck(body:unknown,i:Intent,expectedConfig:Record<string,unknown>,volume:VolumeAck):MachineAck{const m=record(body);const config=record(m.config);
  check(m.name===i.homeId&&m.region===i.policy.region&&m.host_status==="ok"&&m.incomplete_config==null,"identity_changed");
  check(emptyEnvironment(expectedConfig.env)&&emptyEnvironment(config.env)&&emptyServices(expectedConfig.services)&&emptyServices(config.services)
    &&noAutoDestroy(expectedConfig.auto_destroy)&&noAutoDestroy(config.auto_destroy),"identity_changed");
  boundMounts(config.mounts,expectedConfig.mounts,i,volume);
  // Root's actual A6 response added init:{} and omitted processes[0].env:{}.
  // Only those empty representations are equivalent; fixed exec and all other
  // process keys/values remain exact. Both fresh and retained configs are checked.
  check(omittedOrEmptyObject(expectedConfig.init)&&omittedOrEmptyObject(config.init),"identity_changed");
  boundProcesses(config.processes,expectedConfig.processes,i,volume);
  for(const [key,value]of Object.entries(expectedConfig)){
    if(["env","services","auto_destroy","mounts","init","processes"].includes(key))continue;
    check(config[key]!==undefined&&value!==undefined&&canonical(config[key]).equals(canonical(value)),"identity_changed");
  }
  const allowed=new Set([...Object.keys(expectedConfig),"env","services","auto_destroy","dns","schedule","checks","stop_config","metrics","statics","size","disable_machine_autostart","init"]);
  for(const [key,value]of Object.entries(config))if(!allowed.has(key))check(value===null||value===false||Array.isArray(value)&&value.length===0||typeof value==="object"&&value!==null&&Object.keys(value).length===0,"identity_changed");
  check(typeof m.state==="string"&&["created","stopped","starting","started"].includes(m.state),"identity_changed");
  return {id:text(m.id),instanceId:text(m.instance_id),name:m.name,region:m.region,state:m.state,configSha256:hash(canonical(config)),config:snapshot(config)};}

/** Actual authenticated response path. No resource inventory creates an owner. */
export class FlyManagedHomeProvider {
  #sessions=new Map<string,FlyMachineLeaseSession[]>(); // original nonces/unknown mutations retained
  #disposable=new Map<string,OriginalDisposableMachineCustody>();
  #originals=new Map<string,unknown>(); // filtered original results, even if a later check fails
  constructor(private readonly transport:ProviderTransport){}
  [inspect.custom](){return "FlyManagedHomeProvider [retained private owners]";}
  toJSON():never{fail("invalid_request");}
  #machineRead(i:Intent,body:unknown,stage:"shape"|"binding"|"verified"|"lease"):void {
    // One bounded diagnostic tuple per existing retained home, never config,
    // headers, arbitrary provider text or a resource/ACK adoption constructor.
    try{
      const m=record(body),safe=(v:unknown,re:RegExp)=>typeof v==="string"&&re.test(v)?v:null;
      const states=new Set(["created","starting","started","stopping","stopped","replacing","destroying","destroyed","suspended","failed"]);
      let configSha256:string|null=null;try{configSha256=hash(canonical(record(m.config)));}catch{}
      this.#originals.set(i.nonce+":machine_read",{stage,id:safe(m.id,/^[a-f0-9]{14,16}$/),instanceId:safe(m.instance_id,/^[A-Za-z0-9]{26}$/),
        state:states.has(m.state)?m.state:"unrecognized",hostOk:m.host_status==="ok",incomplete:m.incomplete_config!=null,configSha256});
    }catch{} // Diagnostics cannot decide whether a healthy machine is admitted.
  }
  archiveKnownCompleted(i:Intent):void {check(!this.#disposable.has(i.nonce),"busy");const sessions=this.#sessions.get(i.nonce)??[];for(const s of sessions){const held=s.status();check(held.releaseAcknowledged===true&&held.inFlight===null,"effect_unknown");}if(this.transport instanceof FlyHttpTransport)this.transport.archiveKnownCompleted(i.appName);this.#sessions.delete(i.nonce);for(const k of this.#originals.keys())if(k.startsWith(i.nonce+":"))this.#originals.delete(k);}
  /** Only actual original responses from THIS fixed authenticated transport
   * can mint creation custody. A lookup/serialized expectation/fake transport
   * cannot. Capture occurs before archive and retains all original sessions. */
  captureOriginalDisposableMachine(home:BoundHome):OriginalDisposableMachineCustody {
    const i=home.intent;check(i.runtimeQualification&&home.phase==="routable"&&Object.getPrototypeOf(this.transport)===FlyHttpTransport.prototype,"identity_changed");
    check(!this.#disposable.has(i.nonce)&&this.#disposable.size<8,"busy");
    const originals=["app","volume","machine","configure","start"].map(k=>this.#originals.get(i.nonce+":"+k));
    check(originals.every(x=>x!==undefined),"effect_unknown");
    check((originals[0] as AppAck).id===(home.acks.app as AppAck).id,"identity_changed");
    for(const [j,k] of [[1,"volume"],[2,"machine"],[3,"configure"]] as const)check(canonical(originals[j]).equals(canonical(home.acks[k])),"identity_changed");
    const start=originals[4] as {originalHttpAck?:boolean;ownedReadback?:unknown};check(start.originalHttpAck===true&&canonical(start.ownedReadback).equals(canonical(home.acks.start)),"identity_changed");
    const owner=OriginalDisposableMachineCustody.issue(DISPOSABLE_ISSUER,this,home,originals,this.#sessions.get(i.nonce)??[],this.transport);
    this.#disposable.set(i.nonce,owner);return owner;
  }
  /** Recovery DATA from actually retained response slots, never an owner or
   * adoption constructor. Secret values are never stored in these slots. */
  originalDisposableResponseData(i:Intent):unknown {check(i.runtimeQualification,"unconfigured");return snapshot({provider_request_budget:this.transport instanceof FlyHttpTransport?this.transport.requestBudgetData():null,transport_requests:this.transport instanceof FlyHttpTransport?this.transport.originalResponseCustodyData(i.appName):null,latest_machine_read:this.#originals.get(i.nonce+":machine_read")??null,original_responses:Object.fromEntries(["app","volume","machine","configure","start"].map(k=>[k,this.#originals.get(i.nonce+":"+k)??null]))});}
  describe(effect:Effect,i:Intent,acks:Partial<Record<Effect,EffectAck>>,privateValue?:string):unknown{const prefix=`/v1/apps/${i.appName}`,v=acks.volume as VolumeAck;
    if(effect==="app")return {method:"POST",path:"/v1/apps?wait=true",body:{name:i.appName,org_slug:i.policy.organization,network:i.networkName,idempotency_key:i.nonce}};
    if(effect==="volume")return {method:"POST",path:prefix+"/volumes",body:{name:volumeName(i),region:i.policy.region,size_gb:i.policy.volumeGb,encrypted:true},original_app:acks.app};
    if(effect==="machine")return {method:"POST",path:prefix+"/machines",body:{name:i.homeId,region:i.policy.region,skip_launch:true,skip_service_registration:true,skip_secrets:true,config:machineConfig(i,v,false)},original_app:acks.app};
    if(effect.endsWith("_secret")){check(typeof privateValue==="string");const name=effect==="config_secret"?CONFIG_SECRET:effect==="token_secret"?TOKEN_SECRET:REGISTRATION_SECRET;return {method:"POST",path:prefix+"/secrets/"+name,body_sha256:hash(canonical(secretFileBody(privateValue))),original_machine:acks.machine};}
    const old=(effect==="configure"?acks.machine:acks.configure) as MachineAck;const body=effect==="configure"?{current_version:old.instanceId,config:{...old.config,files:machineConfig(i,v,true).files},skip_launch:true,skip_service_registration:true,min_secrets_version:Math.max(...[acks.config_secret,acks.token_secret,acks.registration_secret].map(a=>Number((a as any).version)))}:null;
    return {method:"POST",path:prefix+"/machines/"+old.id+(effect==="start"?"/start":""),body,original_machine:old,lease:{challenge:i.nonce.replaceAll("-","")+i.nonce.replaceAll("-",""),ttl_seconds:FLY_MANAGED_HOME_LIFECYCLE_LIMITS.leaseSeconds,release_original_nonce:true}};
  }
  async #call(method:"GET"|"POST",path:string,body:unknown,time:OriginalTime,headers?:Record<string,string>):Promise<unknown>{const r=await this.transport.request(method,path,body,time,headers);check(r.status>=200&&r.status<300,"effect_unknown");return r.body;}
  async observeApp(i:Intent,owned:AppAck,time:OriginalTime):Promise<void>{const actual=appAck(await this.#call("GET",`/v1/apps/${i.appName}`,undefined,time),i);check(canonical(actual).equals(canonical(owned)),"identity_changed");}
  async observeMachine(i:Intent,owned:MachineAck,volume:VolumeAck,_configured:boolean,time:OriginalTime):Promise<MachineAck>{const raw=await this.#call("GET",`/v1/apps/${i.appName}/machines/${owned.id}`,undefined,time);this.#machineRead(i,raw,"shape");const actual=machineAck(raw,i,owned.config,volume);this.#machineRead(i,raw,"binding");check(actual.id===owned.id&&actual.instanceId===owned.instanceId&&actual.configSha256===owned.configSha256,"identity_changed");this.#machineRead(i,raw,"verified");return actual;}
  async execute(effect:Effect,i:Intent,acks:Partial<Record<Effect,EffectAck>>,privateValue:string|undefined,time:OriginalTime):Promise<EffectAck>{
    time.remaining();const prefix=`/v1/apps/${i.appName}`;
    if(effect==="app"){
      // Both stable key and body are fixed by the durable intent. Automatic
      // replay is deliberately absent pending provider qualification.
      const raw=record(await this.#call("POST","/v1/apps?wait=true",{name:i.appName,org_slug:i.policy.organization,network:i.networkName,idempotency_key:i.nonce},time));const originalId=text(raw.id);
      this.#originals.set(i.nonce+":app",{id:originalId});const readback=appAck(await this.#call("GET",prefix,undefined,time),i);check(readback.id===originalId,"identity_changed");return readback;
    }
    const app=acks.app as AppAck;check(app,"identity_changed");await this.observeApp(i,app,time);
    if(effect==="volume"){const v=record(await this.#call("POST",prefix+"/volumes",{name:volumeName(i),region:i.policy.region,size_gb:i.policy.volumeGb,encrypted:true},time));
      // Retain the original creation ID before optional metadata validation.
      // This is recovery DATA only; a partial reply never becomes VolumeAck.
      const originalId=text(v.id);this.#originals.set(i.nonce+":volume",{id:originalId});
      check(v.name===volumeName(i)&&v.region===i.policy.region&&v.size_gb===i.policy.volumeGb,"identity_changed");const ack={id:originalId,name:v.name,region:v.region,sizeGb:v.size_gb};this.#originals.set(i.nonce+":volume",ack);return ack;}
    const volume=acks.volume as VolumeAck;check(volume,"identity_changed");
    if(effect==="machine"){const raw=record(await this.#call("POST",prefix+"/machines",{name:i.homeId,region:i.policy.region,skip_launch:true,skip_service_registration:true,skip_secrets:true,config:machineConfig(i,volume,false)},time));
      // Original creation correlation survives every later config/schema check.
      // Partial DATA is not MachineAck, execution admission or teardown authority.
      const originalId=text(raw.id);this.#originals.set(i.nonce+":machine",{id:originalId});
      if(typeof raw.instance_id==="string"&&/^[A-Za-z0-9_:.@\-]{1,180}$/.test(raw.instance_id))this.#originals.set(i.nonce+":machine",{id:originalId,instanceId:raw.instance_id});
      const m=machineAck(raw,i,machineConfig(i,volume,false),volume);check(m.state==="created"||m.state==="stopped","identity_changed");this.#originals.set(i.nonce+":machine",m);return m;}
    if(effect==="config_secret"||effect==="token_secret"||effect==="registration_secret"){
      const machine=acks.machine as MachineAck;await this.observeMachine(i,machine,volume,false,time);check(typeof privateValue==="string"&&privateValue.length>0);
      const name=effect==="config_secret"?CONFIG_SECRET:effect==="token_secret"?TOKEN_SECRET:REGISTRATION_SECRET;const s=record(await this.#call("POST",prefix+"/secrets/"+name,secretFileBody(privateValue),time));
      check(s.name===name&&typeof s.digest==="string"&&/^[a-fA-F0-9]{8,128}$/.test(s.digest)&&Number.isSafeInteger(s.version)&&s.version>0,"identity_changed");
      return {name,digest:s.digest,version:String(s.version)}; // NEVER retain optional value/body
    }
    const old=(effect==="configure"?acks.machine:acks.configure) as MachineAck;check(old,"identity_changed");const configured=effect==="start";
    await this.observeMachine(i,old,volume,configured,time);
    const nextConfig={...snapshot(old.config),files:(machineConfig(i,volume,true).files as unknown[])};
    let update:MachineAck|undefined;let startAck=false;
    const transport:FlyCustodyTransport={request:async(r:FlyCustodyRequest):Promise<FlyCustodyReply>=>{
      // Existing nonce/instance-bound custody code owns the actual lease. This
      // wrapper captures ONLY the response of its original update/start call.
      let body=r.body===undefined?undefined:JSON.parse(r.body);
      if(r.method==="POST"&&r.path===prefix+"/machines/"+old.id)body={...body,skip_launch:true,skip_service_registration:true,min_secrets_version:Math.max(Number((acks.config_secret as any).version),Number((acks.token_secret as any).version),Number((acks.registration_secret as any).version))};
      const response=await this.transport.request(r.method,r.path,body,time,{...r.headers},{signal:r.signal,lifecycleProfile:r.lifecycleProfile});
      if(r.method==="GET"&&r.path===prefix+"/machines/"+old.id)this.#machineRead(i,response.body,"lease");
      if(response.status>=200&&response.status<300&&r.method==="POST"&&r.path===prefix+"/machines/"+old.id){update=machineAck(response.body,i,nextConfig,volume);this.#originals.set(i.nonce+":configure",update);}
      if(response.status>=200&&response.status<300&&r.method==="POST"&&r.path===prefix+"/machines/"+old.id+"/start")startAck=true;
      return response;}};
    const clock={sample:()=>{const raw=readFileSync("/proc/uptime","utf8").split(" ")[0];const uptime=Math.floor(Number(raw)*1000);check(Number.isSafeInteger(uptime)&&uptime>0,"unavailable");return {wallMs:Date.now(),boottimeMs:uptime};}};
    const session=createFlyMachineLeaseSession({transport,clock,expected:{app:i.appName,machineId:old.id,instanceId:old.instanceId,configSha256:old.configSha256,challenge:i.nonce.replaceAll("-","")+i.nonce.replaceAll("-","")},ttlSeconds:FLY_MANAGED_HOME_LIFECYCLE_LIMITS.leaseSeconds,lifecycleProfile:FLY_MANAGED_HOME_LIFECYCLE,sessionLifetimeMs:Math.min(time.remaining(),FLY_MANAGED_HOME_LIFECYCLE_LIMITS.sessionMs)});const retained=this.#sessions.get(i.nonce)??[];retained.push(session);this.#sessions.set(i.nonce,retained);
    await session.acquire();
    if(effect==="configure"){await session.mutate({action:"update",config:nextConfig});check(update&&update.id===old.id&&(update.state==="stopped"||update.state==="created"),"identity_changed");await session.release();return update;}
    await session.mutate({action:"start"});check(startAck,"effect_unknown");
    // This is DISTINCT owned-resource readback after an actual original start
    // HTTP ACK, never reconstruction of an unknown create/start response.
    const observed=machineAck(await this.#call("GET",prefix+"/machines/"+old.id,undefined,time),i,old.config,volume);check(observed.id===old.id&&observed.state==="started"&&observed.configSha256===old.configSha256,"identity_changed");this.#originals.set(i.nonce+":start",{originalHttpAck:true,ownedReadback:observed});await session.release();return observed;
  }
}

const DISPOSABLE_ISSUER=Symbol("actual original disposable Fly create responses");
const originalDisposableOwners=new WeakSet<OriginalDisposableMachineCustody>();
/** Non-Serde original creation custody, not a generic machine-lease or normal
 * retirement proof. All provider sessions/unknown responses remain retained. */
export class OriginalDisposableMachineCustody {
 #publication?:FlyMachineLeaseSession;#publicationStarted=false;
 private constructor(issuer:symbol,private readonly provider:FlyManagedHomeProvider,private readonly home:BoundHome,
   private readonly originals:unknown[],private readonly sessions:FlyMachineLeaseSession[],private readonly transport:ProviderTransport){check(issuer===DISPOSABLE_ISSUER);originalDisposableOwners.add(this);}
 static issue(issuer:symbol,p:FlyManagedHomeProvider,h:BoundHome,o:unknown[],s:FlyMachineLeaseSession[],t:ProviderTransport){return new OriginalDisposableMachineCustody(issuer,p,h,o,s,t);}
 matchesOriginal(home:BoundHome):boolean{return this.home.intent===home.intent||canonical(this.home).equals(canonical(home));}
 async assertOriginalCurrent(time:OriginalTime):Promise<void>{time.remaining();const h=this.home;
  await this.provider.observeApp(h.intent,h.acks.app as AppAck,time);
  const now=await this.provider.observeMachine(h.intent,h.acks.start as MachineAck,h.acks.volume as VolumeAck,true,time);
  check(now.state==="started"&&this.originals.length===5,"identity_changed");time.remaining();
 }
 /** Called ONCE after a durable Started, before any publication. Acquisition
  * stages the real session before its request; an unknown lease is never reset.
  * Existing create/configure/start witnesses remain independently retained. */
 async beginPublicationLeaseRetaining(time:OriginalTime):Promise<void>{check(!this.#publicationStarted,"effect_unknown");this.#publicationStarted=true;
  const h=this.home,m=h.acks.start as MachineAck;
  const transport:FlyCustodyTransport={request:r=>this.transport.request(r.method,r.path,r.body===undefined?undefined:JSON.parse(r.body),time,{...r.headers},{signal:r.signal})};
  const clock={sample:()=>{const raw=readFileSync("/proc/uptime","utf8").split(" ")[0];const boottimeMs=Math.floor(Number(raw)*1000);check(Number.isSafeInteger(boottimeMs)&&boottimeMs>0,"unavailable");return {wallMs:Date.now(),boottimeMs};}};
  const session=createFlyMachineLeaseSession({transport,clock,expected:{app:h.intent.appName,machineId:m.id,instanceId:m.instanceId,configSha256:m.configSha256,challenge:h.intent.runtimeQualification!.qualification_nonce.replaceAll("-","").repeat(2)},ttlSeconds:15,sessionLifetimeMs:Math.min(time.remaining(),15_000)});
  this.#publication=session;this.sessions.push(session);await session.acquire();await this.assertPublicationCurrent(time);
 }
 /** Actual original nonce/session deadline, not acceptance of status JSON. */
 assertPublicationClockCurrent(time:OriginalTime):void {check(this.#publication,"effect_unknown");time.remaining();const status=this.#publication.status();check(status.state==="held"&&status.inFlight===null,"expired");}
 async assertPublicationCurrent(time:OriginalTime):Promise<void>{check(this.#publication,"effect_unknown");time.remaining();await this.#publication.observe();await this.assertOriginalCurrent(time);time.remaining();}
 [inspect.custom](){return "OriginalDisposableMachineCustody [original create + retained exact lease only]";}
 toJSON():never{fail("invalid_request");}
}

/** Brand validation only. No caller boolean, serialized reply, lease inventory
 * or object with lookalike methods can substitute for an actually issued owner. */
export function assertOriginalDisposableMachineCustody(value:unknown,home:BoundHome):asserts value is OriginalDisposableMachineCustody {
 check(value!==null&&typeof value==="object"&&originalDisposableOwners.has(value as OriginalDisposableMachineCustody),"identity_changed");
 check(OriginalDisposableMachineCustody.prototype.matchesOriginal.call(value,home),"identity_changed");
}
