import type {IncomingMessage,ServerResponse} from "node:http";
import {ManagedHomeError,check,fail,exact,key} from "./types.js";
import type {ManagedHomeRegistry} from "./registry.js";
export interface ManagedHomeProvisioningHttpDependencies {registry:ManagedHomeRegistry;authenticate:(req:IncomingMessage)=>Promise<{accountId:string}>;}
async function body(req:IncomingMessage):Promise<Record<string,unknown>>{let bytes=0;const chunks:Buffer[]=[];try{for await(const part of req){const b=Buffer.isBuffer(part)?part:Buffer.from(part);bytes+=b.length;check(bytes<=4096);chunks.push(b);}const value:unknown=JSON.parse(Buffer.concat(chunks).toString("utf8")||"{}");exact(value,["command_key","new_project_names"]);return value;}catch{fail("invalid_request");}}
function reply(res:ServerResponse,status:number,value:unknown){const bytes=Buffer.from(JSON.stringify(value));res.writeHead(status,{"Content-Type":"application/json; charset=utf-8","Cache-Control":"no-store","Content-Length":bytes.length});res.end(bytes);}
/** Actual authenticated endpoint -> producer. Server registration is supplied
 * separately; no automatic new-home/legacy migration or default activation. */
export async function handleManagedHomeProvisioningHttp(req:IncomingMessage,res:ServerResponse,url:URL,deps:ManagedHomeProvisioningHttpDependencies):Promise<boolean>{
  const root="/api/app/managed-homes/provision";if(url.pathname!==root&&!url.pathname.startsWith(root+"/"))return false;
  try{let value:unknown;const account=await deps.authenticate(req);const tail=url.pathname.slice(root.length);
    if(req.method==="POST"&&tail===""){const input=await body(req);key(input.command_key);check(Array.isArray(input.new_project_names)&&input.new_project_names.length>0&&input.new_project_names.length<=8&&input.new_project_names.every(v=>typeof v==="string"));
      value=await deps.registry.provisionManagedHome(account.accountId,input.command_key,input.new_project_names as string[]);}
    else{const match=/^\/([A-Za-z0-9_:.\-]{1,128})(\/reconcile)?$/.exec(tail);check(match,"not_found");
      if(req.method==="GET"&&!match[2])value=await deps.registry.getProvisioningReceipt(account.accountId,match[1]);
      else if(req.method==="POST"&&match[2]){const input=await body(req);check(Object.keys(input).length===0);value=await deps.registry.reconcileProvisioning(account.accountId,match[1]);}
      else fail("not_found");}
    reply(res,200,value);
  }catch(e){const safe=e instanceof ManagedHomeError?e:new ManagedHomeError("unavailable");reply(res,safe.status,safe.toJSON());}
  return true;
}
