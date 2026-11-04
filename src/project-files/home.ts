import { engineFileGrants, sameFileScope } from "./engine.js";
import { ProjectFileError, ProjectFileService } from "./service.js";
import type { FileScope, ProjectFileRequest, ProjectFileResult } from "./types.js";

/** Used only by the authenticated home RPC receiver, after selecting its own
 * registered project. No browser path or control-plane cwd chooses the root.
 */
export class HomeProjectFiles {
  #services = new Map<string, ProjectFileService>();
  #active = 0;
  constructor(private readonly grants = engineFileGrants) {}
  async scopes(repo: string, signal = AbortSignal.timeout(3000)): Promise<FileScope[]> {
    return this.#bounded(async () => (await this.grants(repo, signal)).map(grant => grant.scope));
  }
  async dispatch(repo: string, request: ProjectFileRequest, signal?: AbortSignal): Promise<ProjectFileResult> {
    return this.#bounded(async () => {
      let service = this.#services.get(repo);
      if (!service) {
        if (this.#services.size >= 64) throw new ProjectFileError("busy", "Too many file workspaces are open");
        service = new ProjectFileService(async (scope, bounded) => {
          const grant = (await this.grants(repo, bounded)).find(item => sameFileScope(scope, item.scope));
          if (!grant) throw new ProjectFileError("denied", "Project file access is no longer current");
          return grant;
        });
        this.#services.set(repo, service);
      }
      return service.dispatch(request, signal);
    });
  }
  async #bounded<T>(work: () => Promise<T>): Promise<T> {
    if (this.#active >= 8) throw new ProjectFileError("busy", "Too many project files are open");
    this.#active++;
    try { return await work(); } finally { this.#active--; }
  }
  close(repo: string): void { this.#services.delete(repo); }
}
