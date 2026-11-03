import type { IncomingMessage, ServerResponse } from "node:http";
import { ProjectFileError, ProjectFileService, validateFilePath, validateFileScope } from "./service.js";
import type { ProjectFileRequest } from "./types.js";

export function parseProjectFileQuery(url: URL): ProjectFileRequest {
  const query = url.searchParams;
  const operation = query.get("operation");
  const epoch = query.get("ownerEpoch") ?? "";
  if (url.search.length > 8192 || query.get("version") !== "1" || (operation !== "list" && operation !== "read") || !/^(0|[1-9][0-9]*)$/.test(epoch)) {
    throw new ProjectFileError("invalid", "Invalid project file request");
  }
  const fields = ["version", "operation", "projectId", "workspaceId", "attemptId", "ownerEpoch", "path"];
  if ([...query.keys()].some((key) => !fields.includes(key)) || fields.some((key) => query.getAll(key).length !== 1)) {
    throw new ProjectFileError("invalid", "Invalid project file parameters");
  }
  return { version: 1, operation, scope: validateFileScope({ projectId: query.get("projectId"), workspaceId: query.get("workspaceId"),
    attemptId: query.get("attemptId"), ownerEpoch: Number(epoch) }), path: validateFilePath(query.get("path")) };
}

/** Mount INSIDE the board's authenticated project handler. The service authorize
 * callback must capture the current session and route's trusted project identity.
 * Serves JSON only: untrusted project bytes are never executable HTTP responses.
 */
export async function handleProjectFiles(req: IncomingMessage, res: ServerResponse, url: URL, service: ProjectFileService): Promise<void> {
  const controller = new AbortController();
  const cancel = (): void => { controller.abort(); };
  req.once("aborted", cancel);
  res.once("close", cancel);
  res.setHeader("Cache-Control", "no-store");
  res.setHeader("Content-Type", "application/json; charset=utf-8");
  res.setHeader("X-Content-Type-Options", "nosniff");
  res.setHeader("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'");
  try {
    if (req.method !== "GET") { res.statusCode = 405; res.setHeader("Allow", "GET"); res.end(JSON.stringify({ error: "Use GET for project files", code: "invalid" })); return; }
    const result = await service.dispatch(parseProjectFileQuery(url), controller.signal);
    if (!controller.signal.aborted) { res.statusCode = 200; res.end(JSON.stringify(result)); }
  } catch (error) {
    if (controller.signal.aborted || res.destroyed) return;
    const code = error instanceof ProjectFileError ? error.code : "denied";
    res.statusCode = ({ invalid: 400, denied: 403, missing: 404, changed: 409, large: 413, busy: 429, unsupported: 415 } as const)[code];
    res.end(JSON.stringify({ code, error: error instanceof ProjectFileError ? error.message : "Project file access is unavailable" }));
  } finally {
    req.off("aborted", cancel);
    res.off("close", cancel);
  }
}
