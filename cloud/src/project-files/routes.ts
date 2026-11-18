import type { IncomingMessage, ServerResponse } from "node:http";
import { CloudProjectFiles, CloudProjectFileError, WRITE_TEXT_BYTES, type CloudFileScope } from "./relay.js";

type Dependencies = {
  workspace(accountId: string, workspaceId: string): { homeId: string };
  rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};
type Binding = { scope: CloudFileScope; accountId: string; homeId: string; workspaceId: string; expires: number };
const key = (scope: CloudFileScope, accountId: string): string => JSON.stringify([accountId, scope.projectId, scope.workspaceId, scope.attemptId, scope.ownerEpoch]);
function validScope(value: unknown): value is CloudFileScope {
  const scope = value as CloudFileScope;
  return Boolean(scope && [scope.projectId, scope.workspaceId, scope.attemptId].every(value => typeof value === "string" && value.length > 0 && value.length <= 256 && !/[\x00-\x1f\x7f]/.test(value))
    && Number.isSafeInteger(scope.ownerEpoch) && scope.ownerEpoch >= 0);
}

/** A save is one file's text (WRITE_TEXT_BYTES of UTF-8) as JSON; escaping at
 * most doubles it. */
const SAVE_BODY_BYTES = 2 * WRITE_TEXT_BYTES + 64 * 1024;
async function saveBody(req: IncomingMessage, signal: AbortSignal): Promise<unknown> {
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    signal.throwIfAborted();
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    total += bytes.length;
    if (total > SAVE_BODY_BYTES) { req.destroy(); throw new CloudProjectFileError(413, "Files over 1 MiB cannot be edited here"); }
    chunks.push(bytes);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); }
  catch { throw new CloudProjectFileError(400, "Send the save as JSON"); }
}

/** The Cloud repository id routes RPC; FileScope.workspaceId remains the actual
 * execution workspace id. Keep that distinction rather than redefining either.
 * Cached bindings only route reads. The home reauthorizes actual custody twice.
 */
export class ProjectFileRoutes {
  #bindings = new Map<string, Binding>();
  #relay: CloudProjectFiles<string>;
  #listing = 0;
  constructor(private readonly dependencies: Dependencies) {
    this.#relay = new CloudProjectFiles({
      authorize: async (accountId, scope, signal) => {
        signal.throwIfAborted();
        const binding = this.#binding(accountId, scope);
        return { scope, homeId: binding.homeId };
      },
      rpc: (homeId, method, request, signal, accountId) => {
        // authorize immediately precedes this callback. The binding is keyed
        // by scope and was supplied only by the selected authenticated home.
        const binding = this.#binding(accountId, request.scope);
        if (binding.homeId !== homeId) throw new CloudProjectFileError(403, "Project file access changed");
        return this.dependencies.rpc(homeId, method, { workspaceId: binding.workspaceId, request }, signal);
      },
    });
  }
  #binding(accountId: string, scope: CloudFileScope): Binding {
    const binding = this.#bindings.get(key(scope, accountId));
    if (!binding || binding.accountId !== accountId || binding.expires <= Date.now()
      || this.dependencies.workspace(accountId, binding.workspaceId).homeId !== binding.homeId) {
      throw new CloudProjectFileError(403, "Refresh this project's file workspaces");
    }
    return binding;
  }
  async handle(req: IncomingMessage, res: ServerResponse, url: URL, accountId: string): Promise<boolean> {
    if (!["/api/app/project-files", "/api/app/project-file-scopes"].includes(url.pathname)) return false;
    res.setHeader("Cache-Control", "no-store");
    res.setHeader("Content-Type", "application/json; charset=utf-8");
    res.setHeader("X-Content-Type-Options", "nosniff");
    const controller = new AbortController();
    const cancel = () => controller.abort();
    req.once("aborted", cancel); res.once("close", cancel);
    try {
      const save = req.method === "POST" && url.pathname === "/api/app/project-files";
      if (req.method !== "GET" && !save) throw new CloudProjectFileError(405, "Use GET to read project files and POST to save one");
      const signal = AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]);
      let result: unknown;
      if (save) {
        if (url.search || !/^application\/json(?:;|$)/i.test(req.headers["content-type"] ?? "")) throw new CloudProjectFileError(400, "Send the save as JSON");
        result = await this.#relay.save(accountId, await saveBody(req, signal), signal);
      } else if (url.pathname.endsWith("project-file-scopes")) {
        if (this.#listing >= 8) throw new CloudProjectFileError(429, "Too many projects are open");
        const workspaceId = url.searchParams.get("workspaceId") ?? "";
        if (!workspaceId || workspaceId.length > 256 || [...url.searchParams.keys()].some(key => key !== "workspaceId")
          || url.searchParams.getAll("workspaceId").length !== 1) throw new CloudProjectFileError(400, "Select a project workspace");
        const { homeId } = this.dependencies.workspace(accountId, workspaceId);
        this.#listing++;
        let scopes: unknown;
        try { scopes = await this.dependencies.rpc(homeId, "projectFileScopes", { workspaceId }, signal); }
        finally { this.#listing--; }
        if (signal.aborted || this.dependencies.workspace(accountId, workspaceId).homeId !== homeId
          || !Array.isArray(scopes) || scopes.length > 16 || !scopes.every(validScope)) throw new CloudProjectFileError(403, "Project file access changed");
        for (const [id, item] of this.#bindings) {
          if (item.expires <= Date.now() || item.accountId === accountId && item.workspaceId === workspaceId) this.#bindings.delete(id);
        }
        if (this.#bindings.size + scopes.length > 1024) throw new CloudProjectFileError(429, "Too many file workspaces are open");
        for (const scope of scopes) {
          const prior = this.#bindings.get(key(scope, accountId));
          if (prior && (prior.accountId !== accountId || prior.homeId !== homeId || prior.workspaceId !== workspaceId)) throw new CloudProjectFileError(403, "Project file identity conflicts");
          this.#bindings.set(key(scope, accountId), { scope, accountId, workspaceId, homeId, expires: Date.now() + 60_000 });
        }
        result = { scopes };
      } else {
        result = await this.#relay.query(accountId, url, signal);
      }
      if (!controller.signal.aborted && !res.destroyed) res.end(JSON.stringify(result));
    } catch (error) {
      if (!controller.signal.aborted && !res.destroyed) {
        res.statusCode = error instanceof CloudProjectFileError ? error.status : 403;
        res.end(JSON.stringify({ error: error instanceof CloudProjectFileError ? error.message : "Project file access is unavailable" }));
      }
    } finally { req.off("aborted", cancel); res.off("close", cancel); }
    return true;
  }
}
