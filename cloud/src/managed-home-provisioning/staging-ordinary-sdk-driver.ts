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
import {check,fail,snapshot,QUALIFICATION_CONTROL_ORIGIN,type ProvisionReceipt,type HostOperation} from "./types.js";
import type {StagingSetupRestriction} from "./staging-role-activation.js";

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
 #sessions=new HomeControlSessions<WebSocket,unknown>({isOpen:s=>s.readyState===WebSocket.OPEN,send:(s,b)=>{if(s.readyState!==WebSocket.OPEN)return false;s.send(b,error=>{if(error)this.#error=true;});return true;}});
 #started=false;#time?:OriginalTime;#error=false;#expired=false;#pending=0;#messages=0;#admissions=0;
 #custody:unknown=null;#provision?:Promise<ProvisionReceipt>;#receipt?:ProvisionReceipt;#route?:ManagedHomeRoute;#commands:Command[]=[];#fuse?:NodeJS.Timeout;
 constructor(config:ManagedHomePostgresConfig,private readonly account:string,private readonly key:string,private readonly setup:StagingSetupRestriction){
  check(config.policy.controlPlane?.profile==="qualification-v1"&&config.freshStorage&&config.policy.compilerSdk&&config.policy.sdkInitialSource==="sdk-fixed-suite-v1","unconfigured");
  // ProviderFenced is explicit, first-owner only. No clock-policy fabrication.
  check(config.policy.ownerMode==="provider-fenced-v1"&&!config.policy.ownerLeaseSafety,"unconfigured");
  this.registry=createManagedHomeRegistry(config);this.wire=new ManagedHomeRegistrationWire(this.registry);
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
      this.#committed.set(worker.homeId,{socket:ws,owner});this.#attached.set(ws,this.#sessions.attach(worker.homeId,ws));
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
  this.#current();check(this.#receipt?.projects.length===1&&!this.#commands.length,"effect_unknown");
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
 statusData(){return snapshot({scope:"ordinary-source-sdk-staging-data-only",receipt:this.#receipt??null,custody:this.#custody,commands:this.#commands.map(v=>({case:v.case,command_key:v.key,acceptance:v.acceptance??null,last:v.last??null,unknown:v.unknown,receipt_queries:v.queries.length})),expired:this.#expired,error:this.#error,owners_retained:true,source_acceptance_claim:false,cache_reuse_claim:false,negative_kernel_reason_claim:false,shared_floor_retirement_claim:false});}
}
