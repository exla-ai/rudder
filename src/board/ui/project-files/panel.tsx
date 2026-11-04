import { useEffect, useMemo, useState } from "preact/hooks";
import { boardAccess } from "../access";
import { ProjectFileViewer, createProjectFileTransport, type ProjectFileReference } from "./index";
import type { FileScope } from "../../../project-files/types.js";

function scopeKey(scope: FileScope): string { return JSON.stringify([scope.projectId, scope.workspaceId, scope.attemptId, scope.ownerEpoch]); }
function validScope(value: unknown): value is FileScope {
  const scope = value as FileScope;
  return Boolean(scope && [scope.projectId, scope.workspaceId, scope.attemptId].every(item => typeof item === "string" && item.length > 0 && item.length <= 256 && !/[\x00-\x1f\x7f]/.test(item))
    && Number.isSafeInteger(scope.ownerEpoch) && scope.ownerEpoch >= 0);
}
function linkedReference(scopes: FileScope[]): ProjectFileReference | null {
  const query = new URLSearchParams(window.location.search);
  if (!query.has("file")) return null;
  const scope = { projectId: query.get("projectId")!, workspaceId: query.get("workspaceId")!, attemptId: query.get("attemptId")!, ownerEpoch: Number(query.get("ownerEpoch")) };
  if (!validScope(scope) || !scopes.some(item => scopeKey(item) === scopeKey(scope))) return null;
  return { scope, path: query.get("file")!, kind: query.get("fileKind") === "folder" ? "folder" : "file" };
}

export function ProjectFilesPanel({ slug }: { slug: string }) {
  const [scopes, setScopes] = useState<FileScope[]>([]);
  const [selected, select] = useState<ProjectFileReference | null>(null);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const endpoint = `/api/projects/${encodeURIComponent(slug)}`;
  const transport = useMemo(() => createProjectFileTransport(`${endpoint}/files`, boardAccess().fetch.bind(boardAccess())), [endpoint]);
  useEffect(() => {
    const controller = new AbortController();
    setScopes([]); select(null); setError("");
    void boardAccess().fetch(`${endpoint}/file-scopes`, { signal: controller.signal }).then(async response => {
      if (!response.ok) throw new Error("Project files require a current engine workspace.");
      const result = await response.json();
      if (!Array.isArray(result.scopes) || result.scopes.length > 16 || !result.scopes.every(validScope)) throw new Error("Project file bindings are unavailable.");
      if (!controller.signal.aborted) { setScopes(result.scopes); select(linkedReference(result.scopes)); }
    }).catch(cause => { if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : "Project files are unavailable."); });
    return () => controller.abort();
  }, [endpoint, refresh]);
  return <section style={{ padding: "12px 24px" }} aria-label="Workspace files">
    <details><summary>Project files</summary>
      <p>Open files from a current task workspace.</p>
      {error && <p role="status">{error}</p>}
      {!error && scopes.length === 0 && <p role="status">No current task workspace is available.</p>}
      {scopes.map(scope => <button type="button" class="btn" key={scopeKey(scope)} onClick={() => select({ scope, path: "", kind: "folder" })}>Files · {scope.workspaceId}</button>)}
      <button type="button" class="btn" onClick={() => setRefresh(refresh + 1)}>Refresh workspaces</button>
    </details>
    {selected && <ProjectFileViewer reference={selected} transport={transport} onClose={() => select(null)} />}
  </section>;
}
