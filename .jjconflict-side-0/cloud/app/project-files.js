import { api } from "/app/core.js";
import { h } from "/app/render.js";
import { prepareHtml, imageData, pdfData } from "/app/project-file-render.js";

const REFRESH_MS = 2000;
// Renew an observed binding before its 60s expiry, leaving room for the whole
// bounded document/asset cycle. Renewal still requires the current account ACL.
const SCOPE_REFRESH_MS = 40000;
const scopeKey = scope => JSON.stringify([scope.projectId, scope.workspaceId, scope.attemptId, scope.ownerEpoch]);
function exactScope(item) {
  if (!item || ![item.projectId, item.workspaceId, item.attemptId].every(id => typeof id === "string" && id.length > 0 && id.length <= 256 && !/[\x00-\x1f\x7f]/.test(id))
    || !Number.isSafeInteger(item.ownerEpoch) || item.ownerEpoch < 0) throw new Error("Invalid file workspace");
  return { projectId: item.projectId, workspaceId: item.workspaceId, attemptId: item.attemptId, ownerEpoch: item.ownerEpoch };
}
async function cancellable(work, signal) {
  signal.throwIfAborted();
  let cancel;
  const aborted = new Promise((_, reject) => {
    cancel = () => reject(signal.reason || new Error("File request cancelled"));
    signal.addEventListener("abort", cancel, { once: true });
  });
  try { return await Promise.race([work, aborted]); }
  finally { signal.removeEventListener("abort", cancel); }
}

// Reads and nearby assets share the account session and an observed execution
// scope. Saved UI state never manufactures a binding or a managed VFS grant.
export function ProjectFiles(workspaceId) {
  const el = h('<details class="section"><summary>Project files</summary><div class="file-workspaces"></div><div class="file-actions"></div><div class="file-result" role="status"></div></details>');
  const workspaces = el.querySelector(".file-workspaces"), actions = el.querySelector(".file-actions"), result = el.querySelector(".file-result");
  let scope, current = "", currentKind = "folder", file, source = false, generation = 0, destroyed = false;
  let controller, timer, scopeCheckedAt = -Infinity, painted = "";
  const button = (label, run) => { const b = window.document.createElement("button"); b.type = "button"; b.textContent = label; b.addEventListener("click", run); return b; };
  const error = e => { painted = ""; result.textContent = e?.message || "Unable to open project files"; };
  function cancel() { clearTimeout(timer); timer = undefined; generation++; controller?.abort(); }
  function begin() {
    cancel(); controller = new AbortController();
    const active = controller, token = generation;
    const timeout = setTimeout(() => active.abort(new Error("File request timed out")), 10000);
    return { token, signal: active.signal, finish: () => clearTimeout(timeout) };
  }
  const currentRequest = token => !destroyed && el.open && token === generation;
  function schedule(token) {
    if (currentRequest(token) && scope) timer = setTimeout(() => { void load(current, currentKind, true); }, REFRESH_MS);
  }
  async function request(operation, path, selected, signal) {
    signal.throwIfAborted();
    const query = new URLSearchParams({ version: "1", operation, ...selected, path });
    const value = await cancellable(api(`/api/app/project-files?${query}`, { signal }), signal);
    if (value?.path !== path) throw new Error("Project file identity changed");
    if (operation === "list") {
      if (!Array.isArray(value.entries) || value.entries.length > 500 || typeof value.truncated !== "boolean") throw new Error("Invalid folder response");
      const seen = new Set();
      for (const entry of value.entries) {
        if (!["folder", "file"].includes(entry.kind) || typeof entry.name !== "string" || !entry.name || /[/\\\x00-\x1f\x7f]/.test(entry.name)
          || [".", ".."].includes(entry.name) || entry.path !== (path ? path + "/" : "") + entry.name || seen.has(entry.path)) throw new Error("Invalid folder entry");
        seen.add(entry.path);
      }
    } else if (typeof value.revision !== "string" || !/^[a-f0-9]{64}$/.test(value.revision) || !["text", "markdown", "html", "image", "pdf", "binary"].includes(value.kind)
      || typeof value.mime !== "string" || value.mime.length > 100
      || value.text !== undefined && (typeof value.text !== "string" || value.text.length > 4 * 1024 * 1024)
      || value.base64 !== undefined && (typeof value.base64 !== "string" || value.base64.length > 23 * 1024 * 1024)) throw new Error("Invalid file response");
    return value;
  }
  function workspaceButtons(scopes) {
    workspaces.replaceChildren(button("Refresh workspaces", () => { void discover(); }));
    for (const item of scopes) workspaces.append(button(item.workspaceId, () => {
      scope = item; source = false; void load("", "folder");
    }));
  }
  async function scopesFor(signal) {
    signal.throwIfAborted();
    const response = await cancellable(api(`/api/app/project-file-scopes?${new URLSearchParams({ workspaceId })}`, { signal }), signal);
    if (!Array.isArray(response?.scopes) || response.scopes.length > 16) throw new Error("Invalid file workspaces");
    const scopes = response.scopes.map(exactScope);
    if (new Set(scopes.map(scopeKey)).size !== scopes.length) throw new Error("Invalid file workspaces");
    signal.throwIfAborted(); return scopes;
  }
  async function renew(signal, token) {
    if (Date.now() - scopeCheckedAt < SCOPE_REFRESH_MS) return;
    const scopes = await scopesFor(signal);
    if (!currentRequest(token)) return;
    scopeCheckedAt = Date.now(); workspaceButtons(scopes);
    const retained = scopes.find(item => scopeKey(item) === scopeKey(scope));
    if (!retained) { scope = undefined; actions.replaceChildren(); throw new Error("File workspace access changed. Choose a current workspace."); }
    scope = retained;
  }
  function navigation() {
    actions.replaceChildren(button("Up", () => { void load(current.split("/").slice(0, -1).join("/"), "folder"); }),
      button("Refresh", () => { scopeCheckedAt = -Infinity; void load(current, currentKind, true); }));
    if (file) actions.append(button("Rendered / source", () => { source = !source; void load(current, currentKind, true); }));
  }
  async function paint(value, selected, signal, token) {
    let mode, body;
    if (source || !["html", "markdown", "image", "pdf"].includes(value.kind)) {
      mode = "source"; body = value.text ?? "Source is unavailable for this binary file";
    } else if (value.kind === "image") {
      mode = "image"; body = imageData(value); if (!body) throw new Error("Unsupported image");
    } else if (value.kind === "pdf") {
      mode = "pdf"; body = pdfData(value); if (!body) throw new Error("Unsupported PDF");
    } else {
      mode = "html";
      body = await prepareHtml(value, selected, { read: (observed, path, assetSignal) => request("read", path, observed, assetSignal) }, signal);
    }
    signal.throwIfAborted();
    if (!currentRequest(token)) return;
    const key = JSON.stringify([scopeKey(selected), current, mode, body]);
    // Preserve the actual DOM/frame, scroll position and selection when neither
    // document nor nearby rendered asset bytes changed.
    if (painted === key) return;
    const node = window.document.createElement(mode === "source" ? "pre" : mode === "image" ? "img" : "iframe");
    if (mode === "source") node.textContent = body;
    else if (mode === "image") { node.alt = current; node.style.maxWidth = "100%"; node.src = body; }
    else {
      node.title = current; node.referrerPolicy = "no-referrer"; node.style.cssText = "width:100%;height:65vh;border:0;background:white";
      if (mode === "html") { node.setAttribute("sandbox", ""); node.srcdoc = body; }
      else node.src = body;
    }
    painted = key; result.replaceChildren(node);
  }
  async function load(path, kind, background = false) {
    if (destroyed || !el.open || !scope) return;
    const active = begin(); let failed = false;
    current = path; currentKind = kind;
    if (!background) { source = false; painted = ""; file = null; result.textContent = "Loading files"; actions.replaceChildren(); }
    try {
      await renew(active.signal, active.token);
      if (!currentRequest(active.token) || !scope) return;
      const selected = scope;
      const value = await request(kind === "folder" ? "list" : "read", path, selected, active.signal);
      if (!currentRequest(active.token)) return;
      file = kind === "folder" ? null : value;
      if (file) await paint(file, selected, active.signal, active.token);
      else {
        const key = JSON.stringify([scopeKey(selected), path, value.entries, value.truncated]);
        if (painted !== key) {
          const entries = value.entries.map(entry => button((entry.kind === "folder" ? "Folder: " : "") + entry.name, () => { void load(entry.path, entry.kind); }));
          if (value.truncated) entries.push("More files exist in this folder. Open a smaller folder.");
          result.replaceChildren(...entries); painted = key;
        }
      }
      if (currentRequest(active.token)) navigation();
    } catch (e) { failed = true; if (currentRequest(active.token)) error(e); }
    finally { active.finish(); if (!failed) schedule(active.token); }
  }
  async function discover() {
    if (destroyed || !el.open) return;
    const active = begin();
    try {
      const scopes = await scopesFor(active.signal);
      if (!currentRequest(active.token)) return;
      scopeCheckedAt = Date.now(); workspaceButtons(scopes);
      if (scope && scopes.some(item => scopeKey(item) === scopeKey(scope))) { void load(current, currentKind, true); return; }
      scope = undefined; file = null; painted = ""; actions.replaceChildren();
      result.textContent = scopes.length ? "Choose an active workspace" : "No active file workspaces";
    } catch (e) { if (currentRequest(active.token)) { scope = undefined; error(e); } }
    finally { active.finish(); }
  }
  el.addEventListener("toggle", () => {
    if (!el.open) cancel();
    else if (scope) void load(current, currentKind, true);
    else void discover();
  });
  return { el, destroy() { destroyed = true; cancel(); result.replaceChildren(); workspaces.replaceChildren(); actions.replaceChildren(); } };
}
