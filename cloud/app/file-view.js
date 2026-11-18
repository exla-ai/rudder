// A folder tree and one open file over any file source: the Files panel beside
// a conversation and the Files page both use these.
//
// A source names one place files live and how to reach them:
//   { key, list(path), read(path, knownRevision), save(path, baseRevision, text) }
// list/read resolve to the service's listing or document; save to
// { outcome: "saved", revision } or { outcome: "conflict" }. Saves carry the
// revision the text was read at, so a change made since (by an agent) is never
// silently replaced.
import { confirmDialog } from "/app/core.js";
import { h, icon, loadMarkdown, renderMarkdown } from "/app/render.js";
import { prepareHtml, imageData, pdfData, nearbyPath } from "/app/project-file-render.js";
import { createEditor } from "/app/code-editor.js";

const FILE_POLL_MS = 2000;
const EDIT_BYTES = 1024 * 1024;

const utf8Bytes = (text) => new TextEncoder().encode(text).length;
export const parentOf = (path) => path.split("/").slice(0, -1).join("/");
export const nameOf = (path) => path.split("/").pop() || "";

export function iconButton(className, label, html, run) {
  const b = h(`<button type="button" class="${className}">${html}</button>`);
  b.setAttribute("aria-label", label);
  b.title = label;
  b.addEventListener("click", run);
  return b;
}

export function errorRow(e, fallback = "Unable to open these files") {
  const row = h(`<div class="fp-empty fp-error"></div>`);
  row.textContent = e?.message || fallback;
  return row;
}

function note(text, depth, error = false) {
  const row = h(`<div class="fp-note${error ? " fp-error" : ""}"></div>`);
  row.style.setProperty("--depth", depth);
  row.textContent = text;
  return row;
}

// ---------- Tree ----------

/** One source's folders, loaded as they are expanded. `current()` is the path
 * of the file open from this source (highlighted), or null. */
export function FileTree({ source, onOpen, current = () => null, label = "Files" }) {
  const el = h(`<nav class="fp-tree"></nav>`);
  el.setAttribute("aria-label", label);
  const folders = new Map(); // path -> { entries, truncated, error }
  const expanded = new Set([""]);
  let generation = 0;
  let painted = "";

  async function load(path) {
    try {
      const value = await source.list(path);
      folders.set(path, { entries: value.entries, truncated: value.truncated });
    } catch (e) {
      folders.set(path, { entries: [], truncated: false, error: e?.message || "Unable to open this folder" });
    }
  }

  async function refresh() {
    const mine = ++generation;
    for (const path of [...expanded]) {
      await load(path);
      if (mine !== generation) return;
    }
    paint();
  }

  function paint() {
    const key = JSON.stringify([[...folders], [...expanded], current()]);
    if (key === painted && el.childElementCount) return;
    painted = key;
    const rows = [];
    const open = current();
    const walk = (path, depth) => {
      const folder = folders.get(path);
      if (!folder) { if (expanded.has(path)) rows.push(note("Loading", depth)); return; }
      if (folder.error) { rows.push(note(folder.error, depth, true)); return; }
      if (!folder.entries.length && depth === 0) rows.push(note("This folder is empty", depth));
      for (const entry of folder.entries) {
        const isOpen = entry.kind === "folder" && expanded.has(entry.path);
        const row = h(`<button type="button" class="fp-row"><span class="fp-chev">${entry.kind === "folder" ? icon("chevron") : ""}</span>${icon(entry.kind === "folder" ? "folder" : "file")}<span class="fp-name"></span></button>`);
        row.querySelector(".fp-name").textContent = entry.name;
        row.style.setProperty("--depth", depth);
        row.dataset.path = entry.path;
        row.dataset.kind = entry.kind;
        row.classList.toggle("open", isOpen);
        row.classList.toggle("on", open === entry.path);
        rows.push(row);
        if (isOpen) walk(entry.path, depth + 1);
      }
      if (folder.truncated) rows.push(note("More files are in this folder than the tree shows", depth));
    };
    walk("", 0);
    el.replaceChildren(...rows);
  }

  el.addEventListener("click", async (e) => {
    const row = e.target.closest(".fp-row");
    if (!row || !el.contains(row)) return;
    const { path, kind } = row.dataset;
    if (kind === "folder") {
      if (expanded.has(path)) { expanded.delete(path); paint(); return; }
      expanded.add(path);
      paint();
      await load(path);
      paint();
      return;
    }
    onOpen(path);
  });

  paint();
  return {
    el, source, refresh, paint,
    /** Show `path`'s folder open, as when a Markdown link opens a file. */
    reveal(path) { const folder = parentOf(path); if (folder) expanded.add(folder); return refresh(); },
    destroy() { generation++; },
  };
}

// ---------- The open file ----------

const views = new Set();
window.addEventListener("beforeunload", (e) => { if ([...views].some((v) => v.dirty())) e.preventDefault(); });

/** One open file: Markdown and HTML preview first with an Edit toggle, code
 * straight in the editor, explicit save, and a banner when an agent changed
 * the file while it had unsaved edits.
 *   onBack: the narrow layout's back button. onLink(path): a relative link in
 *   a preview. onChange(): a file opened or closed. */
export function FileView({ onBack = null, onLink, onChange = () => {} }) {
  const el = h(`<div class="fp-file" hidden>
    <div class="fp-file-head">
      <button type="button" class="icon-btn fp-back" aria-label="Back to files" title="Back to files">${icon("back")}</button>
      <span class="fp-path"></span>
      <span class="fp-modes" role="group" aria-label="View"></span>
      <span class="fp-status" role="status"></span>
      <button type="button" class="btn primary fp-save" disabled>Save</button>
    </div>
    <div class="fp-banner" hidden role="alert"></div>
    <div class="fp-content"></div>
  </div>`);
  const $ = (s) => el.querySelector(s);
  const pathEl = $(".fp-path"), modesEl = $(".fp-modes"), statusEl = $(".fp-status"), saveBtn = $(".fp-save");
  const bannerEl = $(".fp-banner"), contentEl = $(".fp-content"), backBtn = $(".fp-back");
  backBtn.hidden = !onBack;
  if (onBack) backBtn.addEventListener("click", onBack);
  saveBtn.addEventListener("click", () => { void save(); });

  // { source, path, doc, base (text the revision holds), revision, mode, editor,
  //   pending, editable, readOnlyReason, latest (a newer read while dirty), closed, saving,
  //   readError (the last poll failed and its message shows) }
  let file = null;
  let timer = 0;

  function dirty() {
    if (!file?.editor || !file.editable) return false;
    const doc = file.editor.view.state.doc;
    return doc.length !== file.base.length || doc.toString() !== file.base;
  }

  async function discardOk() {
    if (!dirty()) return true;
    return confirmDialog({ title: "Discard unsaved changes?", body: `${nameOf(file.path)} has edits that are not saved.`, confirm: "Discard", danger: true });
  }

  function close() {
    clearTimeout(timer);
    file?.editor?.destroy();
    file = null;
    el.hidden = true;
    contentEl.replaceChildren();
    bannerEl.hidden = true;
    onChange();
  }

  async function open(source, path) {
    close();
    const mine = { source, path, doc: null, base: "", revision: "", mode: "view", editor: null, editable: false, readOnlyReason: "", latest: null, closed: false };
    file = mine;
    el.hidden = false;
    pathEl.textContent = path;
    pathEl.title = path;
    statusEl.textContent = "Opening";
    modesEl.replaceChildren();
    saveBtn.hidden = true;
    onChange();
    try {
      const value = await source.read(path);
      if (file !== mine) return;
      await show(mine, value);
      poll();
    } catch (e) {
      if (file !== mine) return;
      statusEl.textContent = "";
      contentEl.replaceChildren(errorRow(e, "Unable to open this file"));
    }
  }

  /** Show a freshly read document, keeping the current mode where it applies. */
  async function show(f, doc) {
    f.doc = doc;
    f.revision = doc.revision;
    const text = typeof doc.text === "string" ? doc.text : null;
    f.base = text ?? "";
    f.editable = text !== null && utf8Bytes(text) <= EDIT_BYTES;
    f.readOnlyReason = text !== null && !f.editable ? "Read only: files over 1 MiB cannot be edited here" : "";
    if (f.mode === "view" && !["markdown", "html", "image", "pdf"].includes(doc.kind) && text !== null) f.mode = "edit";
    if (f.editor) {
      if (f.editor.text() !== f.base) f.editor.setText(f.base);
      f.editor.setReadOnly(!f.editable);
    }
    await paintContent(f);
  }

  function paintControls() {
    const f = file;
    if (!f?.doc) return;
    const kind = f.doc.kind;
    modesEl.replaceChildren();
    if (["markdown", "html"].includes(kind) && typeof f.doc.text === "string") {
      for (const [mode, label, glyph] of [["view", "Preview", "eye"], ["edit", "Edit", "edit"]]) {
        const b = h(`<button type="button" class="fp-mode">${icon(glyph)}<span>${label}</span></button>`);
        b.classList.toggle("on", f.mode === mode);
        b.setAttribute("aria-pressed", String(f.mode === mode));
        b.addEventListener("click", () => { if (f.mode !== mode) { f.mode = mode; void paintContent(f); } });
        modesEl.appendChild(b);
      }
    }
    const editing = f.mode === "edit" && !!f.editor;
    saveBtn.hidden = !editing || !f.editable;
    saveBtn.disabled = !dirty() || f.saving || f.closed;
    if (f.closed) statusEl.textContent = "Workspace closed";
    else if (f.saving) statusEl.textContent = "Saving";
    else if (editing && f.readOnlyReason) statusEl.textContent = f.readOnlyReason;
    else if (editing && dirty()) statusEl.textContent = "Edited";
    else if (!statusEl.dataset.flash) statusEl.textContent = "";
  }

  async function paintContent(f) {
    const doc = f.doc;
    const top = contentEl.scrollTop;
    statusEl.textContent = "";
    paintControls();
    if (f.mode === "edit" && typeof doc.text === "string") {
      if (!f.editor) {
        f.pending ??= createEditor({
          parent: h(`<div class="fp-editor"></div>`), text: f.base, path: f.path, readOnly: !f.editable,
          onChange: () => paintControls(),
          onSave: () => { void save(); },
        });
        let editor;
        try { editor = await f.pending; }
        catch { f.pending = null; if (file === f) contentEl.replaceChildren(errorRow(new Error("The editor could not load. Check your connection and open the file again."))); return; }
        if (file !== f) { if (f.pending) { f.pending = null; editor.destroy(); } return; }
        f.editor = editor;
        // Text that arrived while the editor loaded.
        if (editor.text() !== f.base) editor.setText(f.base);
        editor.setReadOnly(!f.editable);
      }
      if (f.mode !== "edit") return;
      const host = f.editor.view.dom.parentElement;
      if (host.parentElement !== contentEl) { contentEl.replaceChildren(host); f.editor.view.requestMeasure(); }
      paintControls();
      return;
    }
    const view = await renderView(f, doc);
    if (file !== f || f.mode === "edit") return;
    contentEl.replaceChildren(view);
    contentEl.scrollTop = top;
  }

  async function renderView(f, doc) {
    if (doc.kind === "image") {
      const img = document.createElement("img");
      img.className = "fp-image"; img.alt = f.path; img.src = imageData(doc) ?? "";
      return img;
    }
    if (doc.kind === "pdf") {
      const frame = document.createElement("iframe");
      frame.className = "fp-frame"; frame.title = f.path; frame.referrerPolicy = "no-referrer"; frame.src = pdfData(doc) ?? "about:blank";
      return frame;
    }
    if (doc.kind === "html" && typeof doc.text === "string") {
      const html = await prepareHtml({ ...doc, text: f.editor ? f.editor.text() : doc.text }, f.source.key, {
        read: async (_scope, path) => f.source.read(path),
      }, AbortSignal.timeout(10000));
      const frame = document.createElement("iframe");
      frame.className = "fp-frame"; frame.title = f.path; frame.referrerPolicy = "no-referrer";
      frame.setAttribute("sandbox", "");
      frame.srcdoc = html;
      return frame;
    }
    if (doc.kind === "markdown" && typeof doc.text === "string") return markdownView(f, f.editor ? f.editor.text() : doc.text);
    const empty = h(`<div class="fp-empty"></div>`);
    empty.textContent = doc.kind === "binary" ? "This is a binary file. There is nothing to show." : "Nothing to show.";
    return empty;
  }

  /** Markdown the way chat renders it. It is built in an inert template so no
   * image loads before its source is checked; relative images come through
   * the source, links to its files open here. */
  async function markdownView(f, text) {
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
      const resolved = nearbyPath(f.path, src);
      if (resolved === null) return;
      try {
        const data = imageData(await f.source.read(resolved));
        if (data) img.setAttribute("src", data);
      } catch { img.alt = img.alt || `${resolved} is not available`; }
    }));
    for (const a of box.querySelectorAll("a[href]")) {
      const href = a.getAttribute("href");
      if (/^[a-z][a-z\d+.-]*:/i.test(href) || href.startsWith("#") || href.startsWith("/")) continue;
      const resolved = nearbyPath(f.path, href);
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
    if (!(await discardOk())) return;
    onLink(link.dataset.projectPath);
  });

  // ---------- Changes from agents ----------

  async function check(force = false) {
    const f = file;
    if (!f?.doc || f.saving || f.closed || (!force && document.visibilityState !== "visible")) return;
    try {
      const value = await f.source.read(f.path, f.revision);
      if (file !== f) return;
      // A read that works again clears the last one's error.
      if (f.readError) { f.readError = false; paintControls(); }
      if (value.unchanged) return;
      if (dirty()) { f.latest = value; paintConflict(f, "An agent changed this file while you were editing it."); return; }
      await show(f, value);
    } catch (e) {
      if (file !== f) return;
      f.readError = true;
      statusEl.textContent = e?.message || "Unable to refresh this file";
    }
  }

  function poll() {
    clearTimeout(timer);
    const f = file;
    timer = setTimeout(async () => {
      if (file !== f) return;
      if (el.isConnected) await check();
      if (file === f) poll();
    }, FILE_POLL_MS);
  }

  function paintConflict(f, message) {
    const text = h(`<span></span>`);
    text.textContent = message;
    bannerEl.replaceChildren(text,
      iconButton("btn fp-latest", "Load latest", "Load latest", () => {
        const latest = f.latest;
        f.latest = null;
        bannerEl.hidden = true;
        if (latest) void show(f, latest);
      }),
      iconButton("btn fp-overwrite", "Overwrite with mine", "Overwrite with mine", () => { void save(f.latest?.revision); }));
    bannerEl.hidden = false;
  }

  async function save(baseRevision) {
    const f = file;
    if (!f?.editor || !f.editable || f.saving || f.closed) return;
    if (!dirty() && !baseRevision) return;
    const text = f.editor.text();
    f.saving = true;
    paintControls();
    try {
      const result = await f.source.save(f.path, baseRevision || f.revision, text);
      if (file !== f) return;
      if (result.outcome === "saved") {
        f.revision = result.revision;
        f.base = text;
        f.doc = { ...f.doc, revision: result.revision, text };
        f.latest = null;
        bannerEl.hidden = true;
        flash("Saved");
      } else {
        const latest = await f.source.read(f.path);
        if (file !== f) return;
        f.latest = latest;
        paintConflict(f, "This file changed since you opened it. Nothing was saved.");
      }
    } catch (e) {
      if (file === f) flash(e?.message || "Unable to save this file");
    } finally {
      f.saving = false;
      if (file === f) paintControls();
    }
  }

  // A short status ("Saved") that paintControls leaves alone until it ends.
  let flashTimer = 0;
  function flash(text) {
    clearTimeout(flashTimer);
    statusEl.textContent = text;
    statusEl.dataset.flash = "1";
    flashTimer = setTimeout(() => { delete statusEl.dataset.flash; paintControls(); }, 2500);
  }

  const view = {
    el, open, close, dirty, discardOk,
    /** The open file: { source, path } or null. */
    get file() { return file ? { source: file.source, path: file.path } : null; },
    /** Read the open file again now, as the refresh button does. */
    refresh: () => check(true),
    /** Its workspace went away: keep the text on screen, but never save it there. */
    markClosed() { if (file) { file.closed = true; paintControls(); } },
    destroy() { close(); views.delete(view); },
  };
  views.add(view);
  return view;
}
