import type { FileDocument, FileListing, FileScope, ProjectFileTransport } from "../../../project-files/types.js";

const MAX_RESPONSE = 24 * 1024 * 1024;
function validPath(path: unknown): path is string {
  return typeof path === "string" && path.length <= 4096 && !/[\\\u0000-\u001f\u007f]/.test(path)
    && !path.startsWith("/") && !path.split("/").some((part) => part === "." || part === ".." || part === "" && path !== "");
}

async function boundedJson(response: Response, signal: AbortSignal): Promise<any> {
  if (Number(response.headers.get("content-length")) > MAX_RESPONSE) throw new Error("File response is too large");
  if (!response.body) throw new Error("Empty file response");
  const reader = response.body.getReader();
  const cancel = () => { void reader.cancel().catch(() => {}); };
  signal.addEventListener("abort", cancel, { once: true });
  let length = 0;
  let text = "";
  const decoder = new TextDecoder("utf-8", { fatal: true });
  try {
    signal.throwIfAborted();
    for (;;) {
      const { value, done } = await reader.read();
      signal.throwIfAborted();
      if (done) break;
      length += value.length;
      if (length > MAX_RESPONSE) throw new Error("File response is too large");
      text += decoder.decode(value, { stream: true });
    }
    text += decoder.decode();
    return JSON.parse(text);
  } finally {
    signal.removeEventListener("abort", cancel);
    void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

async function abortable<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  signal.throwIfAborted();
  let cancel!: () => void;
  const cancelled = new Promise<never>((_, reject) => {
    cancel = () => reject(signal.reason ?? new Error("File request cancelled"));
    signal.addEventListener("abort", cancel, { once: true });
  });
  try { return await Promise.race([work, cancelled]); }
  finally { signal.removeEventListener("abort", cancel); }
}

/** Inject boardAccess().fetch.bind(boardAccess()) locally, or the private Cloud fetch. */
export function createProjectFileTransport(endpoint: string, authorizedFetch: (input: string, init?: RequestInit) => Promise<Response>): ProjectFileTransport {
  if (!endpoint.startsWith("/") || endpoint.startsWith("//") || /[?#\\\u0000-\u001f]/.test(endpoint)) throw new Error("File endpoint must be a private application route");
  async function request(scope: FileScope, path: string, operation: "list" | "read", signal: AbortSignal) {
    signal.throwIfAborted();
    if (!validPath(path) || !Number.isSafeInteger(scope.ownerEpoch) || scope.ownerEpoch < 0 || [scope.projectId, scope.workspaceId, scope.attemptId].some((id) => typeof id !== "string" || !id || id.length > 256 || /[\u0000-\u001f\u007f]/.test(id))) throw new Error("Invalid project file identity");
    const bounded = AbortSignal.any([signal, AbortSignal.timeout(10_000)]);
    const query = new URLSearchParams({ version: "1", operation, projectId: scope.projectId, workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: String(scope.ownerEpoch), path });
    const response = await abortable(authorizedFetch(`${endpoint}?${query}`, { signal: bounded, credentials: "omit", redirect: "error", cache: "no-store", referrerPolicy: "no-referrer" }), bounded);
    const result = await abortable(boundedJson(response, bounded), bounded);
    if (!response.ok) throw new Error(typeof result?.error === "string" ? result.error.slice(0, 500) : `Unable to open file (${response.status})`);
    if (result?.path !== path) throw new Error("File response identity changed");
    if (operation === "list") {
      if (!Array.isArray(result.entries) || result.entries.length > 500 || typeof result.truncated !== "boolean") throw new Error("Invalid folder response");
      const seen = new Set();
      for (const entry of result.entries) {
        if (!validPath(entry.path) || !entry.path || typeof entry.name !== "string" || !entry.name || entry.path !== (path ? `${path}/` : "") + entry.name || entry.name.includes("/") || !["file", "folder"].includes(entry.kind) || seen.has(entry.path)) throw new Error("Invalid folder entry");
        seen.add(entry.path);
      }
      return result;
    }
    if (typeof result.revision !== "string" || !/^[a-f0-9]{64}$/.test(result.revision) || typeof result.mime !== "string" || result.mime.length > 100 || !["text", "markdown", "html", "image", "pdf", "binary"].includes(result.kind)
      || result.text !== undefined && (typeof result.text !== "string" || result.text.length > 4 * 1024 * 1024)
      || result.base64 !== undefined && (typeof result.base64 !== "string" || result.base64.length > 23 * 1024 * 1024 || !/^[A-Za-z0-9+/]*={0,2}$/.test(result.base64))) throw new Error("Invalid file response");
    return result;
  }
  return {
    list: (scope, path, signal) => request(scope, path, "list", signal) as Promise<FileListing>,
    read: (scope, path, signal) => request(scope, path, "read", signal) as Promise<FileDocument>,
  };
}
