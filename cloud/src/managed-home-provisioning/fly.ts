import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { inspect } from "node:util";
import { createFlyMachineLeaseSession, type FlyCustodyTransport, type FlyCustodyRequest, type FlyCustodyReply, type FlyMachineLeaseSession } from "../fly-custody.js";
import { check, fail, canonical, hash, snapshot, id, CONFIG_SECRET, TOKEN_SECRET, REGISTRATION_SECRET, type Intent, type Effect, type EffectAck, type AppAck, type VolumeAck, type MachineAck } from "./types.js";

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
export interface ProviderTransport {request(method:"GET"|"POST"|"DELETE",path:string,body:unknown,time:OriginalTime,headers?:Record<string,string>):Promise<ProviderReply>;}
export class FlyHttpTransport implements ProviderTransport {
  #token:string;
  constructor(token:string){check(typeof token==="string"&&token.length>20&&token.length<=16384&&!/[\r\n\0]/.test(token));this.#token=token;}
  [inspect.custom](){return "FlyHttpTransport [private]";}
  toJSON():never{fail("invalid_request");}
  async request(method:"GET"|"POST"|"DELETE",path:string,body:unknown,time:OriginalTime,headers:Record<string,string>={}):Promise<ProviderReply>{
    check(path.startsWith("/v1/apps")&&!/[\r\n\0]/.test(path));const abort=new AbortController();const timer=setTimeout(()=>abort.abort(),Math.min(15_000,time.remaining()));
    try{const reply=await fetch("https://api.machines.dev"+path,{method,redirect:"error",signal:abort.signal,headers:{Authorization:`Bearer ${this.#token}`,"Content-Type":"application/json",...headers},...(body===undefined?{}:{body:JSON.stringify(body)})});
      const chunks:Uint8Array[]=[];let size=0;const reader=reply.body?.getReader();if(reader){for(;;){const part=await reader.read();if(part.done)break;size+=part.value.length;if(size>128*1024){await reader.cancel().catch(()=>{});fail("effect_unknown");}chunks.push(part.value);}}
      const bytes=Buffer.concat(chunks);let value:unknown={};if(bytes.length){try{value=JSON.parse(bytes.toString("utf8"));}catch{fail("effect_unknown");}}return {status:reply.status,body:value};
    }catch{fail("effect_unknown");}finally{clearTimeout(timer);}
  }
}
function record(v:unknown):Record<string,any>{check(v!==null&&typeof v==="object"&&!Array.isArray(v),"identity_changed");return v as Record<string,any>;}
function text(v:unknown):string{id(v);return v;}
export function appAck(body:unknown,intent:Intent):AppAck{const a=record(body);check(text(a.name)===intent.appName&&text(a.network)===intent.networkName&&record(a.organization).slug===intent.policy.organization,"identity_changed");
  check(typeof a.network_cidr==="string"&&a.network_cidr.length>0&&a.network_cidr.length<=96,"identity_changed");return {id:text(a.id),name:a.name,network:a.network,networkCidr:a.network_cidr,organization:intent.policy.organization};}
export function machineConfig(i:Intent,volume:VolumeAck,configured:boolean):Record<string,unknown>{return {
  image:i.policy.image,env:{},processes:[{exec:["/usr/local/bin/rudder-machine-root"],env:{},ignore_app_secrets:true}],
  guest:{cpu_kind:"performance",cpus:i.policy.cpus,memory_mb:i.policy.memoryMb},restart:{policy:"no"},auto_destroy:false,
  services:[],mounts:[{volume:volume.id,path:"/workspace"}],metadata:{"rudder-managed-home":i.homeId,"rudder-provision-nonce":i.nonce,"rudder-store-id":i.storeId},
  ...(configured?{files:[{guest_path:"/run/rudder-host-control/config.json",secret_name:CONFIG_SECRET,mode:384},{guest_path:"/run/rudder-host-control/token",secret_name:TOKEN_SECRET,mode:384},{guest_path:"/run/rudder-host-control/registration-key",secret_name:REGISTRATION_SECRET,mode:384}]}:{}),
};}
export function machineAck(body:unknown,i:Intent,expectedConfig:Record<string,unknown>):MachineAck{const m=record(body);const config=record(m.config);
  check(m.name===i.homeId&&m.region===i.policy.region&&m.host_status==="ok"&&m.incomplete_config===undefined,"identity_changed");
  // Compare every selected field; provider additions are not copied into a
  // future mutation. Secret values/extra env/processes/services fail closed.
  for(const [key,value]of Object.entries(expectedConfig))check(hash(canonical(config[key]))===hash(canonical(value)),"identity_changed");
  const allowed=new Set([...Object.keys(expectedConfig),"dns","schedule","checks","stop_config","metrics","statics","size","disable_machine_autostart","init"]);
  for(const [key,value]of Object.entries(config))if(!allowed.has(key))check(value===null||value===false||Array.isArray(value)&&value.length===0||typeof value==="object"&&value!==null&&Object.keys(value).length===0,"identity_changed");
  // Additional init/ingress overrides cannot silently replace fixed exec.
  if(config.init)check(Object.keys(config.init).length===0,"identity_changed");
  check(typeof m.state==="string"&&["created","stopped","starting","started"].includes(m.state),"identity_changed");
  return {id:text(m.id),instanceId:text(m.instance_id),name:m.name,region:m.region,state:m.state,configSha256:hash(canonical(config)),config:snapshot(config)};}

/** Actual authenticated response path. No resource inventory creates an owner. */
export class FlyManagedHomeProvider {
  #sessions=new Map<string,FlyMachineLeaseSession[]>(); // original nonces/unknown mutations retained
  #originals=new Map<string,unknown>(); // filtered original results, even if a later check fails
  constructor(private readonly transport:ProviderTransport){}
  [inspect.custom](){return "FlyManagedHomeProvider [retained private owners]";}
  toJSON():never{fail("invalid_request");}
  archiveKnownCompleted(i:Intent):void {const sessions=this.#sessions.get(i.nonce)??[];for(const s of sessions){const held=s.status();check(held.releaseAcknowledged===true&&held.inFlight===null,"effect_unknown");}this.#sessions.delete(i.nonce);for(const k of this.#originals.keys())if(k.startsWith(i.nonce+":"))this.#originals.delete(k);}
  describe(effect:Effect,i:Intent,acks:Partial<Record<Effect,EffectAck>>,privateValue?:string):unknown{const prefix=`/v1/apps/${i.appName}`,v=acks.volume as VolumeAck;
    if(effect==="app")return {method:"POST",path:"/v1/apps?wait=true",body:{name:i.appName,org_slug:i.policy.organization,network:i.networkName,idempotency_key:i.nonce}};
    if(effect==="volume")return {method:"POST",path:prefix+"/volumes",body:{name:"rudder_"+i.nonce.replaceAll("-",""),region:i.policy.region,size_gb:i.policy.volumeGb,encrypted:true},original_app:acks.app};
    if(effect==="machine")return {method:"POST",path:prefix+"/machines",body:{name:i.homeId,region:i.policy.region,skip_launch:true,skip_service_registration:true,skip_secrets:true,config:machineConfig(i,v,false)},original_app:acks.app};
    if(effect.endsWith("_secret")){check(typeof privateValue==="string");const name=effect==="config_secret"?CONFIG_SECRET:effect==="token_secret"?TOKEN_SECRET:REGISTRATION_SECRET;return {method:"POST",path:prefix+"/secrets/"+name,body_sha256:hash(canonical({value:privateValue})),original_machine:acks.machine};}
    const old=(effect==="configure"?acks.machine:acks.configure) as MachineAck;const body=effect==="configure"?{current_version:old.instanceId,config:{...old.config,files:machineConfig(i,v,true).files},skip_launch:true,skip_service_registration:true,min_secrets_version:Math.max(...[acks.config_secret,acks.token_secret,acks.registration_secret].map(a=>Number((a as any).version)))}:null;
    return {method:"POST",path:prefix+"/machines/"+old.id+(effect==="start"?"/start":""),body,original_machine:old,lease:{challenge:i.nonce.replaceAll("-","")+i.nonce.replaceAll("-",""),ttl_seconds:15,release_original_nonce:true}};
  }
  async #call(method:"GET"|"POST",path:string,body:unknown,time:OriginalTime,headers?:Record<string,string>):Promise<unknown>{const r=await this.transport.request(method,path,body,time,headers);check(r.status>=200&&r.status<300,"effect_unknown");return r.body;}
  async observeApp(i:Intent,owned:AppAck,time:OriginalTime):Promise<void>{const actual=appAck(await this.#call("GET",`/v1/apps/${i.appName}`,undefined,time),i);check(canonical(actual).equals(canonical(owned)),"identity_changed");}
  async observeMachine(i:Intent,owned:MachineAck,_volume:VolumeAck,_configured:boolean,time:OriginalTime):Promise<MachineAck>{const actual=machineAck(await this.#call("GET",`/v1/apps/${i.appName}/machines/${owned.id}`,undefined,time),i,owned.config);check(actual.id===owned.id&&actual.instanceId===owned.instanceId&&actual.configSha256===owned.configSha256,"identity_changed");return actual;}
  async execute(effect:Effect,i:Intent,acks:Partial<Record<Effect,EffectAck>>,privateValue:string|undefined,time:OriginalTime):Promise<EffectAck>{
    time.remaining();const prefix=`/v1/apps/${i.appName}`;
    if(effect==="app"){
      // Both stable key and body are fixed by the durable intent. Automatic
      // replay is deliberately absent pending provider qualification.
      const raw=record(await this.#call("POST","/v1/apps?wait=true",{name:i.appName,org_slug:i.policy.organization,network:i.networkName,idempotency_key:i.nonce},time));const originalId=text(raw.id);
      this.#originals.set(i.nonce+":app",{id:originalId});const readback=appAck(await this.#call("GET",prefix,undefined,time),i);check(readback.id===originalId,"identity_changed");return readback;
    }
    const app=acks.app as AppAck;check(app,"identity_changed");await this.observeApp(i,app,time);
    if(effect==="volume"){const v=record(await this.#call("POST",prefix+"/volumes",{name:"rudder_"+i.nonce.replaceAll("-",""),region:i.policy.region,size_gb:i.policy.volumeGb,encrypted:true},time));
      check(v.name==="rudder_"+i.nonce.replaceAll("-","")&&v.region===i.policy.region&&v.size_gb===i.policy.volumeGb,"identity_changed");return {id:text(v.id),name:v.name,region:v.region,sizeGb:v.size_gb};}
    const volume=acks.volume as VolumeAck;check(volume,"identity_changed");
    if(effect==="machine"){const m=machineAck(await this.#call("POST",prefix+"/machines",{name:i.homeId,region:i.policy.region,skip_launch:true,skip_service_registration:true,skip_secrets:true,config:machineConfig(i,volume,false)},time),i,machineConfig(i,volume,false));check(m.state==="created"||m.state==="stopped","identity_changed");return m;}
    if(effect==="config_secret"||effect==="token_secret"||effect==="registration_secret"){
      const machine=acks.machine as MachineAck;await this.observeMachine(i,machine,volume,false,time);check(typeof privateValue==="string"&&privateValue.length>0);
      const name=effect==="config_secret"?CONFIG_SECRET:effect==="token_secret"?TOKEN_SECRET:REGISTRATION_SECRET;const s=record(await this.#call("POST",prefix+"/secrets/"+name,{value:privateValue},time));
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
      const response=await this.transport.request(r.method,r.path,body,time,{...r.headers});
      if(response.status>=200&&response.status<300&&r.method==="POST"&&r.path===prefix+"/machines/"+old.id){update=machineAck(response.body,i,nextConfig);this.#originals.set(i.nonce+":configure",update);}
      if(response.status>=200&&response.status<300&&r.method==="POST"&&r.path===prefix+"/machines/"+old.id+"/start")startAck=true;
      return response;}};
    const clock={sample:()=>{const raw=readFileSync("/proc/uptime","utf8").split(" ")[0];const uptime=Math.floor(Number(raw)*1000);check(Number.isSafeInteger(uptime)&&uptime>0,"unavailable");return {wallMs:Date.now(),boottimeMs:uptime};}};
    const session=createFlyMachineLeaseSession({transport,clock,expected:{app:i.appName,machineId:old.id,instanceId:old.instanceId,configSha256:old.configSha256,challenge:i.nonce.replaceAll("-","")+i.nonce.replaceAll("-","")},ttlSeconds:15,sessionLifetimeMs:Math.min(time.remaining(),15_000)});const retained=this.#sessions.get(i.nonce)??[];retained.push(session);this.#sessions.set(i.nonce,retained);
    await session.acquire();
    if(effect==="configure"){await session.mutate({action:"update",config:nextConfig});check(update&&update.id===old.id&&(update.state==="stopped"||update.state==="created"),"identity_changed");await session.release();return update;}
    await session.mutate({action:"start"});check(startAck,"effect_unknown");
    // This is DISTINCT owned-resource readback after an actual original start
    // HTTP ACK, never reconstruction of an unknown create/start response.
    const observed=machineAck(await this.#call("GET",prefix+"/machines/"+old.id,undefined,time),i,old.config);check(observed.id===old.id&&observed.state==="started"&&observed.configSha256===old.configSha256,"identity_changed");this.#originals.set(i.nonce+":start",{originalHttpAck:true,ownedReadback:observed});await session.release();return observed;
  }
}
