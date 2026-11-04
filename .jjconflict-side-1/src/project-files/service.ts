import { constants } from "node:fs";
import { lstat, open, opendir, realpath } from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import type { FileDocument, FileEntry, FileListing, FileScope, ProjectFileRequest, ProjectFileResult } from "./types.js";

export class ProjectFileError extends Error {
  constructor(readonly code: "denied" | "invalid" | "missing" | "changed" | "large" | "busy" | "unsupported", message: string) {
    super(message); this.name = "ProjectFileError";
  }
}
export type FileGrant = { scope: FileScope; root: string };
/** Resolve from CURRENT trusted project/run state, never from a client-supplied root.
 * Check session/account ACL, project/workspace/attempt membership and owner epoch.
 * Called again before publication, so revocation or ownership changes fail closed.
 */
export type AuthorizeProjectFiles = (scope: FileScope, signal: AbortSignal) => Promise<FileGrant>;
export const FILE_LIMITS = { entries: 500, textBytes: 4 * 1024 * 1024, mediaBytes: 16 * 1024 * 1024, active: 8, timeoutMs: 10_000 } as const;

export function validateFileScope(value: unknown): FileScope {
  const scope = value as FileScope | undefined;
  if (!scope || ![scope.projectId, scope.workspaceId, scope.attemptId].every((v) => typeof v === "string" && v.length > 0 && v.length <= 256 && !/[\x00-\x1f\x7f]/.test(v))
    || !Number.isSafeInteger(scope.ownerEpoch) || scope.ownerEpoch < 0) throw new ProjectFileError("invalid", "Project file identity is required");
  return { projectId: scope.projectId, workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: scope.ownerEpoch };
}
export function validateFilePath(value: unknown): string {
  if (typeof value !== "string" || value.length > 4096 || /[\x00-\x1f\x7f\\]/.test(value) || value.startsWith("/") || /^[a-z]:/i.test(value)) {
    throw new ProjectFileError("invalid", "Use a project-relative file path");
  }
  if (value === "") return "";
  if (value.split("/").some((part) => part === ".." || part === "." || part === "")) throw new ProjectFileError("invalid", "Invalid project file path");
  // Private infrastructure and credentials are never document assets.
  if (value.split("/").some((part) => /^\.(git|jj|rudder|rudder-workspaces|rudder-worktrees|ssh|gnupg)$/i.test(part)
    || /^\.env(?:\.|$)/i.test(part) || /^(?:RUDDER_SHARED\.md|\.credentials\.json|\.npmrc|\.netrc)$/i.test(part)
    || /\.(?:pem|key|p12|pfx)$/i.test(part))) throw new ProjectFileError("denied", "Private project metadata is not available in the viewer");
  return value;
}
function sameScope(a: FileScope, b: FileScope): boolean {
  return a.projectId === b.projectId && a.workspaceId === b.workspaceId && a.attemptId === b.attemptId && a.ownerEpoch === b.ownerEpoch;
}
async function abortable<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  signal.throwIfAborted();
  let cancel!: () => void;
  const aborted = new Promise<never>((_, reject) => {
    cancel = () => { reject(signal.reason); };
    signal.addEventListener("abort", cancel, { once: true });
  });
  try { return await Promise.race([work, aborted]); }
  finally { signal.removeEventListener("abort", cancel); }
}
function sameFile(a: { dev: number; ino: number }, b: { dev: number; ino: number }): boolean { return a.dev === b.dev && a.ino === b.ino; }
function sameVersion(a: { dev: number; ino: number; size: number; mtimeMs: number; ctimeMs: number }, b: typeof a): boolean {
  return sameFile(a, b) && a.size === b.size && a.mtimeMs === b.mtimeMs && a.ctimeMs === b.ctimeMs;
}
function classification(relative: string): Pick<FileDocument, "mime" | "kind"> {
  const ext = path.extname(relative).toLowerCase();
  const images: Record<string, string> = { ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".gif": "image/gif", ".webp": "image/webp", ".avif": "image/avif" };
  if (images[ext]) return { mime: images[ext], kind: "image" };
  if (ext === ".pdf") return { mime: "application/pdf", kind: "pdf" };
  if ([".md", ".markdown"].includes(ext)) return { mime: "text/markdown", kind: "markdown" };
  if ([".html", ".htm"].includes(ext)) return { mime: "text/html", kind: "html" };
  return { mime: ext === ".css" ? "text/css" : "text/plain", kind: "text" };
}
function validMedia(bytes: Buffer, mime: string): boolean {
  if (mime === "application/pdf") return bytes.subarray(0, 5).toString() === "%PDF-";
  if (mime === "image/png") return bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  if (mime === "image/jpeg") return bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255;
  if (mime === "image/gif") return /^GIF8[79]a$/.test(bytes.subarray(0, 6).toString());
  if (mime === "image/webp") return bytes.subarray(0, 4).toString() === "RIFF" && bytes.subarray(8, 12).toString() === "WEBP";
  if (mime === "image/avif") return bytes.subarray(4, 8).toString() === "ftyp" && /avif|avis/.test(bytes.subarray(8, 32).toString());
  return false;
}

/** No listeners, persistent watchers, synchronous filesystem IO or scheduling. */
export class ProjectFileService {
  #active = 0;
  constructor(private readonly authorize: AuthorizeProjectFiles) {}

  async dispatch(request: ProjectFileRequest, signal?: AbortSignal): Promise<ProjectFileResult> {
    if (request?.version !== 1 || !["list", "read"].includes(request.operation)) throw new ProjectFileError("invalid", "Unsupported project file request");
    const scope = validateFileScope(request.scope);
    const relative = validateFilePath(request.path);
    if (this.#active >= FILE_LIMITS.active) throw new ProjectFileError("busy", "Too many project files are open");
    this.#active++;
    const bounded = AbortSignal.any([AbortSignal.timeout(FILE_LIMITS.timeoutMs), ...(signal ? [signal] : [])]);
    try {
      bounded.throwIfAborted();
      const grant = await abortable(this.authorize(scope, bounded), bounded);
      if (!sameScope(scope, grant.scope) || !path.isAbsolute(grant.root)) throw new ProjectFileError("denied", "Project file access is no longer current");
      const root = await realpath(grant.root);
      const handles: FileHandle[] = [];
      try {
        // Linux resolves every component relative to pinned directory handles.
        // NOFOLLOW rejects symlinks even when an attacker swaps an ancestor.
        const rootHandle = await open(root, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
        handles.push(rootHandle);
        const parts = relative ? relative.split("/") : [];
        let current = root;
        for (let index = 0; index < parts.length; index++) {
          bounded.throwIfAborted();
          const parent = handles[handles.length - 1];
          const candidate = process.platform === "linux" ? `/proc/self/fd/${parent.fd}/${parts[index]}` : path.join(current, parts[index]);
          const directory = index < parts.length - 1 || request.operation === "list";
          const handle = await open(candidate, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK | (directory ? constants.O_DIRECTORY : 0));
          handles.push(handle);
          current = path.join(current, parts[index]);
          // Portable check also detects replacement between resolving and open.
          if (!sameFile(await handle.stat(), await lstat(current))) throw new ProjectFileError("changed", "The file changed while opening it");
        }
        const selected = handles[handles.length - 1];
        const before = await selected.stat();
        const result = request.operation === "list"
          ? await this.list(selected, current, relative, bounded)
          : await this.read(selected, relative, bounded);
        bounded.throwIfAborted();
        if (!sameVersion(before, await selected.stat())) throw new ProjectFileError("changed", "The file changed while reading it");
        // Verify canonical ancestry and descriptor identity again before publishing.
        if (await realpath(current) !== current || !sameFile(await lstat(current), before)
          || !sameFile(await lstat(root), await rootHandle.stat())) throw new ProjectFileError("changed", "The project file moved while reading it");
        if (process.platform === "linux" && await realpath(`/proc/self/fd/${selected.fd}`) !== current) throw new ProjectFileError("changed", "The project file moved while reading it");
        const fresh = await abortable(this.authorize(scope, bounded), bounded);
        bounded.throwIfAborted();
        if (!sameScope(scope, fresh.scope) || fresh.root !== grant.root || await realpath(fresh.root) !== root) throw new ProjectFileError("denied", "Project file access is no longer current");
        if (!sameVersion(before, await selected.stat()) || !sameFile(await lstat(current), before)
          || await realpath(current) !== current) throw new ProjectFileError("changed", "The project file changed before it could be displayed");
        return result;
      } finally { await Promise.all(handles.map((handle) => handle.close())); }
    } catch (error) {
      if (error instanceof ProjectFileError || bounded.aborted) throw error;
      const code = (error as NodeJS.ErrnoException)?.code;
      if (code === "ENOENT" || code === "ENOTDIR") throw new ProjectFileError("missing", "The project file no longer exists");
      if (code === "ELOOP" || code === "EACCES" || code === "EPERM") throw new ProjectFileError("denied", "The project file is not available");
      throw new ProjectFileError("changed", "The project file could not be read safely");
    } finally { this.#active--; }
  }

  private async list(handle: FileHandle, absolute: string, relative: string, signal: AbortSignal): Promise<FileListing> {
    if (!(await handle.stat()).isDirectory()) throw new ProjectFileError("invalid", "Choose a project folder");
    const directory = await opendir(process.platform === "linux" ? `/proc/self/fd/${handle.fd}` : absolute);
    const entries: FileEntry[] = [];
    let examined = 0;
    let truncated = false;
    for await (const entry of directory) {
      signal.throwIfAborted();
      if (++examined > FILE_LIMITS.entries) { truncated = true; break; }
      const child = relative ? `${relative}/${entry.name}` : entry.name;
      try { validateFilePath(child); } catch { continue; }
      if (entry.isSymbolicLink() || (!entry.isDirectory() && !entry.isFile())) continue;
      entries.push({ name: entry.name, path: child, kind: entry.isDirectory() ? "folder" : "file" });
    }
    entries.sort((a, b) => (a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "folder" ? -1 : 1));
    return { path: relative, entries, truncated };
  }

  private async read(handle: FileHandle, relative: string, signal: AbortSignal): Promise<FileDocument> {
    const stat = await handle.stat();
    if (!stat.isFile()) throw new ProjectFileError("unsupported", "Choose a regular project file");
    const format = classification(relative);
    const media = format.kind === "image" || format.kind === "pdf";
    const limit = media ? FILE_LIMITS.mediaBytes : FILE_LIMITS.textBytes;
    if (stat.size > limit) throw new ProjectFileError("large", "This file is too large to preview");
    // Allocate from checked size; never readFile a file that can grow unbounded.
    const bytes = Buffer.alloc(stat.size + 1);
    let offset = 0;
    while (offset < bytes.length) {
      signal.throwIfAborted();
      const read = await handle.read(bytes, offset, Math.min(64 * 1024, bytes.length - offset), offset);
      if (!read.bytesRead) break;
      offset += read.bytesRead;
    }
    if (offset !== stat.size) throw new ProjectFileError("changed", "The file changed while reading it");
    const content = bytes.subarray(0, offset);
    const revision = createHash("sha256").update(content).digest("hex");
    if (media) {
      if (!validMedia(content, format.mime)) throw new ProjectFileError("unsupported", "The file content does not match its media type");
      return { path: relative, revision, ...format, base64: content.toString("base64") };
    }
    try {
      if (content.includes(0)) throw new Error("binary");
      const text = new TextDecoder("utf-8", { fatal: true }).decode(content);
      if (/[\x00-\x08\x0b\x0c\x0e-\x1f]/.test(text)) throw new Error("binary");
      return { path: relative, revision, ...format, text };
    } catch { return { path: relative, revision, mime: "application/octet-stream", kind: "binary" }; }
  }
}
