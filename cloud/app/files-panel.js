// The Files panel: a project's tree and one open file beside the conversation,
// in the right-hand slot it shares with Canvas, the terminal and Notes
// (side-panel.js): a desktop column, or narrower a full-height sheet.
//
// Every read and save goes through the home's file service with an observed
// workspace scope: the project checkout or a live agent's own workspace (its
// fork mount on Linux). The browser never names a host path.
import { api, track } from "/app/core.js";
import { h, icon } from "/app/render.js";
import { FileTree, FileView, iconButton, errorRow } from "/app/file-view.js";
import { dropdown } from "/app/select.js";
import { openSide, closeSide, placeSide, dragColumn } from "/app/side-panel.js";

const CHECKOUT = "checkout";
const TREE_POLL_MS = 6000;
// Scope bindings expire after 60s on the control plane; renew well before.
const SCOPE_RENEW_MS = 40000;

const scopeKey = (s) => JSON.stringify([s.projectId, s.workspaceId, s.attemptId, s.ownerEpoch]);

/** A workspace scope as a file source. `renew` runs before each request so
 * the control plane's binding is current; a 403 asks for a fresh one. */
export function projectFileSource(scope, { renew, denied }) {
  const fields = () => ({ projectId: scope.projectId, workspaceId: scope.workspaceId, attemptId: scope.attemptId, ownerEpoch: String(scope.ownerEpoch) });
  const call = async (run) => {
    await renew();
    try { return await run(); } catch (e) { if (e?.status === 403) denied(); throw e; }
  };
  const query = (params) => call(() => api(`/api/app/project-files?${new URLSearchParams({ version: "1", ...params, ...fields() })}`));
  return {
    key: scopeKey(scope),
    scope,
    list: (path) => query({ operation: "list", path }),
    read: (path, knownRevision) => query({ operation: "read", path, ...(knownRevision ? { knownRevision } : {}) }),
    save: (path, baseRevision, text) => call(() => api("/api/app/project-files", { method: "POST", body: { version: 1, operation: "write", scope, path, baseRevision, text } })),
  };
}

/** The file workspaces of a project: its checkout and each live agent's own. */
export async function projectFileScopes(ws) {
  const response = await api(`/api/app/project-file-scopes?${new URLSearchParams({ workspaceId: ws })}`);
  if (!Array.isArray(response?.scopes)) throw new Error("Invalid file workspaces");
  return response.scopes;
}

// ---------- One panel per page ----------

const state = {
  open: false,
  target: null,       // { ws, run, findAgent }
  scopes: [],
  scopesAt: 0,
  scope: null,
  notice: "",
  tree: null,         // FileTree of the chosen scope
  treeTimer: 0,
};

const el = h(`<section class="fp" aria-label="Files">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="fp-head">
    <span class="fp-title">${icon("folder")}<span>Files</span></span>
    <select class="fp-ws" aria-label="Workspace"></select>
    <button type="button" class="icon-btn fp-refresh" aria-label="Refresh" title="Refresh">${icon("refresh")}</button>
    <button type="button" class="icon-btn fp-close" aria-label="Close files" title="Close files">${icon("x")}</button>
  </header>
  <div class="fp-notice" hidden></div>
  <div class="fp-body"><nav class="fp-tree"></nav></div>
</section>`);
const $ = (s) => el.querySelector(s);
const wsSelect = $(".fp-ws"), noticeEl = $(".fp-notice"), bodyEl = $(".fp-body");
// The app's dropdown (select.js) over the select, which stays the value.
dropdown(wsSelect);

const view = FileView({
  onBack: () => el.classList.remove("show-file"),
  onLink: (path) => { void openFile(path); void state.tree?.reveal(path); },
  onChange: () => {
    el.classList.toggle("has-file", !!view.file);
    el.classList.toggle("show-file", !!view.file);
    state.tree?.paint();
  },
});
bodyEl.appendChild(view.el);

$(".fp-close").addEventListener("click", () => { void closePanel(); });
$(".fp-refresh").addEventListener("click", () => { state.scopesAt = 0; void state.tree?.refresh(); void view.refresh(); });
wsSelect.addEventListener("change", async () => {
  const next = state.scopes.find((s) => scopeKey(s) === wsSelect.value);
  if (!next || !(await view.discardOk())) { paintWorkspaces(); return; }
  selectScope(next);
});
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible" && state.open) schedule(); });

// ---------- The right-hand slot (side-panel.js) ----------

// Another panel taking the slot closes Files, unless it holds unsaved edits.
const panel = { el, close: () => closePanel() };
$(".side-grip").addEventListener("pointerdown", dragColumn);

// ---------- Public surface ----------

/** The topbar Files button of a project or agent screen (`run` null for the
 * project). `findAgent(ws, run)` names agents in the workspace picker. */
export function filesButton({ ws, run = null, findAgent }) {
  return iconButton("icon-btn fp-toggle", "Files", icon("folder"), () => { void togglePanel({ ws, run, findAgent }); });
}

/** The page's route changed, or the other of two panes took focus: an open
 * panel follows that project and agent. Unsaved edits keep it where it is. */
export function followFiles(route, findAgent) {
  if (!state.open) return;
  placeSide(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  if (view.dirty()) return;
  if (!route?.ws || !["project", "agent"].includes(route.name)) { void closePanel(); return; }
  const target = { ws: route.ws, run: route.name === "agent" ? route.run : null, findAgent };
  if (!sameTarget(state.target, target)) void retarget(target);
}

const sameTarget = (a, b) => !!a && !!b && a.ws === b.ws && a.run === b.run;
const agentName = (target, id) => target?.findAgent(target.ws, id)?.title || null;

async function togglePanel(target) {
  if (state.open && sameTarget(state.target, target)) { await closePanel(); return; }
  if (!(await openSide(panel))) return;
  state.open = true;
  track("feature_used", { feature: "files" });
  if (!sameTarget(state.target, target)) {
    if (!(await view.discardOk())) return;
    await retarget(target);
  } else schedule();
}

/** Resolves false when unsaved edits keep the panel open. */
async function closePanel() {
  if (!(await view.discardOk())) return false;
  state.open = false;
  clearTimeout(state.treeTimer);
  view.close();
  closeSide(panel);
  return true;
}

async function retarget(target) {
  state.target = target;
  state.scope = null;
  state.scopes = [];
  state.scopesAt = 0;
  view.close();
  setTree(null, h(`<div class="fp-empty">Loading files</div>`));
  try { await renewScopes(); }
  catch (e) { if (state.target === target) setTree(null, errorRow(e)); return; }
  // The panel moved to another project or agent while this one loaded.
  if (state.target !== target) return;
  const want = target.run ? state.scopes.find((s) => s.workspaceId === target.run) : null;
  const checkout = state.scopes.find((s) => s.workspaceId === CHECKOUT);
  const notice = target.run && !want ? `${agentName(target, target.run) || "This agent"} is not running. Showing the main checkout.` : "";
  const first = want || checkout || state.scopes[0];
  if (!first) { paintWorkspaces(); setTree(null, h(`<div class="fp-empty">No file workspaces are open. Start the project's engine on its home machine.</div>`)); return; }
  selectScope(first, notice);
}

// ---------- Scopes ----------

async function renewScopes() {
  const ws = state.target?.ws;
  if (!ws || Date.now() - state.scopesAt < SCOPE_RENEW_MS) return;
  const scopes = await projectFileScopes(ws);
  if (state.target?.ws !== ws) return;
  state.scopes = scopes;
  state.scopesAt = Date.now();
  if (state.scope && !state.scopes.some((s) => scopeKey(s) === scopeKey(state.scope))) {
    state.scope = null;
    state.notice = "That workspace closed. Choose another one.";
    view.markClosed();
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

/** The chosen scope's tree, or a message where it goes. */
function setTree(tree, message = null) {
  state.tree?.destroy();
  state.tree = tree;
  const slot = tree ? tree.el : h(`<nav class="fp-tree"></nav>`);
  if (message) slot.replaceChildren(message);
  bodyEl.querySelector(".fp-tree").replaceWith(slot);
}

function selectScope(scope, notice = "") {
  state.scope = scope;
  state.notice = notice;
  view.close();
  const source = projectFileSource(scope, { renew: renewScopes, denied: () => { state.scopesAt = 0; } });
  setTree(FileTree({ source, label: "Project tree", current: () => (view.file?.source === source ? view.file.path : null), onOpen: (path) => { void openFile(path); } }));
  paintWorkspaces();
  void state.tree.refresh();
  schedule();
}

async function openFile(path) {
  const tree = state.tree;
  if (!tree) return;
  if (view.file?.source === tree.source && view.file.path === path) { el.classList.add("show-file"); return; }
  if (!(await view.discardOk())) return;
  await view.open(tree.source, path);
}

// ---------- Timers ----------

function schedule() {
  clearTimeout(state.treeTimer);
  if (!state.open) return;
  const tick = async () => {
    if (document.visibilityState === "visible") await state.tree?.refresh();
    if (state.open) state.treeTimer = setTimeout(tick, TREE_POLL_MS);
  };
  state.treeTimer = setTimeout(tick, TREE_POLL_MS);
}
