import type { IncomingMessage, ServerResponse } from "node:http";
import type { BoardAuthority, BoardSession } from "./authority.js";
import { engineFileGrants, sameFileScope } from "../project-files/engine.js";
import { handleProjectFiles } from "../project-files/http.js";
import { ProjectFileError, ProjectFileService } from "../project-files/service.js";

/** One bounded context per authenticated board session. Weak references release
 * expired sessions; a daemon-wide permit also bounds distinct project contexts.
 */
export class BoardProjectFiles {
  #sessions = new WeakMap<BoardSession, Map<string, ProjectFileService>>();
  #active = 0;
  constructor(private readonly grants = engineFileGrants) {}
  async handle(req: IncomingMessage, res: ServerResponse, url: URL, repo: string, authority: BoardAuthority, session: BoardSession): Promise<void> {
    res.setHeader("Cache-Control", "no-store");
    res.setHeader("X-Content-Type-Options", "nosniff");
    res.setHeader("Content-Type", "application/json; charset=utf-8");
    if (this.#active >= 8) { res.writeHead(429); res.end(JSON.stringify({ error: "Too many project files are open" })); return; }
    this.#active++;
    try {
      if (url.pathname.endsWith("/file-scopes")) {
        const controller = new AbortController();
        const cancel = () => controller.abort();
        res.once("close", cancel);
        try {
          if (req.method !== "GET") { res.writeHead(405); res.end(); return; }
          const grants = await this.grants(repo, AbortSignal.any([controller.signal, AbortSignal.timeout(3000)]));
          if (!authority.valid(session)) throw new ProjectFileError("denied", "Project file access expired");
          if (!res.destroyed) res.end(JSON.stringify({ scopes: grants.map(grant => grant.scope) }));
        } finally { res.off("close", cancel); }
        return;
      }
      let contexts = this.#sessions.get(session);
      if (!contexts) { contexts = new Map(); this.#sessions.set(session, contexts); }
      let service = contexts.get(repo);
      if (!service) {
        if (contexts.size >= 16) throw new ProjectFileError("busy", "Too many projects are open");
        service = new ProjectFileService(async (scope, signal) => {
          if (!authority.valid(session)) throw new ProjectFileError("denied", "Project file access expired");
          const grant = (await this.grants(repo, signal)).find(item => sameFileScope(item.scope, scope));
          if (!authority.valid(session) || !grant) throw new ProjectFileError("denied", "Project file access is no longer current");
          return grant;
        });
        contexts.set(repo, service);
      }
      await handleProjectFiles(req, res, url, service);
    } catch {
      if (!res.destroyed && !res.writableEnded) { res.writeHead(403); res.end(JSON.stringify({ error: "Project file access is unavailable" })); }
    } finally { this.#active--; }
  }
}
