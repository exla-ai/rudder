import type {IncomingMessage,ServerResponse} from "node:http";
import type {ManagedHomeRegistry} from "./registry.js";
import {ManagedHomeError,check,fail,exact} from "./types.js";
import {storageReference} from "./storage-protocol.js";
let active=0;
function reply(res:ServerResponse,status:number,value:unknown){const bytes=Buffer.from(JSON.stringify(value));check(bytes.length<=64*1024);res.writeHead(status,{"Content-Type":"application/json","Cache-Control":"no-store","Content-Length":bytes.length});res.end(bytes);}
async function missingBody(req:IncomingMessage){let size=0;const parts:Buffer[]=[];const timer=setTimeout(()=>req.destroy(),5000);try{for await(const part of req){const b=Buffer.from(part);size+=b.length;check(size<=24*1024);parts.push(b);}const v:unknown=JSON.parse(new TextDecoder("utf-8",{fatal:true}).decode(Buffer.concat(parts)));exact(v,["format_version","objects"]);check(v.format_version===1&&Array.isArray(v.objects)&&v.objects.length<=128);return v.objects;}finally{clearTimeout(timer);}}
/** Fixed account-isolated streaming path. A bearer or URL never creates the
 * Root-authenticated transfer, changes its roots, publishes or acquires owner.
 * No body bytes/credentials/driver causes are logged or saved in PG.
 */
export async function handleManagedStorageHttp(req:IncomingMessage,res:ServerResponse,url:URL,registry:ManagedHomeRegistry):Promise<boolean>{
 if(!/^\/api\/rudder\/home\/[^/]+\/fresh-roots(?:\/|$)/.test(url.pathname))return false;
 let admitted=false;try{check(active<12,"busy");active++;admitted=true;check(url.search==="");const m=/^\/api\/rudder\/home\/([A-Za-z0-9_:.\-]{1,128})\/fresh-roots\/([a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12})\/(missing|objects\/([a-z_]+)\/([a-f0-9]{64})\/(0|[1-9][0-9]{0,9}))$/.exec(url.pathname);const b=/^\/api\/rudder\/home\/([A-Za-z0-9_:.\-]{1,128})\/fresh-roots\/([a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12})\/batches\/([a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12})(\/reconcile)?$/.exec(url.pathname);check(m||b,"not_found");
  const auth=req.headers.authorization;check(typeof auth==="string"&&/^Bearer rdrmh_[a-f0-9]{64}$/.test(auth),"not_found");const worker=await registry.authenticateManagedHomeWorker((m??b!)[1],auth.slice(7));
  check(req.headers["content-encoding"]===undefined,"invalid_request");let result:unknown;
  if(b){if(b[4]){check(req.method==="POST"&&req.headers["content-type"]==="application/json");let total=0;const chunks:Buffer[]=[];const timer=setTimeout(()=>req.destroy(),5000);try{for await(const value of req){const bytes=Buffer.from(value);total+=bytes.length;check(total<=4096);chunks.push(bytes);}result=await registry.reconcileManagedImageBatch(worker,b[2],b[3],JSON.parse(Buffer.concat(chunks,total).toString("utf8")));}finally{clearTimeout(timer);}}
   else{check(req.method==="PUT"&&req.headers["content-type"]==="application/vnd.rudder.image-batch-v1"&&typeof req.headers["content-length"]==="string"&&/^[1-9][0-9]{0,8}$/.test(req.headers["content-length"])&&Number(req.headers["content-length"])<=4+262144+33554432);result=await registry.managedImageBatchPut(worker,b[2],b[3],req);}reply(res,200,result);return true;}
  check(m,"not_found");
  if(m[3]==="missing"){check(req.method==="POST"&&req.headers["content-type"]==="application/json");result=await registry.managedStorageMissing(worker,m[2],await missingBody(req));}
  else{check(req.method==="PUT"&&req.headers["content-type"]==="application/octet-stream");const object=storageReference({kind:m[4],algorithm:"sha256",digest:m[5],size_bytes:Number(m[6])});check(req.headers["content-length"]===String(object.size_bytes));result=await registry.managedStoragePut(worker,m[2],object,req);}
  reply(res,200,result);
 }catch(e){const safe=e instanceof ManagedHomeError?e:new ManagedHomeError("effect_unknown");if(!res.destroyed&&!res.headersSent)reply(res,safe.status,safe.toJSON());req.destroy?.();}finally{if(admitted)active--;}
 return true;
}
