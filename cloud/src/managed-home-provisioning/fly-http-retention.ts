/** Private original HTTP owners. Recovery summaries never contain bodies,
 * headers, exception messages or credential values, and cannot authorize effects. */
import { inspect } from "node:util";
import {flyProviderHttpTimeoutLimit,isFlyManagedHomeLifecycle} from "../fly-custody.js";
import { check, fail, hash } from "./types.js";
import type { OriginalTime, ProviderReply, ProviderTransport, ProviderRequestContext } from "./fly.js";

type Method="GET"|"POST"|"DELETE";
type Classification="pending"|"ok"|"provider_non_2xx"|"request_invalid"|"request_timeout"|"original_cancelled"|"original_expired"|"fetch_error"|"response_error"|"reply_read_error"|"reply_too_large"|"reply_invalid_json";
type HeldRequest={number:number;method:Method;path:string;app:string|null;abort:AbortController;timer?:ReturnType<typeof setTimeout>;
  // These are the EXACT argument object, Response and reader used by fetch/read.
  request?:{url:string;init:RequestInit};requestBytes?:Buffer;fetch?:Promise<Response>;fetchSettled:boolean;
  response?:Response;reader?:ReadableStreamDefaultReader<Uint8Array>;read?:Promise<ReadableStreamReadResult<Uint8Array>>;
  cancel?:Promise<void>;cancelSettled:boolean;cancelError?:unknown;chunks:Buffer[];size:number;eof:boolean;reply?:ProviderReply;
  originalSignal?:AbortSignal;originalAbortListener?:()=>void;originalAborted:boolean;
  classification:Classification;error?:unknown;timedOut:boolean;returned:boolean;dispatched:boolean};
const LIMIT=128*1024,MAX_ORIGINAL_REQUESTS=128,MAX_DEFAULT_ORIGINALS=16;
const appName=(v:unknown):v is string=>typeof v==="string"&&/^[a-z0-9][a-z0-9-]{0,62}$/.test(v);
function scopedApp(path:string,body:unknown):string|null{
  const m=/^\/v1\/apps\/([^/?]+)(?:[/?]|$)/.exec(path);if(m&&appName(m[1]))return m[1];
  if(/^\/v1\/apps(?:\?|$)/.test(path)&&body&&typeof body==="object"&&appName((body as {name?:unknown}).name))return (body as {name:string}).name;
  return null;
}
function safePath(path:string):string{
  // Existing callers use fixed resource paths; never export query values or an
  // unrecognized route, which could contain a caller-supplied secret string.
  const p=path.split("?",1)[0];return /^\/v1\/apps(?:\/[a-z0-9][a-z0-9-]{0,62}(?:\/(?:machines|volumes|secrets)(?:\/[A-Za-z0-9_-]{1,128})?(?:\/(?:start|stop|suspend|cordon|uncordon|lease|wait))?)?)?$/.test(p)?p:"/v1/apps/[redacted-path]";
}
export class FlyHttpTransport implements ProviderTransport {
  #token:string;#requests=0;#retained=new Map<number,HeldRequest>();
  constructor(token:string,private readonly maximumRequests?:number){if(maximumRequests!==undefined)check(Number.isInteger(maximumRequests)&&maximumRequests>=1&&maximumRequests<=128);check(typeof token==="string"&&token.length>20&&token.length<=16384&&!/[\r\n\0]/.test(token));this.#token=token;}
  requestBudgetData(){return {maximum:this.maximumRequests??null,attempted:this.#requests};}
  [inspect.custom](){return "FlyHttpTransport [private retained original HTTP owners]";}
  toJSON():never{fail("invalid_request");}
  /** Qualification-only caller supplies the actual intent's app. This method
   * returns mechanically redacted DATA; no original owner can be reconstructed. */
  originalResponseCustodyData(app:string){check(appName(app));return [...this.#retained.values()].filter(h=>h.app===app).map(h=>({request_number:h.number,method:h.method,path:safePath(h.path),status:h.response?.status??null,
    body_sha256:h.response?hash(Buffer.concat(h.chunks,h.size)):null,body_complete:h.eof,classification:h.classification,
    settlement:(!h.dispatched||h.fetchSettled)&&!h.read&&(!h.cancel||h.cancelSettled)?"settled":"pending"}));}
  /** Explicitly called only after the original provisioning owner validates all
   * acknowledgments. Unknown/error/pending records remain charged and private. */
  archiveKnownCompleted(app:string):void{check(appName(app));const rows=[...this.#retained.values()].filter(h=>h.app===app);
    for(const h of rows)check(h.returned&&h.fetchSettled&&!h.read&&!h.cancel&&h.eof&&h.classification==="ok"&&h.reply,"effect_unknown");
    for(const h of rows){h.reader?.releaseLock();h.requestBytes?.fill(0);for(const b of h.chunks)b.fill(0);this.#retained.delete(h.number);}
  }
  #cancel(h:HeldRequest):void{if(!h.reader||h.eof||h.cancel)return;
    try{h.cancel=h.reader.cancel();h.cancel.then(()=>{h.cancelSettled=true;},error=>{h.cancelError=error;h.cancelSettled=true;});}
    catch(error){h.cancelError=error;h.cancelSettled=true;}
  }
  #guard(h:HeldRequest,time:OriginalTime):void{if(h.timedOut){h.classification="request_timeout";fail("effect_unknown");}
    if(h.originalAborted||h.originalSignal?.aborted){h.classification="original_cancelled";fail("effect_unknown");}
    try{time.remaining();}catch(error){h.classification="original_expired";throw error;}}
  async request(method:Method,path:string,body:unknown,time:OriginalTime,headers:Record<string,string>={},context?:ProviderRequestContext):Promise<ProviderReply>{
    // An explicit qualification campaign retains its ORIGINAL lifetime budget,
    // even after known-complete archival. The default service instead retains
    // up to the registry's16 original app carriers, each bounded independently.
    check(this.maximumRequests===undefined||this.#requests<this.maximumRequests,"busy");
    let app:string|null;try{app=scopedApp(path,body);}catch{fail("invalid_request");}
    if(this.maximumRequests===undefined){const groups=new Map<string|null,number>();
      for(const held of this.#retained.values())groups.set(held.app,(groups.get(held.app)??0)+1);
      // Unidentified requests consume one group too; they cannot bypass bounds.
      check((groups.get(app)??0)<MAX_ORIGINAL_REQUESTS&&(groups.has(app)||groups.size<MAX_DEFAULT_ORIGINALS),"busy");
    }else check(this.#retained.size<MAX_ORIGINAL_REQUESTS,"busy");
    // Scope admission and reservation are synchronous, before any effect/await.
    // Stage before serialization, fetch, response parsing or post-validation.
    const h:HeldRequest={number:++this.#requests,method,path,app,abort:new AbortController(),fetchSettled:false,cancelSettled:false,chunks:[],size:0,eof:false,classification:"pending",originalAborted:false,timedOut:false,returned:false,dispatched:false};this.#retained.set(h.number,h);
    let stage:"request"|"fetch"|"response"|"read"|"parse"="request";
    try{
      if(context!==undefined){check(context!==null&&typeof context==="object"&&context.signal instanceof AbortSignal);
        check(context.lifecycleProfile===undefined||isFlyManagedHomeLifecycle(context.lifecycleProfile));
        h.originalSignal=context.signal;h.originalAbortListener=()=>{h.originalAborted=true;h.classification="original_cancelled";h.abort.abort();this.#cancel(h);};
        context.signal.addEventListener("abort",h.originalAbortListener,{once:true});if(context.signal.aborted)h.originalAbortListener();}
      this.#guard(h,time);
      check(path.startsWith("/v1/apps")&&!/[\r\n\0]/.test(path));
      let wire:string|undefined;if(body!==undefined){wire=JSON.stringify(body);check(typeof wire==="string"&&Buffer.byteLength(wire)<=LIMIT);h.requestBytes=Buffer.from(wire);}
      const milliseconds=Math.min(flyProviderHttpTimeoutLimit(context?.lifecycleProfile,method,path),time.remaining());h.timer=setTimeout(()=>{h.timedOut=true;h.classification="request_timeout";h.abort.abort();},milliseconds);
      h.request={url:"https://api.machines.dev"+path,init:{method,redirect:"error",signal:h.abort.signal,headers:{Authorization:`Bearer ${this.#token}`,"Content-Type":"application/json",...headers},...(wire===undefined?{}:{body:wire})}};
      stage="fetch";h.dispatched=true;h.fetch=fetch(h.request.url,h.request.init);
      try{h.response=await h.fetch;}finally{h.fetchSettled=true;}
      // Preserve Response/reader BEFORE any deadline/body/schema postcheck.
      stage="response";h.reader=h.response.body?.getReader();this.#guard(h,time);
      if(h.reader){stage="read";for(;;){h.read=h.reader.read();let part:ReadableStreamReadResult<Uint8Array>;
        try{part=await h.read;}finally{h.read=undefined;}
        if(part.done){h.eof=true;break;}
        const remaining=LIMIT-h.size,take=Math.min(remaining,part.value.byteLength);if(take){h.chunks.push(Buffer.from(part.value.subarray(0,take)));h.size+=take;}
        if(part.value.byteLength>remaining){h.classification="reply_too_large";fail("effect_unknown");}this.#guard(h,time);
      }}else h.eof=true;
      this.#guard(h,time);stage="parse";const bytes=Buffer.concat(h.chunks,h.size);let value:unknown={};
      if(bytes.length){try{value=JSON.parse(bytes.toString("utf8"));}catch(error){h.classification="reply_invalid_json";throw error;}}
      h.reply={status:h.response.status,body:value};this.#guard(h,time);h.classification=h.response.status>=200&&h.response.status<300?"ok":"provider_non_2xx";return h.reply;
    }catch(error){h.error=error;if(h.timedOut)h.classification="request_timeout";
      else if(h.originalAborted)h.classification="original_cancelled";
      else if(h.classification==="pending")h.classification=stage==="request"?"request_invalid":stage==="fetch"?"fetch_error":stage==="read"?"reply_read_error":"response_error";
      h.abort.abort();this.#cancel(h);return fail("effect_unknown");
    }finally{h.returned=true;clearTimeout(h.timer);if(h.originalSignal&&h.originalAbortListener)h.originalSignal.removeEventListener("abort",h.originalAbortListener);}
  }
}
