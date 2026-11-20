import type { IncomingMessage } from "node:http";
import { CloudProjectFileError } from "./relay.js";

/** A file a chat message names, opened read only beside the chat
 * (cloud/app/file-link.js): in the agent's workspace, the project checkout or
 * the home user's own folders. Only the home's own account reaches it, as
 * with an agent's notes: a teammate never does, even in a project shared with
 * them. The home resolves the path and applies the deny list
 * (src/project-files/file-refs.ts); this checks who asks and what comes back. */
export const FILE_REF_PATH = "/api/app/file-ref";

type Dependencies = {
  /** The account's own connected home, or null. */
  ownHome(accountId: string): string | null;
  workspace(accountId: string, workspaceId: string): { homeId: string };
  rpc(homeId: string, method: string, params: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
};
type Target = { workspaceId: string; runId: string; path: string; knownRevision?: string };

const RUN_ID = /^[A-Za-z0-9._-]{1,256}$/;
const REVISION = /^[a-f0-9]{64}$/;
const CONTROL = /[\x00-\x1f\x7f]/;
const TEXT_BYTES = 2 * 1024 * 1024;
const MEDIA_BYTES = 16 * 1024 * 1024;
const TEXT_MIMES = new Set(["text/plain", "text/css", "text/html", "text/markdown"]);
const IMAGE_MIMES = new Set(["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif"]);
// Match through the actual end of the string, including when it ends in a newline.
const BASE64 = /^[A-Za-z0-9+/]+={0,2}(?!.)/s;

function targetFrom(url: URL): Target {
  const q = url.searchParams;
  const keys = [...q.keys()];
  const one = (key: string) => (q.getAll(key).length === 1 ? q.get(key)! : null);
  const workspaceId = one("workspaceId");
  const runId = one("runId");
  const path = one("path");
  const knownRevision = q.has("knownRevision") ? one("knownRevision") : undefined;
  if (keys.some((key) => !["workspaceId", "runId", "path", "knownRevision"].includes(key))
    || !workspaceId || workspaceId.length > 256 || !runId || !RUN_ID.test(runId)
    || !path || path.length > 4096 || CONTROL.test(path) || (knownRevision !== undefined && !(knownRevision && REVISION.test(knownRevision)))) {
    throw new CloudProjectFileError(400, "Invalid file request");
  }
  return { workspaceId, runId, path, ...(knownRevision ? { knownRevision } : {}) };
}

/** Only known fields with bounded values reach the browser. */
function documentFrom(value: unknown, target: Target) {
  const doc = value as Record<string, unknown> | null;
  const invalid = () => new CloudProjectFileError(502, "Invalid file reply");
  if (!doc || typeof doc !== "object" || Array.isArray(doc)) throw invalid();
  if (typeof doc.path !== "string" || !doc.path.startsWith("/") || doc.path.length > 4096 || CONTROL.test(doc.path)
    || typeof doc.revision !== "string" || !REVISION.test(doc.revision) || typeof doc.mime !== "string") throw invalid();
  const media = doc.kind === "image" || doc.kind === "pdf";
  const supported = doc.kind === "pdf" ? doc.mime === "application/pdf"
    : doc.kind === "image" ? IMAGE_MIMES.has(doc.mime)
    : (doc.kind === "text" || doc.kind === "markdown") && TEXT_MIMES.has(doc.mime);
  if (!supported) throw invalid();
  const base = { path: doc.path, revision: doc.revision, kind: doc.kind, mime: doc.mime };
  if (doc.unchanged !== undefined) {
    if (doc.unchanged !== true || doc.revision !== target.knownRevision || doc.text !== undefined || doc.base64 !== undefined) throw invalid();
    return { ...base, unchanged: true };
  }
  if (media) {
    const { base64 } = doc;
    if (doc.text !== undefined || typeof base64 !== "string" || base64.length > Math.ceil(MEDIA_BYTES / 3) * 4
      || base64.length % 4 !== 0 || !BASE64.test(base64) || Buffer.byteLength(base64, "base64") > MEDIA_BYTES) throw invalid();
    return { ...base, base64 };
  }
  if (doc.base64 !== undefined || typeof doc.text !== "string" || Buffer.byteLength(doc.text) > TEXT_BYTES) throw invalid();
  return { ...base, text: doc.text };
}

export class FileRefRoutes {
  constructor(private readonly dependencies: Dependencies) {}
  /** GET: read the file. Already behind account auth. */
  async handle(req: IncomingMessage, url: URL, accountId: string, signal: AbortSignal): Promise<unknown> {
    if (req.method !== "GET") throw new CloudProjectFileError(405, "Use GET to open a file");
    const target = targetFrom(url);
    const homeId = this.dependencies.ownHome(accountId);
    if (!homeId) throw new CloudProjectFileError(404, "Your cloud machine is not connected");
    let reply: unknown;
    try {
      if (this.dependencies.workspace(accountId, target.workspaceId).homeId !== homeId) {
        throw new CloudProjectFileError(403, "Files open only for agents on your own cloud machine");
      }
      const { workspaceId, ...request } = target;
      reply = await this.dependencies.rpc(homeId, "fileRef", { workspaceId, request: { version: 1, ...request } }, signal);
    } catch (error) {
      // The home's own message and status reach the browser ("File not
      // found: ...", a sleeping home), and an older home's Update now.
      const status = (error as { status?: unknown } | null)?.status;
      if (!(error instanceof CloudProjectFileError) && error instanceof Error && typeof status === "number") {
        throw Object.assign(new CloudProjectFileError(status, error.message), (error as { needsUpdate?: unknown }).needsUpdate === true ? { needsUpdate: true } : {});
      }
      throw error;
    }
    return documentFrom(reply, target);
  }
}
