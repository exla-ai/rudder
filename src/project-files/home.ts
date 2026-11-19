import { engineFileGrants, sameFileScope } from "./engine.js";
import { readFileRef, type FileRefPolicy } from "./file-refs.js";
import { FILE_LIMITS, ProjectFileError, ProjectFileService } from "./service.js";
import type { FileDocument, FileScope, ProjectFileRequest, ProjectFileResult } from "./types.js";

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
    return this.#bounded(async () => this.#service(repo).dispatch(request, signal));
  }
  /** A file a chat message in this project names (file-refs.ts). The home's
   * own account only: the Cloud route checks that before it asks. */
  async fileRef(repo: string, request: unknown, policy: FileRefPolicy, signal = AbortSignal.timeout(FILE_LIMITS.timeoutMs)): Promise<FileDocument> {
    return this.#bounded(async () => readFileRef(request, {
      grants: (bounded) => this.grants(repo, bounded),
      read: async (scope, path, knownRevision, bounded) => await this.#service(repo).dispatch(
        { version: 1, operation: "read", scope, path, ...(knownRevision ? { knownRevision } : {}) }, bounded) as FileDocument,
      policy,
    }, signal));
  }
  #service(repo: string): ProjectFileService {
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
    return service;
  }
  async #bounded<T>(work: () => Promise<T>): Promise<T> {
    if (this.#active >= 8) throw new ProjectFileError("busy", "Too many project files are open");
    this.#active++;
    try { return await work(); } finally { this.#active--; }
  }
  close(repo: string): void { this.#services.delete(repo); }
}
