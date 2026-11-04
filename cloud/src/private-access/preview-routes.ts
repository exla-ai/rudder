import type { IncomingMessage, ServerResponse } from "node:http";
import { AuthorizedPreviewReports, type PreviewReport, type PreviewScope } from "./preview-access.js";

export class PrivatePreviewError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}
const identifier = (value: unknown): value is string => typeof value === "string" && value.length > 0
  && value.length <= 256 && !/[\x00-\x1f\x7f]/.test(value);
/** Actor and Cloud repository select the authenticated Home connection. Native
 * process records supply the run scope within that Home, never the browser.
 * All ready URLs are re-observed through that current connection before open. */
export class PrivatePreviewRoutes {
  #active = 0;
  constructor(readonly dependencies: {
    workspace(actor: string, workspaceId: string): { homeId: string };
    rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
  }) {}
  async #bounded<T>(work: () => Promise<T>): Promise<T> {
    if (this.#active >= 8) throw new PrivatePreviewError(429,"Too many private previews are open");
    this.#active++;
    try { return await work(); } finally { this.#active--; }
  }
  async list(actor: string, workspaceId: string): Promise<PreviewReport[]> {
    return this.#bounded(async () => {
      if (!identifier(workspaceId)) throw new PrivatePreviewError(400,"Select a project workspace");
      const { homeId } = this.dependencies.workspace(actor,workspaceId);
      const raw = await this.dependencies.rpc(homeId,"previews",{workspaceId},AbortSignal.timeout(10_000));
      if (this.dependencies.workspace(actor,workspaceId).homeId !== homeId) throw new PrivatePreviewError(409,"Project Home changed");
      if (!Array.isArray(raw) || raw.length > 64) throw new PrivatePreviewError(502,"Invalid private preview discovery");
      const reports = new AuthorizedPreviewReports<string>(async current => {
        try { return this.dependencies.workspace(current,workspaceId).homeId === homeId; } catch { return false; }
      });
      const scopes = new Map<string, {scope:PreviewScope;rows:unknown[]}>();
      for (const value of raw) {
        const row = value as Partial<PreviewReport>;
        // Tenant comes from authenticated Home custody. A payload cannot report
        // another Home's tenant or replace the selected Cloud repository.
        if (!row || row.tenantId !== homeId || !identifier(row.projectId) || !identifier(row.workspaceId) || !identifier(row.attemptId)) continue;
        const scope = {tenantId:homeId,projectId:row.projectId,workspaceId:row.workspaceId,attemptId:row.attemptId};
        const key = JSON.stringify(scope);
        const group = scopes.get(key) ?? {scope,rows:[]}; group.rows.push(row); scopes.set(key,group);
      }
      const output: PreviewReport[] = [];
      for (const {scope,rows} of scopes.values()) {
        reports.receive(scope,rows);
        output.push(...await reports.list(actor,scope));
      }
      return output;
    });
  }
  async resolve(actor: string, workspaceId: string, id: string): Promise<PreviewReport> {
    if (!/^[a-f0-9]{64}$/.test(id)) throw new PrivatePreviewError(400,"Select an exact private preview reference");
    const row = (await this.list(actor,workspaceId)).find(report => report.id === id);
    if (!row) throw new PrivatePreviewError(409,"The private preview is no longer ready. Refresh discovery.");
    return row;
  }
  async register(actor: string, workspaceId: string, request: unknown): Promise<unknown> {
    return this.#bounded(async () => {
      if (!identifier(workspaceId) || Buffer.byteLength(JSON.stringify(request) ?? "") > 4096) throw new PrivatePreviewError(400,"Invalid preview registration");
      const {homeId} = this.dependencies.workspace(actor,workspaceId);
      const result = await this.dependencies.rpc(homeId,"previewRegister",{workspaceId,request},AbortSignal.timeout(5000));
      if (this.dependencies.workspace(actor,workspaceId).homeId !== homeId) throw new PrivatePreviewError(409,"Project Home changed");
      return result;
    });
  }
  async handle(req: IncomingMessage,res: ServerResponse,url: URL,actor: string): Promise<boolean> {
    if (!["/api/app/previews","/api/app/previews/open","/api/app/previews/register"].includes(url.pathname)) return false;
    const send = (status:number,value:unknown) => { res.writeHead(status,{"content-type":"application/json","cache-control":"no-store"});res.end(JSON.stringify(value)); };
    try {
      const allowed = url.pathname.endsWith("/open") ? ["workspace","id"] : ["workspace"];
      if ([...url.searchParams.keys()].some(key => !allowed.includes(key)) || allowed.some(key => url.searchParams.getAll(key).length > 1)) {
        throw new PrivatePreviewError(400,"Invalid private preview selection");
      }
      const workspace = url.searchParams.get("workspace") ?? "";
      if (req.method === "GET" && url.pathname === "/api/app/previews") send(200,{version:1,previews:await this.list(actor,workspace)});
      else if (req.method === "GET" && url.pathname.endsWith("/open")) send(200,{version:1,preview:await this.resolve(actor,workspace,url.searchParams.get("id") ?? "")});
      else if (req.method === "POST" && url.pathname.endsWith("/register")) {
        const chunks: Buffer[] = []; let size=0;
        const deadline = setTimeout(() => req.destroy(new Error("Preview registration request timed out")),3000);
        try {
          for await (const chunk of req) { const bytes=Buffer.from(chunk);size+=bytes.length;if(size>4096)throw new PrivatePreviewError(413,"Preview registration exceeds bound");chunks.push(bytes); }
        } finally { clearTimeout(deadline); }
        let request: unknown;try{request=JSON.parse(Buffer.concat(chunks).toString("utf8"));}catch{throw new PrivatePreviewError(400,"Invalid preview registration");}
        send(200,{version:1,preview:await this.register(actor,workspace,request)});
      } else send(405,{error:"Method not allowed"});
    } catch(error) { send(error instanceof PrivatePreviewError ? error.status : typeof (error as {status?:unknown})?.status === "number" ? (error as {status:number}).status : 403,{error:error instanceof Error ? error.message : "Private preview access denied"}); }
    return true;
  }
}
