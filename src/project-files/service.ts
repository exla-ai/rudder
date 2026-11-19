import { constants } from "node:fs";
import { lstat, open, opendir, readlink, realpath, rename, unlink } from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import path from "node:path";
import { createHash, randomUUID } from "node:crypto";
import type { FileAccess, FileDocument, FileEntry, FileListing, FileScope, FileWriteResult, ProjectFileRequest, ProjectFileResult } from "./types.js";

export class ProjectFileError extends Error {
  constructor(readonly code: "denied" | "invalid" | "missing" | "changed" | "large" | "busy" | "unsupported", message: string) {
    super(message); this.name = "ProjectFileError";
  }
}
/** `only` narrows a grant to the one file of that name at its root. */
export type FileGrant = { scope: FileScope; root: string; access: FileAccess; only?: string };
/** Resolve from CURRENT trusted project/run state, never from a client-supplied root.
 * Check session/account ACL, project/workspace/attempt membership and owner epoch.
 * Called again before publication, so revocation or ownership changes fail closed.
 */
export type AuthorizeProjectFiles = (scope: FileScope, signal: AbortSignal) => Promise<FileGrant>;
export const FILE_LIMITS = { entries: 500, textBytes: 4 * 1024 * 1024, mediaBytes: 16 * 1024 * 1024, writeBytes: 1024 * 1024, active: 8, timeoutMs: 10_000 } as const;
const REVISION = /^[a-f0-9]{64}$/;
const BINARY_TEXT = /[\x00-\x08\x0b\x0c\x0e-\x1f]/;

export function validateFileScope(value: unknown): FileScope {
  const scope = value as FileScope | undefined;
  if (!scope || ![scope.projectId, scope.workspaceId, scope.attemptId].every((v) => typeof v === "string" && v.length > 0 && v.length <= 256 && !/[\x00-\x1f\x7f]/.test(v))
    || !Number.isSafeInteger(scope.ownerEpoch) || scope.ownerEpoch < 0) throw new ProjectFileError("invalid", "Project file identity is required");
  return { projectId: scope.projectId, workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: scope.ownerEpoch };
}
export function validateFileAccess(value: unknown): FileAccess {
  const access = value as FileAccess | undefined;
  if (access?.kind === "host") return { kind: "host" };
  if (access?.kind === "namespace" && Number.isSafeInteger(access.pid) && access.pid > 1) return { kind: "namespace", pid: access.pid };
  throw new ProjectFileError("denied", "Project file access is unavailable");
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
function validateRequest(request: ProjectFileRequest): void {
  if (request?.version !== 1 || !["list", "read", "write"].includes(request.operation)) throw new ProjectFileError("invalid", "Unsupported project file request");
  if (request.operation === "read" && request.knownRevision !== undefined && !(typeof request.knownRevision === "string" && REVISION.test(request.knownRevision))) {
    throw new ProjectFileError("invalid", "Invalid known file revision");
  }
  if (request.operation === "write") {
    if (request.path === "" || typeof request.baseRevision !== "string" || !REVISION.test(request.baseRevision) || typeof request.text !== "string") {
      throw new ProjectFileError("invalid", "Invalid project file save");
    }
    if (Buffer.byteLength(request.text) > FILE_LIMITS.writeBytes) throw new ProjectFileError("large", "Files over 1 MiB cannot be edited here");
    if (BINARY_TEXT.test(request.text)) throw new ProjectFileError("invalid", "The text contains control characters");
  }
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
export function classification(relative: string): Pick<FileDocument, "mime" | "kind"> {
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
/** UTF-8 text without control characters, or null for binary content. */
export function textOf(content: Buffer): string | null {
  if (content.includes(0)) return null;
  try {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(content);
    return BINARY_TEXT.test(text) ? null : text;
  } catch { return null; }
}
export const revisionOf = (bytes: Buffer): string => createHash("sha256").update(bytes).digest("hex");

/** Where a grant's files are reachable from this process, and how to prove a
 * path still names the same mount. A namespace grant walks the agent's own
 * root (`/proc/<pid>/root`), so the service sees exactly the overlay the agent
 * writes through; the host path of a fork holds only `.jj`.
 */
type Place = {
  root: string;
  at(parts: readonly string[]): string;
  /** Host: the path is canonical. Namespace: the agent is still in the mount namespace first observed. */
  current(at: string): Promise<boolean>;
  /** A fresh grant still names this exact place. */
  holds(fresh: FileGrant): Promise<boolean>;
};
async function placeOf(grant: FileGrant): Promise<Place> {
  if (!path.isAbsolute(grant.root) || path.normalize(grant.root) !== grant.root) throw new ProjectFileError("denied", "Project file access is no longer current");
  if (grant.access.kind === "host") {
    const root = await realpath(grant.root);
    return {
      root, at: (parts) => path.join(root, ...parts),
      current: async (at) => await realpath(at) === at,
      holds: async (fresh) => fresh.access.kind === "host" && fresh.root === grant.root && fresh.only === grant.only && await realpath(fresh.root) === root,
    };
  }
  if (process.platform !== "linux") throw new ProjectFileError("denied", "Agent workspace namespaces exist only on Linux");
  const pid = grant.access.pid;
  const namespace = `/proc/${pid}/ns/mnt`;
  const mount = await readlink(namespace);
  // Until the fork's enter script has unshared, the agent still sees the host
  // mount, where its workspace holds only `.jj`. Never read that instead.
  if (mount === await readlink("/proc/self/ns/mnt")) throw new ProjectFileError("denied", "The agent workspace is not mounted yet");
  const root = `/proc/${pid}/root${grant.root}`;
  return {
    root, at: (parts) => [root, ...parts].join("/"),
    current: async () => await readlink(namespace) === mount,
    holds: async (fresh) => fresh.access.kind === "namespace" && fresh.access.pid === pid && fresh.root === grant.root && fresh.only === grant.only && await readlink(namespace) === mount,
  };
}

/** No listeners, persistent watchers, synchronous filesystem IO or scheduling. */
export class ProjectFileService {
  #active = 0;
  constructor(private readonly authorize: AuthorizeProjectFiles) {}

  async dispatch(request: ProjectFileRequest, signal?: AbortSignal): Promise<ProjectFileResult> {
    validateRequest(request);
    const scope = validateFileScope(request.scope);
    const relative = validateFilePath(request.path);
    if (this.#active >= FILE_LIMITS.active) throw new ProjectFileError("busy", "Too many project files are open");
    this.#active++;
    const bounded = AbortSignal.any([AbortSignal.timeout(FILE_LIMITS.timeoutMs), ...(signal ? [signal] : [])]);
    try {
      bounded.throwIfAborted();
      const grant = await this.#grant(scope, bounded);
      if (grant.only !== undefined && relative !== grant.only && !(relative === "" && request.operation === "list")) {
        throw new ProjectFileError("denied", "Only one file is open here");
      }
      const place = await placeOf(grant);
      const handles: FileHandle[] = [];
      try {
        // Linux resolves every component relative to pinned directory handles.
        // NOFOLLOW rejects symlinks even when an attacker swaps an ancestor.
        const rootHandle = await open(place.root, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
        handles.push(rootHandle);
        const parts = relative ? relative.split("/") : [];
        for (let index = 0; index < parts.length; index++) {
          bounded.throwIfAborted();
          const parent = handles[handles.length - 1];
          const candidate = process.platform === "linux" ? `/proc/self/fd/${parent.fd}/${parts[index]}` : place.at(parts.slice(0, index + 1));
          const directory = index < parts.length - 1 || request.operation === "list";
          const handle = await open(candidate, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK | (directory ? constants.O_DIRECTORY : 0));
          handles.push(handle);
          // Portable check also detects replacement between resolving and open.
          if (!sameFile(await handle.stat(), await lstat(place.at(parts.slice(0, index + 1))))) throw new ProjectFileError("changed", "The file changed while opening it");
        }
        const selected = handles[handles.length - 1];
        const current = place.at(parts);
        const before = await selected.stat();
        // Verify canonical ancestry and descriptor identity before trusting the result.
        const unmoved = async () => await place.current(current) && sameFile(await lstat(current), before)
          && sameFile(await lstat(place.root), await rootHandle.stat())
          && (process.platform !== "linux" || grant.access.kind !== "host" || await realpath(`/proc/self/fd/${selected.fd}`) === current);
        if (request.operation === "write") {
          const parent = handles[handles.length - 2];
          const directory = process.platform === "linux" ? `/proc/self/fd/${parent.fd}` : place.at(parts.slice(0, -1));
          return await this.write(selected, before, directory, parts[parts.length - 1], request, async () => {
            const fresh = await this.#grant(scope, bounded);
            return await place.holds(fresh) && await unmoved() && sameVersion(before, await selected.stat());
          }, bounded);
        }
        const result = request.operation === "list"
          ? await this.list(selected, current, relative, bounded, grant.only)
          : await this.read(selected, relative, request.knownRevision, bounded);
        bounded.throwIfAborted();
        if (!sameVersion(before, await selected.stat())) throw new ProjectFileError("changed", "The file changed while reading it");
        if (!await unmoved()) throw new ProjectFileError("changed", "The project file moved while reading it");
        const fresh = await this.#grant(scope, bounded);
        bounded.throwIfAborted();
        if (!await place.holds(fresh)) throw new ProjectFileError("denied", "Project file access is no longer current");
        if (!sameVersion(before, await selected.stat()) || !await unmoved()) throw new ProjectFileError("changed", "The project file changed before it could be displayed");
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

  async #grant(scope: FileScope, signal: AbortSignal): Promise<FileGrant> {
    const grant = await abortable(this.authorize(scope, signal), signal);
    signal.throwIfAborted();
    if (!sameScope(scope, grant.scope)) throw new ProjectFileError("denied", "Project file access is no longer current");
    if (grant.only !== undefined && (typeof grant.only !== "string" || validateFilePath(grant.only).includes("/") || grant.only === "")) {
      throw new ProjectFileError("denied", "Project file access is no longer current");
    }
    return { scope: grant.scope, root: grant.root, access: validateFileAccess(grant.access), ...(grant.only === undefined ? {} : { only: grant.only }) };
  }

  private async list(handle: FileHandle, absolute: string, relative: string, signal: AbortSignal, only?: string): Promise<FileListing> {
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
      if (only !== undefined && (entry.name !== only || !entry.isFile())) continue;
      entries.push({ name: entry.name, path: child, kind: entry.isDirectory() ? "folder" : "file" });
    }
    entries.sort((a, b) => (a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "folder" ? -1 : 1));
    return { path: relative, entries, truncated };
  }

  /** Allocate from checked size; never readFile a file that can grow unbounded. */
  private async bytes(handle: FileHandle, limit: number, tooLarge: string, signal: AbortSignal): Promise<Buffer> {
    const stat = await handle.stat();
    if (!stat.isFile()) throw new ProjectFileError("unsupported", "Choose a regular project file");
    if (stat.size > limit) throw new ProjectFileError("large", tooLarge);
    const bytes = Buffer.alloc(stat.size + 1);
    let offset = 0;
    while (offset < bytes.length) {
      signal.throwIfAborted();
      const read = await handle.read(bytes, offset, Math.min(64 * 1024, bytes.length - offset), offset);
      if (!read.bytesRead) break;
      offset += read.bytesRead;
    }
    if (offset !== stat.size) throw new ProjectFileError("changed", "The file changed while reading it");
    return bytes.subarray(0, offset);
  }

  private async read(handle: FileHandle, relative: string, knownRevision: string | undefined, signal: AbortSignal): Promise<FileDocument> {
    const format = classification(relative);
    const media = format.kind === "image" || format.kind === "pdf";
    const content = await this.bytes(handle, media ? FILE_LIMITS.mediaBytes : FILE_LIMITS.textBytes, "This file is too large to preview", signal);
    const revision = revisionOf(content);
    if (media) {
      if (!validMedia(content, format.mime)) throw new ProjectFileError("unsupported", "The file content does not match its media type");
      return revision === knownRevision ? { path: relative, revision, ...format, unchanged: true } : { path: relative, revision, ...format, base64: content.toString("base64") };
    }
    const text = textOf(content);
    if (text === null) return { path: relative, revision, mime: "application/octet-stream", kind: "binary", ...(revision === knownRevision ? { unchanged: true } : {}) };
    return revision === knownRevision ? { path: relative, revision, ...format, unchanged: true } : { path: relative, revision, ...format, text };
  }

  /** Replace an existing UTF-8 file only while it still holds `baseRevision`.
   * The new bytes go to a temporary file beside it, then one rename swaps them
   * in, so an agent never reads a half-written file. Through a fork's own
   * mount this lands in the agent's upper layer like any agent edit, and the
   * next snapshot records it in that agent's change.
   */
  private async write(handle: FileHandle, before: { mode: number }, directory: string, name: string,
    request: { path: string; baseRevision: string; text: string }, unchanged: () => Promise<boolean>, signal: AbortSignal): Promise<FileWriteResult> {
    const kind = classification(request.path).kind;
    if (kind === "image" || kind === "pdf") throw new ProjectFileError("unsupported", "Only text files can be edited here");
    const content = await this.bytes(handle, FILE_LIMITS.writeBytes, "Files over 1 MiB cannot be edited here", signal);
    if (textOf(content) === null) throw new ProjectFileError("unsupported", "Only UTF-8 text files can be edited here");
    if (revisionOf(content) !== request.baseRevision) return { path: request.path, outcome: "conflict" };
    const next = Buffer.from(request.text, "utf8");
    if (next.equals(content)) return { path: request.path, outcome: "saved", revision: request.baseRevision };
    const temporary = `${directory}/.rudder-save-${randomUUID()}`;
    const file = await open(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
    let placed = false;
    try {
      try {
        await file.chmod(before.mode & 0o7777);
        await file.writeFile(next);
        await file.sync();
      } finally { await file.close(); }
      signal.throwIfAborted();
      // An agent may have written between the comparison and now. The window
      // left after this check is the rename itself.
      if (!await unchanged()) return { path: request.path, outcome: "conflict" };
      await rename(temporary, `${directory}/${name}`);
      placed = true;
    } finally { if (!placed) await unlink(temporary); }
    const parent = await open(directory, constants.O_RDONLY | constants.O_DIRECTORY);
    try { await parent.sync(); } finally { await parent.close(); }
    return { path: request.path, outcome: "saved", revision: revisionOf(next) };
  }
}
