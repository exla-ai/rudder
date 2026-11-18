/** Explicit identity carried over existing authenticated board/home transports. */
export type FileScope = {
  projectId: string;
  workspaceId: string;
  attemptId: string;
  ownerEpoch: number;
};
/** How the file service reaches a granted root. `host`: an ordinary directory
 * (the project checkout, a macOS clone, a plain jj workspace). `namespace`: a
 * Linux overlay fork, whose files exist only in its agent's mount namespace;
 * the service walks `/proc/<pid>/root` + root, the agent's own mount.
 */
export type FileAccess = { kind: "host" } | { kind: "namespace"; pid: number };
export type FileEntry = { path: string; name: string; kind: "file" | "folder"; size?: number };
export type FileListing = { path: string; entries: FileEntry[]; truncated: boolean };
export type FileDocument = {
  path: string;
  revision: string;
  mime: string;
  kind: "text" | "markdown" | "html" | "image" | "pdf" | "binary";
  text?: string;
  base64?: string;
  /** Set when the read named this revision: the content is left out. */
  unchanged?: true;
};
/** `conflict`: the file no longer holds `baseRevision`; nothing was written. */
export type FileWriteResult = { path: string; outcome: "saved"; revision: string } | { path: string; outcome: "conflict" };
export type ProjectFileReference = { scope: FileScope; path: string; kind?: "file" | "folder" };
export type ProjectFileRequest =
  | (ProjectFileReference & { version: 1; operation: "list" })
  | (ProjectFileReference & { version: 1; operation: "read"; knownRevision?: string })
  | (ProjectFileReference & { version: 1; operation: "write"; baseRevision: string; text: string });
export type ProjectFileResult = FileListing | FileDocument | FileWriteResult;
export interface ProjectFileTransport {
  list(scope: FileScope, path: string, signal: AbortSignal): Promise<FileListing>;
  read(scope: FileScope, path: string, signal: AbortSignal): Promise<FileDocument>;
}
