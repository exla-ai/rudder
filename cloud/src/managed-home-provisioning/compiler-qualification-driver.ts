import {OriginalSdkDiagnosticRead,sdkDiagnosticTarget,type SdkFailureDiagnostic} from "./staging-sdk-diagnostics.js";
import {StagingListenerAdmissionClosure} from "./staging-listener-admission.js";
import type {OriginalSdkRoleActivationPermit} from "./compiler-qualification.js";
import {createServer,type Server} from "node:http";
import {WebSocket,WebSocketServer} from "ws";
import {inspect} from "node:util";
import {createManagedCompilerSdkQualificationRegistry,type ManagedHomePostgresConfig} from "./index.js";
import {ManagedHomeRegistrationWire} from "./wire.js";
import {originalTime,type OriginalTime} from "./fly.js";
import {check,fail,snapshot,controlOrigin,QUALIFICATION_CONTROL_ORIGIN,type Code} from "./types.js";
import type {CompilerQualificationCandidate,CompilerQualificationAck} from "./compiler-qualification-protocol.js";
import type {Envelope} from "./registration.js";
import type {ManagedHomeRegistry,RuntimeQualificationProvisionReceipt} from "./registry.js";

/** Dedicated staging fixture/driver, not the production scheduler or another
 * config writer. Same concrete registry, original provider effects and wire.
 * One fixed HTTPS/WSS origin; no RPC/project/model/general-operation handler.
 * Servers/sockets/campaign owners remain held after any error/timeout until
 * Root's explicit disposable provider teardown; this class never kills/restarts.
 */
export class CompilerSdkQualificationDriver {
 readonly registry:ManagedHomeRegistry;readonly wire:ManagedHomeRegistrationWire;
 #diagnosticToken:string;
 #server?:Server;#wss?:WebSocketServer;#listen?:Promise<void>;#sockets=new Set<WebSocket>();
 #acceptingClosed=false;#listenerClosure?:StagingListenerAdmissionClosure;
 #admissions=0;#pendingUpgrades=0;#messages=0;#started=false;#time?:OriginalTime;#provision?:Promise<RuntimeQualificationProvisionReceipt>;
 #receipt?:RuntimeQualificationProvisionReceipt;#error?:Code;#expired=false;#fuse?:NodeJS.Timeout;
 #diagnosticRead?:OriginalSdkDiagnosticRead;#diagnosticLoop?:Promise<void>;#diagnosticReads=0;#diagnosticFailure?:SdkFailureDiagnostic;#roleObserved=false;
 #ack?:Envelope<CompilerQualificationAck>;#sendAttempted=false;#sendCallback=false;#sendUnknown=false;
 constructor(config:ManagedHomePostgresConfig,candidate:CompilerQualificationCandidate,private readonly accountId:string,private readonly commandKey:string,private readonly diagnosticsExecutable="/usr/local/bin/rudder-staging-fly"){
  this.#diagnosticToken=config.flyToken;
  check(config.policy.controlPlane?.profile==="qualification-v1"&&controlOrigin(config.policy)===QUALIFICATION_CONTROL_ORIGIN,"unconfigured");
  this.registry=createManagedCompilerSdkQualificationRegistry(config,candidate);this.wire=new ManagedHomeRegistrationWire(this.registry);
 }
 [inspect.custom](){return "CompilerSdkQualificationDriver [original disposable fixture owners retained]";}
 toJSON():never{fail("invalid_request");}
 async startRetaining():Promise<void>{check(!this.#started,"effect_unknown");this.#started=true;this.#time=originalTime(300_000);
  // Reference the actual lifetime timer. An unresolved Promise alone does not
  // retain Node/VM lifetime; the live server and this timer both do so.
  this.#fuse=setTimeout(()=>{this.#expired=true;this.#error??="expired";},300_000);
  const wss=new WebSocketServer({noServer:true,maxPayload:24*1024,perMessageDeflate:false});this.#wss=wss;
  const server=createServer((_req,res)=>{res.writeHead(404);res.end();});this.#server=server;
  server.on("error",()=>{this.#error??="unavailable";});wss.on("error",()=>{this.#error??="unavailable";});
  server.headersTimeout=5000;server.requestTimeout=5000;server.maxHeadersCount=32;server.maxConnections=16;
  server.on("upgrade",(request,socket,head)=>{if(this.#acceptingClosed||this.#pendingUpgrades>=4){socket.destroy();return;}this.#pendingUpgrades++;void(async()=>{
   check(this.#time&&!this.#expired&&this.#admissions<4,"expired");this.#time.remaining();
   check(request.method==="GET"&&request.headers.host===new URL(QUALIFICATION_CONTROL_ORIGIN).host&&request.headers["x-forwarded-proto"]==="https","identity_changed");
   const match=/^\/api\/rudder\/home\/(mh-[a-f0-9-]{36})\/control$/.exec(request.url??"");check(match,"identity_changed");
   const authorization=request.headers.authorization;check(typeof authorization==="string"&&/^Bearer rdrmh_[a-f0-9]{64}$/.test(authorization),"not_found");
   const worker=await this.registry.authenticateManagedHomeWorker(match[1],authorization.slice(7));this.#time.remaining();check(!this.#acceptingClosed,"expired");this.#admissions++;
   wss.handleUpgrade(request,socket,head,ws=>{
    // Actual upgrade socket is captured before any packet is accepted.
    this.#sockets.add(ws);this.registry.bindAuthenticatedManagedHomeSocket(ws,worker);
    ws.on("error",()=>{this.#sendUnknown=true;});ws.on("close",()=>{this.registry.closeManagedHostSocket(ws);});
    ws.on("message",data=>{if(this.#messages>=64||this.#expired)return;this.#messages++;
     let message:unknown;try{message=JSON.parse(data.toString());}catch{return;}
     void this.wire.receive(worker,message,reply=>new Promise<void>((resolve,reject)=>{
      const r=reply as {type?:string;packet?:Envelope<CompilerQualificationAck>};
      if(r.type==="runtime_image_qualification_ack_v1"){this.#ack=snapshot(r.packet!);this.#sendAttempted=true;}
      ws.send(JSON.stringify(reply),error=>{if(error){if(r.type==="runtime_image_qualification_ack_v1")this.#sendUnknown=true;reject(new Error("qualification wire send unknown"));}
       else{if(r.type==="runtime_image_qualification_ack_v1")this.#sendCallback=true;resolve();}});
     }),ws,async owner=>{await owner.assertCurrent();check(owner.socket===ws&&ws.readyState===WebSocket.OPEN,"identity_changed");}).catch(()=>{this.#error??="effect_unknown";});
    });
   });
  })().catch(()=>{socket.destroy();}).finally(()=>{this.#pendingUpgrades--;});});
  this.#listen=new Promise<void>((resolve,reject)=>{server.once("listening",resolve);server.once("error",()=>reject(new Error("qualification listen failed")));server.listen(8080,"0.0.0.0");});
  await this.#listen;this.#time.remaining();
  // Stage the actual promise before first provider/PG effects; timeout/error
  // cannot throw away its registry/cipher/key/creation/purpose/unknown owners.
  this.#provision=this.registry.provisionRuntimeImageQualification(this.accountId,this.commandKey);
  try{this.#receipt=await this.#provision;check(this.#receipt.compilerScope?.scope==="disposable-fixed-compiler-sdk-only","identity_changed");this.#time.remaining();this.#diagnosticLoop=this.#observeDiagnosticsRetaining();}catch{this.#error="effect_unknown";}
 }
 /** Logs are observations of the SAME originally created disposable machine.
  * A negative fixed Root frame stops waiting immediately; it never releases
  * any registry/socket/process owner or supplies successful qualification. */
 async #observeDiagnosticsRetaining():Promise<void>{
  try{
   const target=sdkDiagnosticTarget(this.registry.runtimeImageQualificationCustodyData(this.accountId,this.commandKey));
   while(!this.#error&&!this.#expired&&!this.#roleObserved&&this.#diagnosticReads<100){
    this.#time!.remaining();this.#diagnosticReads++;
    const read=new OriginalSdkDiagnosticRead(target,this.#time!,this.diagnosticsExecutable,this.#diagnosticToken,failure=>{
     this.#diagnosticFailure=failure;this.#error??="unavailable";
    });this.#diagnosticRead=read;
    await read.startRetaining();if(this.#error||this.#roleObserved)return;
    const delay=Math.min(2000,this.#time!.remaining());await new Promise<void>(resolve=>setTimeout(resolve,delay));
   }
  }catch{/* Missing/unavailable logs cannot become a helper result or erase original custody. */}
 }
 /** One-way listener restriction. Existing WS, registry, provider and PG
  * owners remain held; close-listener completion is NOT actor/WS retirement. */
 async closeNewListenerAdmissionRetaining():Promise<void>{
  check(this.#server&&!this.#listenerClosure,"effect_unknown");this.#acceptingClosed=true;
  const held=new StagingListenerAdmissionClosure(this.#server);this.#listenerClosure=held;await held.closeRetaining();
 }
 takeOriginalRoleActivationPermit():OriginalSdkRoleActivationPermit|undefined {
  check(!this.#error&&!this.#expired&&!this.#sendUnknown,"effect_unknown");if(!this.#sendCallback)return undefined;
  const permit=this.registry.takeOriginalRoleActivationPermit(this.accountId,this.commandKey) as OriginalSdkRoleActivationPermit|undefined;
  if(permit)this.#roleObserved=true;return permit;
 }
 /** Bounded public DATA for a private fixture output file. A send callback is
  * NOT Root ACK receipt or a readiness/lease/helper/drain constructor. */
 statusData():unknown {
  let custody:unknown=null;try{custody=this.registry.runtimeImageQualificationCustodyData(this.accountId,this.commandKey);}catch{}
  return snapshot({format_version:1,scope:"disposable-compiler-sdk-qualification-driver-data-only",
   state:this.#ack?.body.kind==="compiler_sdk_qualification_ack"&&this.#ack.body.result?.state==="published"?"sdk_role_commit_observed":this.#error??"pending",expired:this.#expired,error:this.#error??null,
   sdk_failure:this.#diagnosticFailure??null,sdk_machine_logs:this.#diagnosticRead?.statusData()??null,sdk_log_reads:this.#diagnosticReads,
   receipt:this.#receipt??null,custody,publication_ack_data:this.#ack??null,
   ack_send_attempted:this.#sendAttempted,ack_send_callback_observed:this.#sendCallback,ack_send_unknown:this.#sendUnknown,
   root_ack_receipt_observed:false,source_authority:false,helper_qualified:false,scheduler_ready:false,normal_retirement:false,
   sockets_retained:this.#sockets.size,messages_observed:this.#messages,owners_retained:true},64*1024);
 }
 /** Read-only reconciliation of exact durable original ACK/Started; no repeat
  * publication/provider mutation/registration/new clock or capability adoption. */
 async historicalResultData():Promise<unknown>{check(this.#receipt,"not_found");return this.registry.runtimeImageQualificationResultData(this.accountId,this.#receipt.scope.qualification_id);}
}
