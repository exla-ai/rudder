import { canonicalPreviewUrl, previewAccess } from "../tailscale-preview.js";

type Preview = {
  id:string;tenantId:string;projectId:string;workspaceId:string;attemptId:string;
  deviceId:string;dnsName:string;url:string;port:number;pid:number;processStart:string;protocol:"http"|"https";ready:true;
};
type Client = {request<T>(route:string,options:{method:"GET"}):Promise<T>};
const identifier=(value:unknown):value is string=>typeof value==="string" && value.length>0 && value.length<=256 && !/[\x00-\x1f\x7f]/.test(value);
function preview(value:unknown):Preview {
  const row=value as Preview;
  if(!row || !/^[a-f0-9]{64}$/.test(row.id) || ![row.tenantId,row.projectId,row.workspaceId,row.attemptId,row.deviceId,row.processStart].every(identifier)
    || row.ready!==true || !Number.isSafeInteger(row.pid) || row.pid<=1 || !["http","https"].includes(row.protocol)
    || canonicalPreviewUrl(row.url,row.dnsName,row.port)!==row.url) throw new Error("Invalid current private preview reference");
  return row;
}
const identity=(row:Preview)=>JSON.stringify([row.id,row.tenantId,row.projectId,row.workspaceId,row.attemptId,row.deviceId,row.dnsName,row.url,row.port,row.pid,row.processStart,row.protocol]);
/** Exact-reference opening for the real Cloud command. URLs are issued only by
 * fresh authenticated Home discovery and re-resolved after local ACL/readiness. */
export async function runPrivatePreviewCommand(args:readonly string[],options:{json?:boolean},dependencies:{
  client:Client;project:{repo?:string|null;name?:string};localStatus:()=>Promise<unknown>;
  reachable:(url:string)=>Promise<boolean>;open:(url:string)=>unknown;
  write?:(text:string)=>void;printJson?:(value:unknown)=>void;
}):Promise<void> {
  const positional=args.filter(arg=>arg!=="--list");
  if(args.filter(arg=>arg==="--list").length>1 || positional.length>1 || positional[0] && !/^[a-f0-9]{64}$/.test(positional[0])) {
    throw new Error("Usage: rudder cloud preview [preview-id] [--list] [--json]");
  }
  const write=dependencies.write ?? ((text:string)=>process.stdout.write(`${text}\n`));
  const state=await dependencies.client.request<{projects:unknown[]}>("/api/rudder/home",{method:"GET"});
  const projects=(Array.isArray(state.projects)?state.projects:[]).filter((raw):raw is {workspaceId:string;repo?:string;name?:string}=>{
    const row=raw as {workspaceId:string;repo?:string;name?:string};
    return identifier(row?.workspaceId) && (dependencies.project.repo
      ? typeof row.repo==="string" && row.repo.toLowerCase()===dependencies.project.repo.toLowerCase()
      : typeof dependencies.project.name==="string" && row.name===dependencies.project.name);
  });
  if(projects.length!==1) throw new Error(projects.length ? "Choose an unambiguous Cloud project before opening its preview." : "This project has no registered Cloud workspace.");
  const workspace=encodeURIComponent(projects[0]!.workspaceId);
  const result=await dependencies.client.request<{version:number;previews:unknown[]}>(`/api/app/previews?workspace=${workspace}`,{method:"GET"});
  if(result?.version!==1 || !Array.isArray(result.previews) || result.previews.length>64) throw new Error("Invalid private preview discovery");
  const rows=result.previews.map(preview);
  if(options.json) {(dependencies.printJson ?? (value=>write(JSON.stringify(value,null,2))))({version:1,previews:rows,access:"tailnet",requiresTailscale:true});return;}
  const requested=positional[0];
  if(!requested || args.includes("--list")) {
    if(!rows.length){write("No registered private preview is ready for this project.");return;}
    for(const row of rows) write(`${row.id} :${row.port} ${row.url}`);
    write("Open one with: rudder cloud preview <preview-id>");return;
  }
  const selected=rows.find(row=>row.id===requested);
  if(!selected) throw new Error("The exact private preview is no longer ready. Refresh discovery.");
  const resolve=async()=>{
    const response=await dependencies.client.request<{version:number;preview:unknown}>(`/api/app/previews/open?workspace=${workspace}&id=${requested}`,{method:"GET"});
    if(response?.version!==1) throw new Error("Invalid private preview discovery");
    const current=preview(response.preview);
    if(identity(current)!==identity(selected)) throw new Error("The private preview binding changed. Refresh discovery.");
    return current;
  };
  const current=await resolve();
  const access=previewAccess(await dependencies.localStatus(),current.dnsName,current.deviceId);
  if(!access.ok) throw new Error(access.error);
  if(!(await dependencies.reachable(current.url))) throw new Error("The private preview is not reachable with this computer's existing Tailscale access rules.");
  // The potentially slow local probe grants no durable authorization. A changed
  // owner, process token, device or project ACL must prevent browser launch.
  const final=await resolve();
  const finalAccess=previewAccess(await dependencies.localStatus(),final.dnsName,final.deviceId);
  if(!finalAccess.ok) throw new Error(finalAccess.error);
  await dependencies.open(final.url);write(final.url);
}
