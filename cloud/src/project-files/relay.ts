import type { IncomingMessage } from "node:http";

/** Structural v1 wire contract mirrors src/project-files/types.ts. Cloud never
 * resolves client file paths on the control plane and never receives a root.
 * Register behind existing account auth and requireHomeRepo project ACL checks.
 */
export type CloudFileScope = { projectId: string; workspaceId: string; attemptId: string; ownerEpoch: number };
export type CloudFileRequest =
  | { version: 1; operation: "list"; scope: CloudFileScope; path: string }
  | { version: 1; operation: "read"; scope: CloudFileScope; path: string; knownRevision?: string }
  | { version: 1; operation: "write"; scope: CloudFileScope; path: string; baseRevision: string; text: string };
export type CloudFileGrant = { scope: CloudFileScope; homeId: string };
export type CloudFileDependencies<Actor> = {
  authorize: (actor: Actor, scope: CloudFileScope, signal: AbortSignal) => Promise<CloudFileGrant>;
  /** Existing authenticated private home RPC. Propagate abort to its pending
   * receipt, bound its response, and dispatch to ProjectFileService on the home.
   */
  rpc: (homeId: string, operation: "projectFiles", request: CloudFileRequest, signal: AbortSignal, actor: Actor) => Promise<unknown>;
};
export class CloudProjectFileError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}
/** Saves carry at most this much UTF-8 text (FILE_LIMITS.writeBytes on the home). */
export const WRITE_TEXT_BYTES = 1024 * 1024;
/** A save is one file's text as JSON; escaping at most doubles it. */
export const SAVE_BODY_BYTES = 2 * WRITE_TEXT_BYTES + 64 * 1024;
export const SAVE_TOO_LARGE = "Files over 1 MiB cannot be edited here";
/** A JSON request body of at most `limit` bytes. */
export async function jsonBody(req: IncomingMessage, limit: number, tooLarge: string, signal: AbortSignal): Promise<unknown> {
  if (!/^application\/json(?:;|$)/i.test(req.headers["content-type"] ?? "")) throw new CloudProjectFileError(400, "Send JSON");
  const chunks: Buffer[] = [];
  let total = 0;
  for await (const chunk of req) {
    signal.throwIfAborted();
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    if ((total += bytes.length) > limit) { req.destroy(); throw new CloudProjectFileError(413, tooLarge); }
    chunks.push(bytes);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString("utf8")); } catch { throw new CloudProjectFileError(400, "Send JSON"); }
}
const MEDIA = new Set(["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif"]);
const REVISION = /^[a-f0-9]{64}$/;
function sameScope(a: CloudFileScope, b: CloudFileScope): boolean {
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
function validIdentity(scope: CloudFileScope, relative: string): boolean {
  return [scope.projectId, scope.workspaceId, scope.attemptId].every((v) => typeof v === "string" && v.length > 0 && v.length <= 256 && !/[\x00-\x1f\x7f]/.test(v))
    && Number.isSafeInteger(scope.ownerEpoch) && scope.ownerEpoch >= 0 && typeof relative === "string" && relative.length <= 4096 && !/[\x00-\x1f\x7f\\]/.test(relative)
    && !relative.startsWith("/") && !/^[a-z]:/i.test(relative) && (relative === "" || !relative.split("/").some((p) => p === "." || p === ".." || p === ""));
}
function requestFrom(url: URL): CloudFileRequest {
  const q = url.searchParams;
  const fields = ["version", "operation", "projectId", "workspaceId", "attemptId", "ownerEpoch", "path"];
  if (url.search.length > 8192 || [...q.keys()].some((k) => !fields.includes(k) && k !== "knownRevision") || fields.some((k) => q.getAll(k).length !== 1)
    || q.getAll("knownRevision").length > 1 || q.get("version") !== "1" || !["list", "read"].includes(q.get("operation") ?? "")
    || !/^(0|[1-9][0-9]*)$/.test(q.get("ownerEpoch") ?? "")) throw new CloudProjectFileError(400, "Invalid project file request");
  const scope = { projectId: q.get("projectId")!, workspaceId: q.get("workspaceId")!, attemptId: q.get("attemptId")!, ownerEpoch: Number(q.get("ownerEpoch")) };
  const relative = q.get("path")!;
  if (!validIdentity(scope, relative)) throw new CloudProjectFileError(400, "Invalid project file identity or path");
  if (q.get("operation") === "list") {
    if (q.has("knownRevision")) throw new CloudProjectFileError(400, "Invalid project file request");
    return { version: 1, operation: "list", scope, path: relative };
  }
  const known = q.get("knownRevision");
  if (known !== null && !REVISION.test(known)) throw new CloudProjectFileError(400, "Invalid known file revision");
  return { version: 1, operation: "read", scope, path: relative, ...(known === null ? {} : { knownRevision: known }) };
}
function saveFrom(body: unknown): CloudFileRequest {
  const value = body as Record<string, unknown> | null;
  const fields = ["version", "operation", "scope", "path", "baseRevision", "text"];
  if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some((k) => !fields.includes(k))
    || value.version !== 1 || value.operation !== "write" || typeof value.baseRevision !== "string" || !REVISION.test(value.baseRevision)
    || typeof value.text !== "string" || typeof value.path !== "string" || value.path === "") throw new CloudProjectFileError(400, "Invalid project file save");
  const raw = value.scope as Record<string, unknown> | null;
  if (!raw || typeof raw !== "object" || Object.keys(raw).some((k) => !["projectId", "workspaceId", "attemptId", "ownerEpoch"].includes(k))) throw new CloudProjectFileError(400, "Invalid project file identity");
  const scope = { projectId: raw.projectId, workspaceId: raw.workspaceId, attemptId: raw.attemptId, ownerEpoch: raw.ownerEpoch } as CloudFileScope;
  if (!validIdentity(scope, value.path)) throw new CloudProjectFileError(400, "Invalid project file identity or path");
  if (Buffer.byteLength(value.text) > WRITE_TEXT_BYTES) throw new CloudProjectFileError(413, SAVE_TOO_LARGE);
  return { version: 1, operation: "write", scope, path: value.path, baseRevision: value.baseRevision, text: value.text };
}

/** One instance per control-plane service bounds concurrent file RPCs. No
 * transport readiness or private IP is treated as application authorization.
 */
export class CloudProjectFiles<Actor> {
  #active = 0;
  constructor(private readonly dependencies: CloudFileDependencies<Actor>) {}
  /** GET: list a folder or read a file. */
  async query(actor: Actor, url: URL, signal?: AbortSignal): Promise<unknown> {
    return this.#dispatch(actor, requestFrom(url), signal);
  }
  /** POST: save a file the browser read at `baseRevision`. */
  async save(actor: Actor, body: unknown, signal?: AbortSignal): Promise<unknown> {
    return this.#dispatch(actor, saveFrom(body), signal);
  }
  async #dispatch(actor: Actor, request: CloudFileRequest, signal?: AbortSignal): Promise<unknown> {
    if (this.#active >= 8) throw new CloudProjectFileError(429, "Too many project files are open");
    const bounded = AbortSignal.any([AbortSignal.timeout(10_000), ...(signal ? [signal] : [])]);
    this.#active++;
    try {
      bounded.throwIfAborted();
      const grant = await abortable(this.dependencies.authorize(actor, request.scope, bounded), bounded);
      if (!sameScope(grant.scope, request.scope) || !grant.homeId) throw new CloudProjectFileError(403, "Project file access is no longer current");
      const result = await abortable(this.dependencies.rpc(grant.homeId, "projectFiles", request, bounded, actor), bounded);
      bounded.throwIfAborted();
      const fresh = await abortable(this.dependencies.authorize(actor, request.scope, bounded), bounded);
      bounded.throwIfAborted();
      if (!sameScope(fresh.scope, request.scope) || fresh.homeId !== grant.homeId) throw new CloudProjectFileError(403, "Project file access is no longer current");
      // The trusted worker has its own bounds; reject malformed remote replies
      // before passing any document to the browser or caching it elsewhere.
      if (!result || typeof result !== "object" || (result as { path?: unknown }).path !== request.path) throw new CloudProjectFileError(502, "Invalid project file reply");
      const value = result as Record<string, unknown>;
      if (request.operation === "write") {
        if (value.outcome === "conflict") return { path: request.path, outcome: "conflict" };
        if (value.outcome !== "saved" || typeof value.revision !== "string" || !REVISION.test(value.revision)) throw new CloudProjectFileError(502, "Invalid project file save reply");
        return { path: request.path, outcome: "saved", revision: value.revision };
      }
      if (request.operation === "list") {
        if (!Array.isArray(value.entries) || value.entries.length > 500 || typeof value.truncated !== "boolean") throw new CloudProjectFileError(502, "Invalid project folder reply");
        const seen = new Set<string>();
        const entries = value.entries.map((entry: unknown) => {
          const item = entry as Record<string, unknown> | undefined;
          if (!item || typeof item.name !== "string" || item.name.length > 255 || !item.name || /[\\/\x00-\x1f\x7f]/.test(item.name)
            || item.name === "." || item.name === ".." || item.path !== (request.path ? `${request.path}/` : "") + item.name
            || !["file", "folder"].includes(String(item.kind)) || seen.has(item.name)) throw new CloudProjectFileError(502, "Invalid project folder entry");
          seen.add(item.name);
          return { name: item.name, path: item.path, kind: item.kind };
        });
        return { path: request.path, entries, truncated: value.truncated };
      }
      if (typeof value.revision !== "string" || !REVISION.test(value.revision)
        || !["text", "markdown", "html", "image", "pdf", "binary"].includes(String(value.kind)) || typeof value.mime !== "string"
        || (value.unchanged !== undefined && (value.unchanged !== true || value.revision !== request.knownRevision || value.text !== undefined || value.base64 !== undefined))
        || (value.text !== undefined && (typeof value.text !== "string" || Buffer.byteLength(value.text) > 4 * 1024 * 1024))
        || (value.base64 !== undefined && (typeof value.base64 !== "string" || value.base64.length > Math.ceil(16 * 1024 * 1024 / 3) * 4))) throw new CloudProjectFileError(502, "Invalid project document reply");
      const media = value.kind === "pdf" || value.kind === "image";
      const unchanged = value.unchanged === true;
      if (media && (value.kind === "image" ? !MEDIA.has(String(value.mime)) : value.mime !== "application/pdf")
        || media && !unchanged && (typeof value.base64 !== "string" || value.base64.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(value.base64))
        || !media && value.kind !== "binary" && (!["text/plain", "text/css", "text/html", "text/markdown"].includes(String(value.mime)) || !unchanged && typeof value.text !== "string")) throw new CloudProjectFileError(502, "Invalid project document content");
      const document = { path: request.path, revision: value.revision, kind: value.kind, mime: value.mime };
      if (unchanged) return { ...document, unchanged: true };
      return { ...document, ...(media ? { base64: value.base64 } : value.kind !== "binary" ? { text: value.text } : {}) };
    } finally { this.#active--; }
  }
}
