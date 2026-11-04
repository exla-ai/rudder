// Version-1 structural wire shapes shared by both browser renderers. These
// contain no roots, credentials or authority; servers authorize every read.
import type { CloudFileScope as FileScope } from "./relay.js";
export type FileDocument = {
  path:string;revision:string;mime:string;
  kind:"text"|"markdown"|"html"|"image"|"pdf"|"binary";
  text?:string;base64?:string;
};
type FileListing = {path:string;entries:{path:string;name:string;kind:"file"|"folder";size?:number}[];truncated:boolean};

export interface ProjectFileTransport {
  list(scope: FileScope, path: string, signal: AbortSignal): Promise<FileListing>;
  read(scope: FileScope, path: string, signal: AbortSignal): Promise<FileDocument>;
}

export const VIEWER_CSP = "default-src 'none'; script-src 'none'; connect-src 'none'; img-src data:; style-src 'unsafe-inline'; font-src data:; media-src 'none'; object-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'";
const IMAGE_MIMES = new Set(["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif"]);

/** Relative paths only. URL schemes, encoded separators and root escapes fail closed. */
export function nearbyPath(from: string, input: string): string | null {
  if (!input || input.length > 2048 || /[\\\u0000-\u001f\u007f]/.test(input) || /^[a-z][a-z\d+.-]*:/i.test(input) || input.startsWith("/")) return null;
  const raw = input.split(/[?#]/, 1)[0];
  let decoded: string;
  try { decoded = decodeURIComponent(raw); } catch { return null; }
  if (decoded !== raw && /[\\/?#\u0000-\u001f\u007f]/.test(decoded)) return null;
  const parts = from.split("/").slice(0, -1);
  for (const part of decoded.split("/")) {
    if (!part || part === ".") continue;
    if (part === "..") { if (!parts.length) return null; parts.pop(); }
    else { if (part.includes(":")) return null; parts.push(part); }
  }
  return parts.join("/");
}

export function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

/** Small, deliberately inert Markdown subset. Raw HTML is always text. */
export function markdownHtml(text: string): string {
  let code = false;
  return text.split(/\r?\n/).map((line) => {
    if (/^```/.test(line)) { code = !code; return code ? "<pre><code>" : "</code></pre>"; }
    if (code) return `${escapeHtml(line)}\n`;
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    const inline = (s: string) => escapeHtml(s).replace(/!\[([^\]]*)\]\(([^\s)]+)\)/g, '<img alt="$1" src="$2">').replace(/\[([^\]]+)\]\(([^\s)]+)\)/g, "$1").replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    if (heading) return `<h${heading[1].length}>${inline(heading[2])}</h${heading[1].length}>`;
    if (/^[-*] /.test(line)) return `<p>• ${inline(line.slice(2))}</p>`;
    return line ? `<p>${inline(line)}</p>` : "";
  }).join("\n") + (code ? "</code></pre>" : "");
}

export function imageData(document: FileDocument): string | null {
  if (!IMAGE_MIMES.has(document.mime) || !document.base64 || !/^[A-Za-z0-9+/]*={0,2}$/.test(document.base64)) return null;
  return `data:${document.mime};base64,${document.base64}`;
}

/** PDF data documents have opaque origins instead of inheriting the control origin. */
export function pdfData(document: FileDocument): string | null {
  const bytes = document.base64;
  if (document.kind !== "pdf" || document.mime !== "application/pdf" || !bytes || bytes.length < 8
    || bytes.length > Math.ceil(16 * 1024 * 1024 / 3) * 4
    || bytes.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(bytes)) return null;
  try {
    if (!atob(bytes.slice(0, 8)).startsWith("%PDF-") || btoa(atob(bytes.slice(-4))) !== bytes.slice(-4)) return null;
  } catch { return null; }
  return `data:application/pdf;base64,${bytes}`;
}

/** Build only an inert srcdoc. Asset bytes are fetched through the same authorized scope. */
export async function prepareHtml(document: FileDocument, scope: FileScope, transport: ProjectFileTransport, signal: AbortSignal): Promise<string> {
  const source = document.kind === "markdown" ? markdownHtml(document.text ?? "") : document.text ?? "";
  const parsed = new DOMParser().parseFromString("<!doctype html><html><head></head><body></body></html>", "text/html");
  // Template content is inert while we rewrite assets. Parsing untrusted HTML into a
  // regular detached document can initiate image requests before sanitization.
  const template = parsed.createElement("template");
  template.innerHTML = source;
  const content = template.content;
  content.querySelectorAll("script,iframe,frame,frameset,object,embed,form,base,meta,svg,math,template,video,audio,source,track").forEach((node) => node.remove());
  let remaining = 16;
  let bytes = 0;
  const asset = async (path: string): Promise<FileDocument | null> => {
    if (remaining-- <= 0 || signal.aborted) return null;
    const resolved = nearbyPath(document.path, path);
    if (resolved === null) return null;
    try {
      const result = await transport.read(scope, resolved, signal);
      bytes += (result.base64?.length ?? result.text?.length ?? 0);
      return bytes <= 12 * 1024 * 1024 ? result : null;
    } catch (error) { if (signal.aborted) throw error; return null; }
  };
  // No event handlers, navigation, external requests or nested browsing contexts survive.
  for (const element of Array.from(content.querySelectorAll("*"))) {
    for (const attribute of Array.from(element.attributes)) {
      if (/^on/i.test(attribute.name) || ["srcdoc", "href", "action", "formaction", "srcset", "poster", "background", "ping", "target", "download", "nonce", "integrity", "is"].includes(attribute.name)) {
        // Stylesheets are handled below using their original URL.
        if (element.tagName === "LINK" && attribute.name === "href") continue;
        element.removeAttribute(attribute.name);
      }
    }
    if (element.tagName === "IMG") {
      const input = element.getAttribute("src") ?? "";
      element.removeAttribute("src");
      const result = await asset(input);
      const data = result && imageData(result);
      if (data) element.setAttribute("src", data);
    } else {
      element.removeAttribute("src");
    }
    if (element.tagName === "LINK") {
      const input = element.getAttribute("href") ?? "";
      if (element.getAttribute("rel")?.toLowerCase() === "stylesheet") {
        const result = await asset(input);
        if (result?.text && ["text/css", "text/plain"].includes(result.mime)) {
          const style = parsed.createElement("style");
          style.textContent = result.text;
          element.replaceWith(style);
        }
      }
      element.remove();
    }
  }
  signal.throwIfAborted();
  return `<!doctype html><html><head><meta http-equiv="Content-Security-Policy" content="${VIEWER_CSP}"><meta name="referrer" content="no-referrer"><style>body{font:16px/1.6 system-ui,sans-serif;padding:20px;overflow-wrap:anywhere}img{max-width:100%}pre{white-space:pre-wrap}code{font-family:monospace}</style></head><body>${template.innerHTML}</body></html>`;
}
