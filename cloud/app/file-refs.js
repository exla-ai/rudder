// File paths in chat messages become links that open the file beside the chat
// (file-link.js): a Markdown link to a local path or a file:// URL, and inline
// code that is exactly a path (`src/foo.ts:42`, `~/.claude/plans/plan.md`).
// Paths in plain prose stay text: telling them from prose has no reliable rule.
//
// Detection only, with no imports, so tests load this module in Node.

/** A file reference: `path` as written (absolute, `~/...` or relative to the
 * agent's workspace), the line and column to show, 0 when not given, and the
 * last line of a change to mark from `line` (a tool card's edit), 0 for none. */
export function fileRef(path, line = 0, col = 0, end = 0) {
  return { path, line, col, end };
}

const MAX_PATH = 4096;
const MAX_CODE = 1024;
const CONTROL = /[\u0000-\u001f\u007f]/;
// One segment of a path in inline code. Spaces, globs (`*`), `:` and quotes
// mean it is not a path.
const SEGMENT = /^[\p{L}\p{N}_.@+=,\-[\]()]+$/u;
// A file extension starts with a letter: `v1.2.3` and `0.5` are not files.
const EXTENSION = /\.([A-Za-z][A-Za-z0-9]{0,11})$/;
// A bare name in inline code (no `/`) is a file only with one of these
// extensions: `package.json` is a file, `console.log` and `mutex.lock` are code.
const BARE_EXTENSIONS = new Set(("md markdown mdx txt rst adoc ts tsx mts cts js jsx mjs cjs json jsonc jsonl ndjson "
  + "rs toml yaml yml py pyi swift kt kts java scala c h cc cpp cxx hpp hh cs rb php lua dart ex exs erl hs "
  + "sh bash zsh fish ps1 css scss sass less html htm xml svg vue svelte astro graphql gql proto tf hcl nix zig "
  + "ini plist pbxproj gradle csv tsv patch diff ipynb png jpg jpeg gif webp avif").split(" "));
// Receivers that make `x.json` or `x.html` a property, not a file (`res.json`).
const RECEIVERS = new Set(["res", "req", "resp", "response", "request", "console", "process", "window", "document", "this", "self", "JSON", "Math", "$", "jQuery"]);
// Files without an extension that are still files.
const NAMED = new Set(["Makefile", "Dockerfile", "Containerfile", "Procfile", "Gemfile", "Rakefile", "Justfile", "Brewfile",
  "Vagrantfile", "Caddyfile", "LICENSE", "README", "CHANGELOG", "CODEOWNERS", "NOTICE", "AUTHORS",
  "Cargo.lock", "Gemfile.lock", "yarn.lock", "poetry.lock", "flake.lock", "uv.lock", "go.mod", "go.sum"]);

/** `path:42`, `path:42:7` or `path:42-50` (a range opens at its first line). */
function splitLine(text) {
  const m = /:(\d{1,7})(?::(\d{1,5})|-\d{1,7})?$/.exec(text);
  if (!m || m.index === 0) return { path: text, line: 0, col: 0 };
  return { path: text.slice(0, m.index), line: Number(m[1]), col: m[2] ? Number(m[2]) : 0 };
}

/** GitHub's `#L42`, `#L42C7` and `#L42-L50`. */
function hashLine(hash) {
  const m = /^#L(\d{1,7})(?:C(\d{1,5}))?(?:-L\d{1,7}(?:C\d{1,5})?)?$/.exec(hash);
  return m ? { line: Number(m[1]), col: m[2] ? Number(m[2]) : 0 } : { line: 0, col: 0 };
}

function decode(text) {
  try { return decodeURIComponent(text); } catch { return null; }
}

/** The file a Markdown link points at, or null for a web address, an in-app
 * page or an anchor. Any local target counts: the link says it is one. */
export function fileRefOfHref(href) {
  const raw = String(href ?? "").trim();
  if (!raw || raw.length > MAX_PATH * 3 || raw.startsWith("#") || raw.startsWith("?") || raw.startsWith("//")) return null;
  if (/^file:/i.test(raw)) {
    let url;
    try { url = new URL(raw); } catch { return null; }
    if (url.host && url.hostname !== "localhost") return null;
    const path = decode(url.pathname);
    if (!path || path === "/" || path.endsWith("/") || CONTROL.test(path)) return null;
    const at = hashLine(url.hash);
    return fileRef(path, at.line, at.col);
  }
  // Any other scheme (https:, mailto:, ...) is not a file.
  if (/^[a-z][a-z\d+.-]*:/i.test(raw)) return null;
  // The app's own pages and API.
  if (/^\/(?:app|api)(?:[/?#]|$)/.test(raw)) return null;
  const hashAt = raw.indexOf("#");
  const hash = hashAt < 0 ? "" : raw.slice(hashAt);
  const bare = (hashAt < 0 ? raw : raw.slice(0, hashAt)).split("?")[0];
  const path = decode(bare);
  if (!path || path.length > MAX_PATH || CONTROL.test(path) || path.endsWith("/") || path === "~") return null;
  const fromHash = hashLine(hash);
  const split = splitLine(path);
  return fromHash.line ? fileRef(split.path, fromHash.line, fromHash.col) : fileRef(split.path, split.line, split.col);
}

/** The file inline code names, when the code is exactly a path. */
export function fileRefOfCode(text) {
  const raw = String(text ?? "").trim();
  if (!raw || raw.length > MAX_CODE || /\s/.test(raw)) return null;
  if (/^file:\/\//i.test(raw)) return fileRefOfHref(raw);
  const { path, line, col } = splitLine(raw);
  let rest = path;
  let rooted = false;
  if (rest.startsWith("~/")) { rest = rest.slice(2); rooted = true; }
  else if (rest.startsWith("/")) { rest = rest.slice(1); rooted = true; }
  const parts = rest.split("/");
  const name = parts[parts.length - 1];
  if (!name || parts.some((p) => !p || !SEGMENT.test(p))) return null;
  // `.` and `..` only lead a relative path, as in `../shared/types.ts`.
  const lead = rooted ? 0 : parts.findIndex((p) => p !== "." && p !== "..");
  if (lead < 0 || parts.slice(lead).some((p) => p === "." || p === "..")) return null;
  const ext = EXTENSION.exec(name);
  if (parts.length === 1 && !rooted) {
    // A bare name: `README.md`, `Dockerfile`. Not `.map`, `res.json` or `v2.1`.
    if (NAMED.has(name)) return fileRef(path, line, col);
    const stem = ext ? name.slice(0, ext.index) : "";
    if (!ext || !stem || !BARE_EXTENSIONS.has(ext[1].toLowerCase()) || RECEIVERS.has(stem)) return null;
    return fileRef(path, line, col);
  }
  // A path with folders: its last part has an extension (`/api/app/files` is
  // a route, `cloud/app/files-panel.js` a file) or is a known name.
  if (!ext && !NAMED.has(name)) return null;
  return fileRef(path, line, col);
}

/** Turn file paths in rendered chat Markdown under `root` into file links:
 * `<a class="file-ref">` without an href, so nothing navigates and the Mac
 * app never hands them to the browser; file-link.js opens them. A file:// link
 * that names no openable file loses its href. Marking again is a no-op. */
export function markFileRefs(root) {
  for (const a of root.querySelectorAll("a[href]")) {
    const href = a.getAttribute("href");
    const ref = fileRefOfHref(href);
    if (!ref) {
      if (/^file:/i.test(href.trim())) a.removeAttribute("href");
      continue;
    }
    for (const attr of ["href", "target", "rel"]) a.removeAttribute(attr);
    linkTo(a, ref);
  }
  for (const code of root.querySelectorAll("code")) {
    if (code.closest("pre, a")) continue;
    const ref = fileRefOfCode(code.textContent);
    if (!ref) continue;
    const a = code.ownerDocument.createElement("a");
    code.replaceWith(a);
    a.appendChild(code);
    linkTo(a, ref);
  }
}

/** Make `a` a file link to `ref`. */
export function linkTo(a, ref) {
  a.classList.add("file-ref");
  a.setAttribute("role", "link");
  a.tabIndex = 0;
  a.dataset.filePath = ref.path;
  if (ref.line) a.dataset.fileLine = String(ref.line);
  if (ref.col) a.dataset.fileCol = String(ref.col);
  if (ref.end) a.dataset.fileEnd = String(ref.end);
  a.title = `Open ${ref.path}${ref.line ? `:${ref.line}` : ""}${ref.end ? `-${ref.end}` : ""}`;
}

/** The reference a marked link holds. */
export function fileRefOfLink(a) {
  return fileRef(a.dataset.filePath, Number(a.dataset.fileLine) || 0, Number(a.dataset.fileCol) || 0, Number(a.dataset.fileEnd) || 0);
}
