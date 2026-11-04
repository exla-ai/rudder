/** Explicit identity carried over existing authenticated board/home transports. */
export type FileScope = {
  projectId: string;
  workspaceId: string;
  attemptId: string;
  ownerEpoch: number;
};
export type FileEntry = { path: string; name: string; kind: "file" | "folder"; size?: number };
export type FileListing = { path: string; entries: FileEntry[]; truncated: boolean };
export type FileDocument = {
  path: string;
  revision: string;
  mime: string;
  kind: "text" | "markdown" | "html" | "image" | "pdf" | "binary";
  text?: string;
  base64?: string;
};
export type ProjectFileReference = { scope: FileScope; path: string; kind?: "file" | "folder" };
export type ProjectFileRequest = ProjectFileReference & { version: 1; operation: "list" | "read" };
export type ProjectFileResult = FileListing | FileDocument;
export interface ProjectFileTransport {
  list(scope: FileScope, path: string, signal: AbortSignal): Promise<FileListing>;
  read(scope: FileScope, path: string, signal: AbortSignal): Promise<FileDocument>;
}
