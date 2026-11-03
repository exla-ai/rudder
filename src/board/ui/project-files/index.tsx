import { useEffect, useState } from "preact/hooks";
import type { FileDocument, FileScope } from "../../../project-files/types.js";
import { imageData, pdfData, prepareHtml, type ProjectFileTransport } from "./render.js";
import { refreshFiles, type FileSnapshot } from "./refresh.js";

export type { ProjectFileTransport } from "./render.js";
export { createProjectFileTransport } from "./transport.js";
export interface ProjectFileReference { scope: FileScope; path: string; kind?: "file" | "folder" }
export interface ProjectFileViewerProps {
  reference: ProjectFileReference;
  transport: ProjectFileTransport;
  onClose?: () => void;
}

export function OpenProjectFile({ reference, onOpen, children }: {
  reference: ProjectFileReference; onOpen(reference: ProjectFileReference): void; children?: string;
}) {
  return <button type="button" class="btn" title={`Open ${reference.path || "project files"}`} onClick={() => onOpen(reference)}>{children ?? (reference.path || "Project files")}</button>;
}

function parentPath(path: string): string { return path.split("/").slice(0, -1).join("/"); }
function scopeKey(scope: FileScope): string { return JSON.stringify([scope.projectId, scope.workspaceId, scope.attemptId, scope.ownerEpoch]); }

export function ProjectFileViewer({ reference, transport, onClose }: ProjectFileViewerProps) {
  const identity = `${scopeKey(reference.scope)}:${reference.kind}:${reference.path}`;
  // Keying the inner component prevents any old scope's bytes being painted after a project switch.
  return <FileBrowser key={identity} reference={reference} transport={transport} onClose={onClose} />;
}

function FileBrowser({ reference, transport, onClose }: ProjectFileViewerProps) {
  const folder = reference.kind === "folder" || !reference.path;
  const [directory, setDirectory] = useState(folder ? reference.path : parentPath(reference.path));
  const [path, setPath] = useState<string | null>(folder ? null : reference.path);
  const [snapshot, setSnapshot] = useState<FileSnapshot | null>(null);
  const [error, setError] = useState("");
  const [source, setSource] = useState(false);
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    setSnapshot(null); setError("");
    return refreshFiles({ transport, scope: reference.scope, directory, path,
      onSnapshot: (next) => { setSnapshot(next); setError(""); },
      onError: (cause) => { setSnapshot(null); setError(cause instanceof Error ? cause.message : "Unable to open project files"); },
    });
  }, [transport, directory, path, refresh]);
  const open = (nextPath: string, kind: "file" | "folder") => {
    setSnapshot(null); setError(""); setSource(false);
    setDirectory(kind === "folder" ? nextPath : parentPath(nextPath));
    setPath(kind === "folder" ? null : nextPath);
  };
  const document = snapshot?.document;
  return <section class="project-file-viewer" aria-label="Project files" style={{ border: "1px solid var(--line, #ddd)", borderRadius: 8, overflow: "hidden", background: "var(--paper, white)" }}>
    <header style={{ display: "flex", alignItems: "center", gap: 8, padding: 12, borderBottom: "1px solid var(--line, #ddd)", flexWrap: "wrap" }}>
      <strong>Project files</strong>
      <span class="mono" style={{ flex: 1, overflowWrap: "anywhere" }}>{path ?? (directory || "/")}</span>
      <button type="button" class="btn" onClick={() => setRefresh(refresh + 1)}>Refresh</button>
      {document && ["text", "markdown", "html"].includes(document.kind) && <div role="group" aria-label="File view">
        <button type="button" class="btn" aria-pressed={!source} onClick={() => setSource(false)}>Rendered</button>
        <button type="button" class="btn" aria-pressed={source} onClick={() => setSource(true)}>Source</button>
      </div>}
      {onClose && <button type="button" class="btn" onClick={onClose}>Close</button>}
    </header>
    <div style={{ display: "flex", flexWrap: "wrap", minHeight: 400 }}>
      <nav aria-label="Nearby files" style={{ width: 240, flex: "0 1 240px", borderRight: "1px solid var(--line, #ddd)", padding: 12, maxHeight: 640, overflow: "auto" }}>
        <button type="button" class="btn" disabled={!directory} onClick={() => open(parentPath(directory), "folder")}>Parent folder</button>
        <button type="button" class="btn" onClick={() => open("", "folder")}>Project root</button>
        <ul style={{ listStyle: "none", padding: 0 }}>
          {snapshot?.listing.entries.map((entry) => <li key={entry.path}><button type="button" aria-current={entry.path === path ? "page" : undefined} onClick={() => open(entry.path, entry.kind)} style={{ border: 0, background: entry.path === path ? "var(--paper-2, #eee)" : "transparent", color: "inherit", textAlign: "left", padding: "7px 2px", width: "100%", overflowWrap: "anywhere", cursor: "pointer" }}>{entry.kind === "folder" ? "▸ " : ""}{entry.name}{entry.kind === "folder" ? "/" : ""}</button></li>)}
        </ul>
        {snapshot?.listing.truncated && <p role="status">Folder limit reached. Open a subfolder to browse more files.</p>}
      </nav>
      <main style={{ flex: "1 1 280px", minWidth: 0, padding: 16 }}>
        {error ? <p role="alert">{error}</p> : !snapshot ? <p role="status">Loading files…</p> : document ? <FilePreview document={document} source={source} scope={reference.scope} transport={transport} /> : <p>Choose a file to preview. Saved changes appear automatically.</p>}
      </main>
    </div>
  </section>;
}

export function FilePreview({ document, source, scope, transport }: {
  document: FileDocument; source: boolean; scope: FileScope; transport: ProjectFileTransport;
}) {
  const [html, setHtml] = useState<{ revision: string; source: string } | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    setError("");
    if (source || !["markdown", "html"].includes(document.kind)) return;
    let stopped = false;
    let controller: AbortController;
    let next: ReturnType<typeof setTimeout>;
    let timer: ReturnType<typeof setTimeout>;
    const render = async () => {
      controller = new AbortController();
      timer = setTimeout(() => controller.abort(), 10_000);
      try {
        const result = await prepareHtml(document, scope, transport, controller.signal);
        if (!stopped && !controller.signal.aborted) { setHtml({ revision: document.revision, source: result }); setError(""); }
      } catch {
        if (!stopped) setError("Unable to render this document. Open Source to read it.");
      } finally {
        clearTimeout(timer);
        // Refresh dependent assets too, including when the HTML itself is unchanged.
        if (!stopped && !controller.signal.aborted) next = setTimeout(render, 2000);
      }
    };
    void render();
    return () => { stopped = true; clearTimeout(timer); clearTimeout(next); controller?.abort(); };
  }, [document.path, document.revision, document.kind, source, transport, scopeKey(scope)]);
  if (source || document.kind === "text") return <pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere", font: "13px/1.6 var(--mono, monospace)" }}>{document.text ?? "Binary file. Source is unavailable."}</pre>;
  if (document.kind === "markdown" || document.kind === "html") return error ? <p role="alert">{error}</p> : html?.revision === document.revision ? <DocumentFrame title={`Preview of ${document.path}`} source={html.source} /> : <p role="status">Rendering preview…</p>;
  if (document.kind === "image") {
    const data = imageData(document);
    return data ? <img src={data} alt={document.path} style={{ maxWidth: "100%", maxHeight: "70vh", objectFit: "contain" }} /> : <p>Unsupported image format. Open a supported image or view its source.</p>;
  }
  if (document.kind === "pdf") return <NativePdfFrame document={document} />;
  return <p>This binary format has no preview.</p>;
}

export function DocumentFrame({ title, source }: { title: string; source: string }) {
  return <iframe title={title} sandbox="" referrerPolicy="no-referrer" srcDoc={source} style={{ border: 0, width: "100%", height: "65vh", background: "white" }} />;
}

export function NativePdfFrame({ document }: { document: FileDocument }) {
  const url = pdfData(document);
  // An iframe sandbox disables native PDF viewers. The signature-checked PDF
  // instead loads as an opaque data document, never an inherited-origin Blob.
  return url ? <iframe title={`PDF: ${document.path}`} referrerPolicy="no-referrer" src={url} style={{ border: 0, width: "100%", height: "65vh" }} /> : <p role="alert">This file is not a supported PDF.</p>;
}
