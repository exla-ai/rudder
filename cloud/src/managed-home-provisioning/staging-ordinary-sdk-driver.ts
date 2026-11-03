import {createServer,type Server} from "node:http";
import {inspect} from "node:util";
import {WebSocket,WebSocketServer} from "ws";
import {HomeControlSessions} from "../home-control-sessions.js";
import {createManagedHomeRegistry,type ManagedHomePostgresConfig} from "./index.js";
import {ManagedHomeRegistrationWire} from "./wire.js";
import {handleManagedStorageHttp} from "./storage-http.js";
import type {CommittedHostTransport} from "./transport.js";
import type {ManagedHomeRegistry,ManagedHomeRoute} from "./registry.js";
import {originalTime,type OriginalTime} from "./fly.js";
import {check,fail,snapshot,canonical,QUALIFICATION_CONTROL_ORIGIN,type ProvisionReceipt,type HostOperation} from "./types.js";
import {OriginalStagingCodexProfileImport,validateStagingCodexAccessSnapshot} from "./staging-codex-profile-import.js";
import type {StagingSetupRestriction} from "./staging-role-activation.js";

import {assertStagingGenuineIngressExperiment,type StagingGenuineIngressExperiment} from "./staging-genuine-ingress-experiment.js";
import {preparePositiveIngressRetaining,validatePositiveIngressInput,type PositiveIngressDriver,type PositiveIngressInput} from "./staging-positive-ingress-driver.js";

export type OriginalGenuineIngressTargetData=Readonly<{app:string;machine:string;instance_id:string}>;
const custodyKeys=["scope","home_id","app_name","network_name","provision_nonce","process_owner","app","volume","machine","configured","started","pending_effect","owners_retained","teardown_authority"];
const exactTargetData=(v:unknown,keys:readonly string[]):v is Record<string,unknown>=>!!v&&typeof v==="object"&&!Array.isArray(v)&&Object.keys(v).length===keys.length&&keys.every(k=>Object.hasOwn(v,k));
const targetId=(v:unknown):v is string=>typeof v==="string"&&/^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/.test(v);
const targetUuid=(v:unknown):v is string=>typeof v==="string"&&/^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/.test(v);
const machineKeys=["id","instance_id","name","region","state","config_sha256"];
function settledTargetMachine(v:unknown):v is Record<string,string>{return exactTargetData(v,machineKeys)&&["id","instance_id","name","region"].every(k=>targetId(v[k]))&&["created","stopped","started"].includes(v.state as string)&&typeof v.config_sha256==="string"&&/^[a-f0-9]{64}$/.test(v.config_sha256)&&v.config_sha256!=="0".repeat(64);}

type Case="accepted"|"proc-deny"|"job-deny";
type Command={key:string;case:Case;operation:HostOperation;dispatch?:Promise<unknown>;acceptance?:unknown;queries:unknown[];last?:any;unknown:boolean;};
const RECEIPT_POLL_MILLISECONDS=500,MAX_CASE_RECEIPT_QUERIES=160,MAX_ORDINARY_MESSAGES=1024;
/** Query-only pacing under the caller's SAME retained restriction. This issues
 * no command/owner authority and never restarts or redispatches a mutation. */
export async function pollOriginalStagingReceiptRetaining<T>(input:{current():void;query():Promise<T>;settled(data:T):boolean;wait(milliseconds:number):Promise<void>}):Promise<void>{
 for(let queries=0;;queries++){
  input.current();check(queries<MAX_CASE_RECEIPT_QUERIES,"expired");const data=await input.query();input.current();
  if(input.settled(data))return;
  await input.wait(RECEIPT_POLL_MILLISECONDS);input.current();
 }
}
/** DISTINCT ordinary home, no qualifier scope. Uses only canonical registry,
 * committed socket and existing RPC/outbox/ReceiptV1. No model/task route. */
export class StagingOrdinarySdkDriver {
 readonly registry:ManagedHomeRegistry;readonly wire:ManagedHomeRegistrationWire;
 #server?:Server;#wss?:WebSocketServer;#sockets=new Set<WebSocket>();#committed=new Map<string,{socket:WebSocket;owner:CommittedHostTransport}>();
 #sessions=new HomeControlSessions<WebSocket,unknown>({isOpen:s=>s.readyState===WebSocket.OPEN,send:(s,b)=>{if(s.readyState!==WebSocket.OPEN)return false;this.genuine?.observeOriginalSerializedSend(s,b);s.send(b,error=>{this.genuine?.observeOriginalSendCallback(!!error);if(error)this.#error=true;});return true;}});
 #started=false;#time?:OriginalTime;#error=false;#expired=false;#pending=0;#messages=0;#admissions=0;
 #custody:unknown=null;#provision?:Promise<ProvisionReceipt>;#receipt?:ProvisionReceipt;#route?:ManagedHomeRoute;#commands:Command[]=[];#fuse?:NodeJS.Timeout;#genuineRun?:Promise<void>;
 #positiveDriver?:PositiveIngressDriver<WebSocket>;#positiveRun?:Promise<void>;
 #profileImport?:OriginalStagingCodexProfileImport<WebSocket>;#profileImportRun?:Promise<void>;
 readonly #snapshot?:Buffer;readonly #flyToken:string;
 constructor(config:ManagedHomePostgresConfig,private readonly account:string,private readonly key:string,private readonly setup:StagingSetupRestriction,private readonly genuine?:StagingGenuineIngressExperiment<WebSocket>,private readonly positive?:PositiveIngressInput,positiveProfileSnapshot?:Buffer){
  check(!(genuine&&positive),"invalid_request");
  if(positive!==undefined)this.positive=validatePositiveIngressInput(positive);
  if(positiveProfileSnapshot!==undefined){check(this.positive,"invalid_request");this.#snapshot=validateStagingCodexAccessSnapshot(positiveProfileSnapshot,this.positive.profileId);}
  this.#flyToken=config.flyToken;
  if(genuine)assertStagingGenuineIngressExperiment(genuine);
  check(config.policy.controlPlane?.profile==="qualification-v1"&&config.freshStorage&&config.policy.compilerSdk&&config.policy.sdkInitialSource==="sdk-fixed-suite-v1","unconfigured");
  // ProviderFenced is explicit, first-owner only. No clock-policy fabrication.
  check(config.policy.ownerMode==="provider-fenced-v1"&&!config.policy.ownerLeaseSafety,"unconfigured");
  this.registry=createManagedHomeRegistry(config);this.wire=new ManagedHomeRegistrationWire(this.registry);if(genuine)genuine.retainOriginalLogCredential(config.flyToken);
 }
 [inspect.custom](){return "StagingOrdinarySdkDriver [original ordinary owners retained]";}
 toJSON():never{fail("invalid_request");}
 #current(){this.setup.remaining();check(this.#time&&!this.#expired,"expired");this.#time.remaining();}
 async startRetaining():Promise<void>{
  check(!this.#started,"effect_unknown");this.#started=true;this.#time=originalTime(300_000);this.#fuse=setTimeout(()=>{this.#expired=true;},300_000);this.#current();
  const server=createServer((req,res)=>{void(async()=>{this.#current();check(req.headers.host===new URL(QUALIFICATION_CONTROL_ORIGIN).host&&req.headers["x-forwarded-proto"]==="https","identity_changed");
   if(!await handleManagedStorageHttp(req,res,new URL(req.url??"/",QUALIFICATION_CONTROL_ORIGIN),this.registry)){res.writeHead(404);res.end();}
  })().catch(()=>{if(!res.headersSent)res.writeHead(503);res.end();});});this.#server=server;
  const wss=new WebSocketServer({noServer:true,maxPayload:64*1024,perMessageDeflate:false});this.#wss=wss;
  server.headersTimeout=5000;server.requestTimeout=5000;server.maxHeadersCount=32;server.maxConnections=16;
  server.on("error",()=>{this.#error=true;});wss.on("error",()=>{this.#error=true;});
  server.on("upgrade",(req,socket,head)=>{if(this.#pending>=4){socket.destroy();return;}this.#pending++;void(async()=>{
   this.#current();check(this.#admissions<4&&req.method==="GET"&&req.headers.host===new URL(QUALIFICATION_CONTROL_ORIGIN).host&&req.headers["x-forwarded-proto"]==="https","identity_changed");
   const m=/^\/api\/rudder\/home\/(mh-[a-f0-9-]{36})\/control$/.exec(req.url??""),auth=req.headers.authorization;
   check(m&&typeof auth==="string"&&/^Bearer rdrmh_[a-f0-9]{64}$/.test(auth),"not_found");
   const worker=await this.registry.authenticateManagedHomeWorker(m[1],auth.slice(7));this.#current();check(worker.accountId===this.account,"identity_changed");this.#admissions++;
   wss.handleUpgrade(req,socket,head,ws=>{
    this.#sockets.add(ws);this.registry.bindAuthenticatedManagedHomeSocket(ws,worker);
    ws.on("error",()=>{this.#error=true;});ws.on("close",()=>{this.registry.closeManagedHostSocket(ws);this.#sessions.closeCurrent(worker.homeId,ws);});
    ws.on("message",raw=>{if(this.#expired||this.#messages>=MAX_ORDINARY_MESSAGES)return;this.#messages++;void(async()=>{
     this.#current();let message:any;try{message=JSON.parse(raw.toString());}catch{return;}
     if(message?.type==="rpc-result"){
      const actual=this.#committed.get(worker.homeId);check(actual?.socket===ws,"identity_changed");await actual.owner.assertCurrent();this.#current();
      check(message&&typeof message.id==="string","identity_changed");this.#reply(worker.homeId,ws,message);return;
     }
     await this.wire.receive(worker,message,reply=>new Promise<void>((resolve,reject)=>{ws.send(JSON.stringify(reply),error=>error?reject(new Error("staging ordinary wire unknown")):resolve());}),ws,async owner=>{
      await owner.assertCurrent();this.#current();check(owner.socket===ws&&ws.readyState===WebSocket.OPEN&&!this.#committed.has(worker.homeId),"identity_changed");
      this.#committed.set(worker.homeId,{socket:ws,owner});const session=this.#sessions.attach(worker.homeId,ws);this.#attached.set(ws,session);this.genuine?.retainOriginalCommitted(ws,owner,session);
     });
    })().catch(()=>{this.#error=true;});});
   });
  })().catch(()=>{socket.destroy();}).finally(()=>{this.#pending--;});});
  const listen=new Promise<void>((resolve,reject)=>{server.once("listening",resolve);server.once("error",()=>reject(new Error("ordinary listen failed")));server.listen(8080,"0.0.0.0");});
  await listen;this.#current();
  this.#provision=this.registry.provisionManagedHome(this.account,this.key+":ordinary",["SDK fixed source suite"]);try{this.#receipt=await this.#provision;this.#current();}finally{try{this.#custody=await this.registry.stagingOrdinaryProvisionCustodyData(this.account,this.key+":ordinary");}catch{}}
 }
 #attached=new Map<WebSocket,ReturnType<HomeControlSessions<WebSocket,unknown>["attach"]>>();
 #reply(home:string,ws:WebSocket,m:any){const s=this.#attached.get(ws);check(s&&s.homeId===home&&this.#sessions.isCurrent(s),"identity_changed");this.#sessions.reply(s,m.id,m.result,typeof m.error==="string"?m.error:undefined);}
 async #wait(){this.#current();await new Promise<void>(resolve=>setTimeout(resolve,100));this.#current();}
 async runCasesRetaining():Promise<void>{
  check(!this.genuine&&!this.positive,"unconfigured");this.#current();check(this.#receipt?.projects.length===1&&!this.#commands.length,"effect_unknown");
  const receipt=this.#receipt,project=receipt.projects[0];
  while(!this.#route){this.#current();try{this.#route=await this.registry.resolveManagedHomeRoute(this.account,receipt.homeId,project.workspaceId);await this.#route.assertRoutableCurrent();}
   catch{this.#route=undefined;await this.#wait();}}
  const route=this.#route;
  const send=async(method:string,params:unknown,remaining:number)=>{
   this.#current();const held=this.#committed.get(route.homeId);check(held,"not_found");await held.owner.assertCurrent();await this.registry.assertManagedHomeSocketMatchesRoute(held.socket,route);this.#current();
   return this.#sessions.request(route.homeId,held.socket,method,params,Math.min(remaining,this.setup.remaining(),this.#time!.remaining()));
  };
  for(const c of ["accepted","proc-deny","job-deny"] as const){
   const operation:HostOperation={op:"BuildCurrentSourceSdkV1",project_alias:route.projectAlias,case:c,repeat_identical:c==="accepted"};
   const cmd:Command={key:this.key+":"+c,case:c,operation,queries:[],unknown:false};this.#commands.push(cmd);this.#current();
   cmd.dispatch=this.registry.dispatchManagedHomeCommand(route,cmd.key,operation,send);
   try{cmd.acceptance=await cmd.dispatch;}catch{cmd.unknown=true;throw new Error("original SDK dispatch unknown; retained, no replay");}
   // Queries use SAME key and canonical ReceiptV1, never redispatch/restart.
   await pollOriginalStagingReceiptRetaining<any>({current:()=>this.#current(),query:async()=>{
    const p=this.registry.dispatchManagedHomeCommand(route,cmd.key,{op:"ReceiptV1",lookup_command_key:cmd.key},send);cmd.queries.push(p);
    const data:any=await p;cmd.last=data;return data;
   },settled:data=>{
    if(data.receipt?.state==="completed"){
     const result=data.receipt.result;check(result?.case===c&&result.cache_reuse===false&&result.source_accepted===false,"identity_changed");
     check(result.phase===(c==="accepted"?"sdk_artifacts_published":"sdk_helper_failed"),"identity_changed");
     const o=data.sdk_observation;
     if(o?.job_disposal==="settled"){check(o.build_worker_join_observed===true&&o.cleanup_worker_join_observed===true&&o.source_authority===false&&o.shared_floor_retirement===false,"identity_changed");return true;}
    }
    return false;
   },wait:milliseconds=>new Promise<void>(resolve=>setTimeout(resolve,milliseconds))});
  }
 }
 /** Explicit opt-in only; reuses the SAME listener/registry/provision/socket
  * owners. One actual OpenPreparedProject dispatch, no SDK case/Receipt query.
  * The original experiment is retained by this listener before any await.
  */
 runGenuineIngressRetaining():Promise<void>{
  check(this.genuine&&!this.positive&&!this.#genuineRun&&!this.#commands.length,"unconfigured");
  const original=this.genuine;
  this.#genuineRun=(async()=>{try{this.#current();check(this.#receipt?.projects.length===1,"effect_unknown");
   const receipt=this.#receipt,project=receipt.projects[0];
   const route=await this.registry.resolveManagedHomeRoute(this.account,receipt.homeId,project.workspaceId);this.#route=route;
   await route.assertRoutableCurrent();this.#current();
   await original.runRetaining(this.registry,route,this.#sessions,this.setup,this.#time!);
  }catch(error){original.refuseOriginalObservationBeforeDispatch();throw error;}})();return this.#genuineRun;
 }
 /** Explicit task arm on the same authenticated listener. Both the driver and
  * original promise remain held on failure. Backend readiness is observation
  * DATA, not enrollment, completion, checkpoint or teardown authority. */
 runPositiveIngressRetaining():Promise<void>{
  check(this.positive&&!this.genuine&&!this.#positiveRun&&!this.#commands.length,"unconfigured");
  this.#positiveRun=Promise.resolve().then(async()=>{
   this.#current();check(this.#receipt?.projects.length===1,"effect_unknown");
   const receipt=this.#receipt;
   const route=await this.registry.resolveManagedHomeRoute(this.account,receipt.homeId,receipt.projects[0].workspaceId);
   this.#route=route;await route.assertRoutableCurrent();this.#current();
   const held=this.#committed.get(route.homeId);check(held,"not_found");
   const session=this.#attached.get(held.socket);check(session&&this.#sessions.isCurrent(session),"identity_changed");
   if(this.#snapshot){
    const target=this.registry.stagingOrdinaryProfileImportTargetData(route);
    this.#profileImport=new OriginalStagingCodexProfileImport({registry:this.registry,route,sessions:this.#sessions,session,
     socket:held.socket,owner:held.owner,setup:this.setup,driver:this.#time!,assertTargetCurrent:async expected=>{
      this.#current();await route.assertRoutableCurrent();this.#current();
      check(canonical(this.registry.stagingOrdinaryProfileImportTargetData(route)).equals(canonical(expected)),"identity_changed");
     }},this.#snapshot,this.positive!.profileId,target,this.#flyToken);
    this.#profileImportRun=this.#profileImport.startRetaining();await this.#profileImportRun;this.#current();this.#snapshot.fill(0);
   }
   this.#positiveDriver=preparePositiveIngressRetaining({registry:this.registry,route,sessions:this.#sessions,session,
    socket:held.socket,owner:held.owner,setup:this.setup,driver:this.#time!},this.positive!);
   await this.#positiveDriver.runRetaining();this.#current();
  });return this.#positiveRun;
 }
 /** Genuine non-serializable SAME-HOST original holder, never status JSON or
  * a remote timeout. Collector invokes its remaining method BEFORE effects.
  * It is retained even on unknown transport outcome; no fresh dispatch exists.
  */
 /** Historical target DATA from this SAME retained original provisioning
  * carrier. No provider lookup, owner adoption, route capability or new clock.
  * Creation may precede a changed configured instance; configured/start ACKs
  * must agree. Dispatch still performs every original route/socket check. */
 originalGenuineIngressTargetDataRetaining():OriginalGenuineIngressTargetData{
  check(this.genuine,"unconfigured");this.#current();const r=this.#receipt,c=this.#custody;
  check(exactTargetData(r,["homeId","commandKey","epoch","phase","projects","nativeIngressReady","taskReady"])&&r.commandKey===this.key+":ordinary"&&typeof r.epoch==="string"&&/^[1-9][0-9]{0,19}$/.test(r.epoch)&&r.phase==="routable"&&Array.isArray(r.projects)&&r.projects.length===1&&r.nativeIngressReady===false&&r.taskReady===false,"effect_unknown");
  const project=r.projects[0];check(exactTargetData(project,["projectId","workspaceId","title","state"])&&targetUuid(project.projectId)&&typeof project.workspaceId==="string"&&project.workspaceId.startsWith("mp-")&&targetUuid(project.workspaceId.slice(3))&&project.title==="SDK fixed source suite"&&project.state==="reserved","identity_changed");
  check(exactTargetData(c,custodyKeys)&&c.scope==="retained-original-ordinary-provision-recovery-data-only"&&c.home_id===r.homeId&&typeof c.home_id==="string"&&c.home_id.startsWith("mh-")&&targetUuid(c.home_id.slice(3))&&targetUuid(c.provision_nonce)&&targetUuid(c.process_owner)&&c.owners_retained===true&&c.teardown_authority===false&&c.pending_effect===null,"identity_changed");
  check(targetId(c.app_name)&&targetId(c.network_name)&&c.app_name==="rudder-mh-"+c.provision_nonce.replaceAll("-", ""),"identity_changed");
  check(exactTargetData(c.app,["id","name","network","organization"])&&Object.values(c.app).every(targetId)&&c.app.name===c.app_name&&c.app.network===c.network_name,"identity_changed");
  check(exactTargetData(c.volume,["id","name","region"])&&Object.values(c.volume).every(targetId),"identity_changed");
  check(settledTargetMachine(c.machine)&&settledTargetMachine(c.configured)&&settledTargetMachine(c.started)&&c.started.state==="started","identity_changed");
  const first=c.machine,configured=c.configured,started=c.started;
  check([first,configured].every(m=>["id","name","region"].every(k=>m[k]===started[k]))&&configured.instance_id===started.instance_id&&configured.config_sha256===started.config_sha256&&c.volume.region===started.region,"identity_changed");
  check([c.app_name,started.id,started.instance_id].every(v=>v.length<=80),"identity_changed");
  return snapshot({app:c.app_name,machine:started.id,instance_id:started.instance_id});
 }
 originalGenuineIngressObservationRetaining():StagingGenuineIngressExperiment<WebSocket>{
  check(this.genuine&&this.#genuineRun,"unconfigured");return this.genuine;
 }
 statusData(){return snapshot({scope:"ordinary-source-sdk-staging-data-only",receipt:this.#receipt??null,custody:this.#custody,profile_import:this.#profileImport?.statusData()??null,...(this.genuine?{genuine_ingress:this.genuine.statusData()}:{}),...(this.positive?{positive_ingress:this.#positiveDriver?.statusData()??null}:{}),commands:this.#commands.map(v=>({case:v.case,command_key:v.key,acceptance:v.acceptance??null,last:v.last??null,unknown:v.unknown,receipt_queries:v.queries.length})),expired:this.#expired,error:this.#error,owners_retained:true,source_acceptance_claim:false,cache_reuse_claim:false,negative_kernel_reason_claim:false,shared_floor_retirement_claim:false});}
}
