// The Files panel: a project's tree and one open file beside the conversation.
// On desktop it fills the shell's right column (`.dsk-files`); narrower, it is
// a full-height sheet over the screen, which stays mounted underneath.
//
// Every read and save goes through the home's file service with an observed
// workspace scope: the project checkout or a live agent's own workspace (its
// fork mount on Linux). The browser never names a host path. Saves carry the
// revision the text was read at, and the home refuses them when the file has
// changed since, so an agent's write is never silently replaced.
import { api, confirmDialog } from "/app/core.js";
import { h, icon, loadMarkdown, renderMarkdown } from "/app/render.js";
import { prepareHtml, imageData, pdfData, nearbyPath } from "/app/project-file-render.js";
import { createEditor } from "/app/code-editor.js";

const CHECKOUT = "checkout";
const FILE_POLL_MS = 2000;
const TREE_POLL_MS = 6000;
// Scope bindings expire after 60s on the control plane; renew well before.
const SCOPE_RENEW_MS = 40000;
const EDIT_BYTES = 1024 * 1024;
const WIDTH_KEY = "rudder.filesWidth";

const scopeKey = (s) => JSON.stringify([s.projectId, s.workspaceId, s.attemptId, s.ownerEpoch]);
const utf8Bytes = (text) => new TextEncoder().encode(text).length;
const parentOf = (path) => path.split("/").slice(0, -1).join("/");
const nameOf = (path) => path.split("/").pop() || "";

function button(className, label, html, run) {
  const b = h(`<button type="button" class="${className}">${html}</button>`);
  b.setAttribute("aria-label", label);
  b.title = label;
  b.addEventListener("click", run);
  return b;
}

// ---------- One panel per page ----------

const state = {
  open: false,
  target: null,       // { ws, run, findAgent }
  scopes: [],
  scopesAt: 0,
  scope: null,
  notice: "",
  folders: new Map(),  // path -> { entries, truncated, error }
  expanded: new Set([""]),
  file: null,          // the open file, see openFile()
  treeTimer: 0,
  fileTimer: 0,
  generation: 0,
};

const el = h(`<section class="fp" aria-label="Files">
  <div class="fp-grip" aria-hidden="true"></div>
  <header class="fp-head">
    <span class="fp-title">${icon("folder")}<span>Files</span></span>
    <select class="fp-ws" aria-label="Workspace"></select>
    <button type="button" class="icon-btn fp-refresh" aria-label="Refresh" title="Refresh">${icon("refresh")}</button>
    <button type="button" class="icon-btn fp-close" aria-label="Close files" title="Close files">${icon("x")}</button>
  </header>
  <div class="fp-notice" hidden></div>
  <div class="fp-body">
    <nav class="fp-tree" aria-label="Project tree"></nav>
    <div class="fp-file" hidden>
      <div class="fp-file-head">
        <button type="button" class="icon-btn fp-back" aria-label="Back to files" title="Back to files">${icon("back")}</button>
        <span class="fp-path"></span>
        <span class="fp-modes" role="group" aria-label="View"></span>
        <span class="fp-status" role="status"></span>
        <button type="button" class="btn primary fp-save" disabled>Save</button>
      </div>
      <div class="fp-banner" hidden role="alert"></div>
      <div class="fp-content"></div>
    </div>
  </div>
</section>`);
const $ = (s) => el.querySelector(s);
const wsSelect = $(".fp-ws"), treeEl = $(".fp-tree"), fileEl = $(".fp-file"), noticeEl = $(".fp-notice");
const pathEl = $(".fp-path"), modesEl = $(".fp-modes"), statusEl = $(".fp-status"), saveBtn = $(".fp-save");
const bannerEl = $(".fp-banner"), contentEl = $(".fp-content");

$(".fp-close").addEventListener("click", () => { void closePanel(); });
$(".fp-refresh").addEventListener("click", () => { state.scopesAt = 0; void refreshTree(); void pollFile(true); });
$(".fp-back").addEventListener("click", () => el.classList.remove("show-file"));
saveBtn.addEventListener("click", () => { void save(); });
wsSelect.addEventListener("change", async () => {
  const next = state.scopes.find((s) => scopeKey(s) === wsSelect.value);
  if (!next || !(await discardOk())) { paintWorkspaces(); return; }
  selectScope(next);
});
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible" && state.open) { schedule(); } });
window.addEventListener("beforeunload", (e) => { if (dirty()) e.preventDefault(); });

// ---------- Placement ----------

function column() { return document.querySelector(".dsk-files"); }

function place() {
  const col = column();
  if (col) {
    el.classList.remove("fp-sheet");
    if (el.parentElement !== col) col.replaceChildren(el);
    col.hidden = false;
    col.closest(".dsk")?.classList.add("with-files");
    const width = Number(localStorage.getItem(WIDTH_KEY));
    if (width) col.closest(".dsk")?.style.setProperty("--files-w", `${width}px`);
  } else {
    el.classList.add("fp-sheet");
    if (el.parentElement !== document.body) document.body.appendChild(el);
  }
}

function unplace() {
  const col = column();
  if (col) { col.hidden = true; col.closest(".dsk")?.classList.remove("with-files"); }
  el.remove();
}

// Drag the panel's left edge to size the desktop column.
$(".fp-grip").addEventListener("pointerdown", (e) => {
  const dsk = el.closest(".dsk");
  if (!dsk) return;
  e.preventDefault();
  const grip = e.currentTarget;
  grip.setPointerCapture(e.pointerId);
  dsk.classList.add("resizing");
  const move = (ev) => {
    const width = Math.round(Math.min(Math.max(window.innerWidth - ev.clientX, 360), window.innerWidth - 420));
    dsk.style.setProperty("--files-w", `${width}px`);
  };
  const up = () => {
    dsk.classList.remove("resizing");
    grip.removeEventListener("pointermove", move);
    const width = parseInt(getComputedStyle(dsk).getPropertyValue("--files-w"), 10);
    try { if (width) localStorage.setItem(WIDTH_KEY, String(width)); } catch { /* private mode keeps the default */ }
  };
  grip.addEventListener("pointermove", move);
  grip.addEventListener("pointerup", up, { once: true });
  grip.addEventListener("pointercancel", up, { once: true });
});

// ---------- Public surface ----------

/** The topbar Files button of a project or agent screen (`run` null for the
 * project). `findAgent(ws, run)` names agents in the workspace picker. */
export function filesButton({ ws, run = null, findAgent }) {
  return button("icon-btn fp-toggle", "Files", icon("folder"), () => { void togglePanel({ ws, run, findAgent }); });
}

/** The page's route changed, or the other of two panes took focus: an open
 * panel follows that project and agent. Unsaved edits keep it where it is. */
export function followFiles(route, findAgent) {
  if (!state.open) return;
  place(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  if (dirty()) return;
  if (!route?.ws || !["project", "agent"].includes(route.name)) { void closePanel(); return; }
  const target = { ws: route.ws, run: route.name === "agent" ? route.run : null, findAgent };
  if (!sameTarget(state.target, target)) void retarget(target);
}

const sameTarget = (a, b) => !!a && !!b && a.ws === b.ws && a.run === b.run;
const agentName = (target, id) => target?.findAgent(target.ws, id)?.title || null;

async function togglePanel(target) {
  if (state.open && sameTarget(state.target, target)) { await closePanel(); return; }
  state.open = true;
  place();
  if (!sameTarget(state.target, target)) {
    if (!(await discardOk())) return;
    await retarget(target);
  } else schedule();
}

async function closePanel() {
  if (!(await discardOk())) return;
  state.open = false;
  stopTimers();
  closeFile();
  unplace();
}

async function retarget(target) {
  state.target = target;
  state.scope = null;
  state.scopes = [];
  state.scopesAt = 0;
  closeFile();
  resetTree();
  treeEl.replaceChildren(h(`<div class="fp-empty">Loading files</div>`));
  try { await renewScopes(); }
  catch (e) { treeEl.replaceChildren(errorRow(e)); return; }
  const want = target.run ? state.scopes.find((s) => s.workspaceId === target.run) : null;
  const checkout = state.scopes.find((s) => s.workspaceId === CHECKOUT);
  const notice = target.run && !want ? `${agentName(target, target.run) || "This agent"} is not running. Showing the main checkout.` : "";
  const first = want || checkout || state.scopes[0];
  if (!first) { paintWorkspaces(); treeEl.replaceChildren(h(`<div class="fp-empty">No file workspaces are open. Start the project's engine on its home machine.</div>`)); return; }
  selectScope(first, notice);
}

// ---------- Scopes ----------

async function renewScopes() {
  const ws = state.target?.ws;
  if (!ws || Date.now() - state.scopesAt < SCOPE_RENEW_MS) return;
  const response = await api(`/api/app/project-file-scopes?${new URLSearchParams({ workspaceId: ws })}`);
  if (state.target?.ws !== ws) return;
  if (!Array.isArray(response?.scopes)) throw new Error("Invalid file workspaces");
  state.scopes = response.scopes;
  state.scopesAt = Date.now();
  if (state.scope) {
    const kept = state.scopes.find((s) => scopeKey(s) === scopeKey(state.scope));
    if (!kept) {
      state.scope = null;
      state.notice = "That workspace closed. Choose another one.";
      if (state.file) state.file.closed = true;
      paintFileControls();
    }
  }
  paintWorkspaces();
}

function scopeLabel(s) {
  if (s.workspaceId === CHECKOUT) return "Main checkout";
  return agentName(state.target, s.workspaceId) || s.workspaceId;
}

function paintWorkspaces() {
  wsSelect.replaceChildren(...state.scopes.map((s) => {
    const o = document.createElement("option");
    o.value = scopeKey(s); o.textContent = scopeLabel(s);
    return o;
  }));
  if (state.scope) wsSelect.value = scopeKey(state.scope);
  else wsSelect.selectedIndex = -1;
  wsSelect.disabled = state.scopes.length < 2;
  noticeEl.textContent = state.notice;
  noticeEl.hidden = !state.notice;
}

function selectScope(scope, notice = "") {
  state.scope = scope;
  state.notice = notice;
  closeFile();
  resetTree();
  paintTree();
  paintWorkspaces();
  void refreshTree();
  schedule();
}

// ---------- Requests ----------

async function request(params) {
  await renewScopes();
  const scope = state.scope;
  if (!scope) throw new Error("Choose a workspace");
  const value = await api(`/api/app/project-files?${new URLSearchParams({ version: "1", ...params, projectId: scope.projectId, workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: String(scope.ownerEpoch) })}`);
  return { scope, value };
}
const list = (path) => request({ operation: "list", path });
const read = (path, knownRevision) => request({ operation: "read", path, ...(knownRevision ? { knownRevision } : {}) });

// ---------- Tree ----------

function resetTree() {
  state.folders = new Map();
  state.expanded = new Set([""]);
  treeEl.replaceChildren();
}

async function loadFolder(path) {
  const scope = state.scope;
  try {
    const { scope: used, value } = await list(path);
    if (used !== state.scope) return;
    state.folders.set(path, { entries: value.entries, truncated: value.truncated });
  } catch (e) {
    if (scope !== state.scope) return;
    if (e?.status === 403) state.scopesAt = 0;
    state.folders.set(path, { entries: [], truncated: false, error: e?.message || "Unable to open this folder" });
  }
}

async function refreshTree() {
  const generation = ++state.generation;
  for (const path of [...state.expanded]) {
    await loadFolder(path);
    if (generation !== state.generation) return;
  }
  paintTree();
}

function errorRow(e) {
  const row = h(`<div class="fp-empty fp-error"></div>`);
  row.textContent = e?.message || "Unable to open project files";
  return row;
}

let painted = "";
function paintTree() {
  const key = JSON.stringify([[...state.folders], [...state.expanded], state.file?.path ?? null]);
  if (key === painted && treeEl.childElementCount) return;
  painted = key;
  const rows = [];
  const walk = (path, depth) => {
    const folder = state.folders.get(path);
    if (!folder) { if (state.expanded.has(path)) rows.push(note("Loading", depth)); return; }
    if (folder.error) { rows.push(note(folder.error, depth, true)); return; }
    if (!folder.entries.length && depth === 0) rows.push(note("This folder is empty", depth));
    for (const entry of folder.entries) {
      const open = entry.kind === "folder" && state.expanded.has(entry.path);
      const row = h(`<button type="button" class="fp-row"><span class="fp-chev">${entry.kind === "folder" ? icon("chevron") : ""}</span>${icon(entry.kind === "folder" ? "folder" : "file")}<span class="fp-name"></span></button>`);
      row.querySelector(".fp-name").textContent = entry.name;
      row.style.setProperty("--depth", depth);
      row.dataset.path = entry.path;
      row.dataset.kind = entry.kind;
      row.classList.toggle("open", open);
      row.classList.toggle("on", state.file?.path === entry.path);
      rows.push(row);
      if (open) walk(entry.path, depth + 1);
    }
    if (folder.truncated) rows.push(note("More files are in this folder than the tree shows", depth));
  };
  walk("", 0);
  treeEl.replaceChildren(...rows);
}

function note(text, depth, error = false) {
  const row = h(`<div class="fp-note${error ? " fp-error" : ""}"></div>`);
  row.style.setProperty("--depth", depth);
  row.textContent = text;
  return row;
}

treeEl.addEventListener("click", async (e) => {
  const row = e.target.closest(".fp-row");
  if (!row) return;
  const { path, kind } = row.dataset;
  if (kind === "folder") {
    if (state.expanded.has(path)) { state.expanded.delete(path); paintTree(); return; }
    state.expanded.add(path);
    paintTree();
    await loadFolder(path);
    paintTree();
    return;
  }
  if (state.file?.path === path) { el.classList.add("show-file"); return; }
  if (!(await discardOk())) return;
  void openFile(path);
});

// ---------- The open file ----------
// file: { path, scope, doc, base (text the revision holds), revision, mode,
//         editor, editable, readOnlyReason, latest (a newer read while dirty), closed }

function dirty() {
  const f = state.file;
  if (!f?.editor || !f.editable) return false;
  const doc = f.editor.view.state.doc;
  return doc.length !== f.base.length || doc.toString() !== f.base;
}

async function discardOk() {
  if (!dirty()) return true;
  return confirmDialog({ title: "Discard unsaved changes?", body: `${nameOf(state.file.path)} has edits that are not saved.`, confirm: "Discard", danger: true });
}

function closeFile() {
  state.file?.editor?.destroy();
  state.file = null;
  fileEl.hidden = true;
  el.classList.remove("show-file", "has-file");
  contentEl.replaceChildren();
  bannerEl.hidden = true;
}

async function openFile(path) {
  closeFile();
  const file = { path, scope: state.scope, doc: null, base: "", revision: "", mode: "view", editor: null, editable: false, readOnlyReason: "", latest: null, closed: false };
  state.file = file;
  fileEl.hidden = false;
  el.classList.add("show-file", "has-file");
  pathEl.textContent = path;
  pathEl.title = path;
  statusEl.textContent = "Opening";
  modesEl.replaceChildren();
  saveBtn.hidden = true;
  paintTree();
  try {
    const { value } = await read(path);
    if (state.file !== file) return;
    await show(file, value);
  } catch (e) {
    if (state.file !== file) return;
    statusEl.textContent = "";
    contentEl.replaceChildren(errorRow(e));
  }
}

/** Show a freshly read document, keeping the current mode where it applies. */
async function show(file, doc) {
  file.doc = doc;
  file.revision = doc.revision;
  const text = typeof doc.text === "string" ? doc.text : null;
  file.base = text ?? "";
  file.editable = text !== null && utf8Bytes(text) <= EDIT_BYTES;
  file.readOnlyReason = text !== null && !file.editable ? "Read only: files over 1 MiB cannot be edited here" : "";
  if (file.mode === "view" && !["markdown", "html", "image", "pdf"].includes(doc.kind) && text !== null) file.mode = "edit";
  if (file.editor) {
    if (file.editor.text() !== file.base) file.editor.setText(file.base);
    file.editor.setReadOnly(!file.editable);
  }
  await paintContent(file);
}

function paintFileControls() {
  const file = state.file;
  if (!file?.doc) return;
  const kind = file.doc.kind;
  const text = typeof file.doc.text === "string";
  modesEl.replaceChildren();
  if (["markdown", "html"].includes(kind) && text) {
    for (const [mode, label, glyph] of [["view", "Preview", "eye"], ["edit", "Edit", "edit"]]) {
      const b = h(`<button type="button" class="fp-mode">${icon(glyph)}<span>${label}</span></button>`);
      b.classList.toggle("on", file.mode === mode);
      b.setAttribute("aria-pressed", String(file.mode === mode));
      b.addEventListener("click", () => { if (file.mode !== mode) { file.mode = mode; void paintContent(file); } });
      modesEl.appendChild(b);
    }
  }
  const editing = file.mode === "edit" && !!file.editor;
  saveBtn.hidden = !editing || !file.editable;
  saveBtn.disabled = !dirty() || file.saving || file.closed;
  if (file.closed) statusEl.textContent = "Workspace closed";
  else if (file.saving) statusEl.textContent = "Saving";
  else if (editing && file.readOnlyReason) statusEl.textContent = file.readOnlyReason;
  else if (editing && dirty()) statusEl.textContent = "Edited";
  else if (!statusEl.dataset.flash) statusEl.textContent = "";
}

async function paintContent(file) {
  const doc = file.doc;
  const scroller = contentEl;
  const top = scroller.scrollTop;
  statusEl.textContent = "";
  paintFileControls();
  if (file.mode === "edit" && typeof doc.text === "string") {
    if (!file.editor) {
      file.pending ??= createEditor({
        parent: h(`<div class="fp-editor"></div>`), text: file.base, path: file.path, readOnly: !file.editable,
        onChange: () => paintFileControls(),
        onSave: () => { void save(); },
      });
      let editor;
      try { editor = await file.pending; }
      catch (e) { file.pending = null; if (state.file === file) contentEl.replaceChildren(errorRow(new Error("The editor could not load. Check your connection and open the file again."))); return; }
      if (state.file !== file) { if (file.pending) { file.pending = null; editor.destroy(); } return; }
      file.editor = editor;
      // Text that arrived while the editor loaded.
      if (editor.text() !== file.base) editor.setText(file.base);
      editor.setReadOnly(!file.editable);
    }
    if (file.mode !== "edit") return;
    const host = file.editor.view.dom.parentElement;
    if (host.parentElement !== contentEl) { contentEl.replaceChildren(host); file.editor.view.requestMeasure(); }
    paintFileControls();
    return;
  }
  const view = await renderView(file, doc);
  if (state.file !== file || file.mode === "edit") return;
  contentEl.replaceChildren(view);
  scroller.scrollTop = top;
}

async function renderView(file, doc) {
  if (doc.kind === "image") {
    const img = document.createElement("img");
    img.className = "fp-image"; img.alt = file.path; img.src = imageData(doc) ?? "";
    return img;
  }
  if (doc.kind === "pdf") {
    const frame = document.createElement("iframe");
    frame.className = "fp-frame"; frame.title = file.path; frame.referrerPolicy = "no-referrer"; frame.src = pdfData(doc) ?? "about:blank";
    return frame;
  }
  if (doc.kind === "html" && typeof doc.text === "string") {
    const html = await prepareHtml({ ...doc, text: file.editor ? file.editor.text() : doc.text }, file.scope, {
      read: async (scope, path) => (await read(path)).value,
    }, AbortSignal.timeout(10000));
    const frame = document.createElement("iframe");
    frame.className = "fp-frame"; frame.title = file.path; frame.referrerPolicy = "no-referrer";
    frame.setAttribute("sandbox", "");
    frame.srcdoc = html;
    return frame;
  }
  if (doc.kind === "markdown" && typeof doc.text === "string") return markdownView(file, file.editor ? file.editor.text() : doc.text);
  const empty = h(`<div class="fp-empty"></div>`);
  empty.textContent = doc.kind === "binary" ? "This is a binary file. There is nothing to show." : "Nothing to show.";
  return empty;
}

/** Markdown the way chat renders it. It is built in an inert template so no
 * image loads before its source is checked; relative images come through the
 * file service, links to project files open here. */
async function markdownView(file, text) {
  await loadMarkdown();
  const inert = document.createElement("template");
  const box = inert.content.appendChild(document.createElement("div"));
  box.className = "md fp-md";
  renderMarkdown(box, text);
  // Repository Markdown must not restyle or re-id the app around it.
  for (const node of box.querySelectorAll("style, link, meta, base")) node.remove();
  for (const node of box.querySelectorAll("[style], [id]")) { node.removeAttribute("style"); node.removeAttribute("id"); }
  await Promise.all([...box.querySelectorAll("img")].map(async (img) => {
    const src = img.getAttribute("src") || "";
    if (/^(?:https?:|data:image\/)/i.test(src)) return;
    img.removeAttribute("src");
    const resolved = nearbyPath(file.path, src);
    if (resolved === null) return;
    try {
      const { value } = await read(resolved);
      const data = imageData(value);
      if (data) img.setAttribute("src", data);
    } catch { img.alt = img.alt || `${resolved} is not available`; }
  }));
  for (const a of box.querySelectorAll("a[href]")) {
    const href = a.getAttribute("href");
    if (/^[a-z][a-z\d+.-]*:/i.test(href) || href.startsWith("#") || href.startsWith("/")) continue;
    const resolved = nearbyPath(file.path, href);
    if (resolved === null) continue;
    a.removeAttribute("target");
    a.dataset.projectPath = resolved;
  }
  return document.adoptNode(box);
}

contentEl.addEventListener("click", async (e) => {
  const link = e.target.closest("a[data-project-path]");
  if (!link) return;
  e.preventDefault();
  const path = link.dataset.projectPath;
  if (!(await discardOk())) return;
  const folder = parentOf(path);
  if (folder) state.expanded.add(folder);
  void openFile(path);
  void refreshTree();
});

// ---------- Changes from agents ----------

async function pollFile(force = false) {
  const file = state.file;
  if (!file?.doc || file.saving || file.closed || (!force && document.visibilityState !== "visible")) return;
  try {
    const { scope, value } = await read(file.path, file.revision);
    if (state.file !== file || scope !== file.scope || value.unchanged) return;
    if (dirty()) { file.latest = value; paintConflict(file, "An agent changed this file while you were editing it."); return; }
    await show(file, value);
  } catch (e) {
    if (state.file !== file) return;
    if (e?.status === 403) state.scopesAt = 0;
    statusEl.textContent = e?.message || "Unable to refresh this file";
  }
}

function paintConflict(file, message) {
  const text = h(`<span></span>`);
  text.textContent = message;
  bannerEl.replaceChildren(text,
    button("btn fp-latest", "Load latest", "Load latest", () => {
      const latest = file.latest;
      file.latest = null;
      bannerEl.hidden = true;
      if (latest) void show(file, latest);
    }),
    button("btn fp-overwrite", "Overwrite with mine", "Overwrite with mine", () => { void save(file.latest?.revision); }));
  bannerEl.hidden = false;
}

async function save(baseRevision) {
  const file = state.file;
  if (!file?.editor || !file.editable || file.saving || file.closed || !state.scope) return;
  if (!dirty() && !baseRevision) return;
  const text = file.editor.text();
  file.saving = true;
  paintFileControls();
  try {
    const scope = file.scope;
    await renewScopes();
    const result = await api("/api/app/project-files", { method: "POST", body: { version: 1, operation: "write", scope, path: file.path, baseRevision: baseRevision || file.revision, text } });
    if (state.file !== file) return;
    if (result.outcome === "saved") {
      file.revision = result.revision;
      file.base = text;
      file.doc = { ...file.doc, revision: result.revision, text };
      file.latest = null;
      bannerEl.hidden = true;
      flash("Saved");
    } else {
      const { value } = await read(file.path);
      if (state.file !== file) return;
      file.latest = value;
      paintConflict(file, "This file changed since you opened it. Nothing was saved.");
    }
  } catch (e) {
    if (state.file === file) { if (e?.status === 403) state.scopesAt = 0; flash(e?.message || "Unable to save this file"); }
  } finally {
    file.saving = false;
    if (state.file === file) paintFileControls();
  }
}

function flash(text) {
  statusEl.textContent = text;
  statusEl.dataset.flash = "1";
  setTimeout(() => { if (statusEl.textContent === text) { delete statusEl.dataset.flash; paintFileControls(); } }, 2500);
}

// ---------- Timers ----------

function stopTimers() { clearTimeout(state.fileTimer); clearTimeout(state.treeTimer); state.fileTimer = state.treeTimer = 0; }

function schedule() {
  stopTimers();
  if (!state.open) return;
  const fileTick = async () => { await pollFile(); if (state.open) state.fileTimer = setTimeout(fileTick, FILE_POLL_MS); };
  const treeTick = async () => { if (document.visibilityState === "visible") await refreshTree(); if (state.open) state.treeTimer = setTimeout(treeTick, TREE_POLL_MS); };
  state.fileTimer = setTimeout(fileTick, FILE_POLL_MS);
  state.treeTimer = setTimeout(treeTick, TREE_POLL_MS);
}
