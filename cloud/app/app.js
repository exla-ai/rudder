// Rudder phone app. Plain ES modules, no build step.
import {
  icon, toolIcon, h, setText, reconcile, renderMarkdown, loadMarkdown, onMarkdownReady,
  markdownReady, copyText, escapeHtml,
} from "/app/render.js";

/* ------------------------------------------------------------------ */
/* Native shell bridge                                                 */
/* ------------------------------------------------------------------ */

const NATIVE = window.RUDDER_NATIVE || null;
function postNative(msg) {
  try { window.webkit?.messageHandlers?.rudder?.postMessage(msg); } catch { /* not in the shell */ }
}
function openExternal(url) {
  if (!url) return;
  if (NATIVE) postNative({ type: "openExternal", url });
  else window.open(url, "_blank", "noopener,noreferrer");
}

/* ------------------------------------------------------------------ */
/* Small utilities                                                     */
/* ------------------------------------------------------------------ */

const ls = {
  get(k, d = null) { try { const v = localStorage.getItem(k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* private mode */ } },
};
const coarse = matchMedia("(pointer: coarse)").matches;
const toMs = (t) => (t == null ? null : typeof t === "number" ? t : Date.parse(t) || null);

function rel(t) {
  const ms = toMs(t);
  if (!ms) return "";
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 45) return "now";
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86400) return `${Math.round(s / 3600)}h`;
  if (s < 2 * 86400) return "Yesterday";
  if (s < 7 * 86400) return `${Math.round(s / 86400)}d`;
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
function dayLabel(ms) {
  const d = new Date(ms), now = new Date();
  const start = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diff = Math.round((start(now) - start(d)) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
}

class ApiError extends Error { constructor(msg, status, waking = false) { super(msg); this.status = status; this.waking = waking; } }

/* ------------------------------------------------------------------ */
/* API                                                                 */
/* ------------------------------------------------------------------ */

async function api(path, { method = "GET", body } = {}) {
  const headers = { Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const token = NATIVE?.token || ls.get("rudder.token");
  if (token) headers.Authorization = `Bearer ${token}`;
  let res;
  try {
    res = await fetch(path, {
      method, headers, credentials: "same-origin", cache: "no-store",
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch {
    throw new ApiError("Can't reach Rudder. Check your connection.", 0);
  }
  if (res.status === 401) { handleSignedOut(); throw new ApiError("Signed out", 401); }
  let data = null;
  try { data = await res.json(); } catch { /* empty body */ }
  if (!res.ok) throw new ApiError(data?.error || `${res.status} ${res.statusText || "Request failed"}`.trim(), res.status, !!data?.waking);
  return data;
}

/* ------------------------------------------------------------------ */
/* Store                                                               */
/* ------------------------------------------------------------------ */

const store = {
  state: null,
  stateError: null,
  signedOut: false,
  transcripts: new Map(),   // `${ws}|${run}` -> transcript
  pending: new Map(),       // `${ws}|${run}` -> [{id,text,ts,state}]
  expanded: new Set(),      // tool message keys
  showMore: new Set(),
  drafts: ls.get("rudder.drafts", {}),
  badge: -1,
};

const STATUS = {
  starting: { label: "Starting", cls: "s-working", live: true },
  working: { label: "Working", cls: "s-working", live: true },
  waiting: { label: "Needs input", cls: "s-needs" },
  review: { label: "Ready for review", cls: "s-done" },
  failed: { label: "Failed", cls: "s-failed" },
  done: { label: "Done", cls: "s-done" },
  merged: { label: "Merged", cls: "s-accent" },
  paused: { label: "Paused", cls: "s-idle" },
  stopped: { label: "Stopped", cls: "s-idle" },
  orphaned: { label: "Orphaned", cls: "s-idle" },
};
const statusOf = (s) => STATUS[s] || { label: s ? s[0].toUpperCase() + s.slice(1) : "Unknown", cls: "s-idle" };
// Prompts the agent is blocked on (permission, question, plan), newest last.
// Ones answered from this phone are hidden at once, before the state agrees.
const answeredAsks = new Set();
const pendingAsks = (a) => (Array.isArray(a?.asks) ? a.asks : []).filter((q) => q && !answeredAsks.has(q.id));
const ASK_LABEL = { permission: "Needs approval", question: "Has a question", plan: "Plan ready" };
const askLabel = (q) => ASK_LABEL[q?.kind] || "Needs approval";
const needsYou = (a) => pendingAsks(a).length > 0 || a.status === "waiting" || a.status === "review" || a.status === "failed";
// An agent blocked on a prompt reads as needing you, whatever its status says.
const agentStatus = (a) => {
  const asks = pendingAsks(a);
  return asks.length ? { label: askLabel(asks[0]), cls: "s-needs" } : statusOf(a?.status);
};
const BACKEND = { claude: "Claude", codex: "Codex", opencode: "opencode" };
const backendLabel = (a) => [BACKEND[a.backend] || a.backend, a.model].filter(Boolean).join(" · ");

function findProject(ws) { return store.state?.projects?.find((p) => p.workspaceId === ws) || null; }
function findAgent(ws, run) { return findProject(ws)?.agents?.find((a) => a.id === run) || null; }
function needsList() {
  const out = [];
  for (const p of store.state?.projects || []) for (const a of p.agents || []) if (needsYou(a)) out.push({ p, a });
  out.sort((x, y) => (y.a.updatedAt || 0) - (x.a.updatedAt || 0));
  return out;
}

let stateInflight = null;
async function refreshState() {
  if (store.signedOut && !NATIVE) return;
  if (stateInflight) return stateInflight;
  stateInflight = (async () => {
    try {
      store.state = await api("/api/app/state");
      store.stateError = null;
      store.signedOut = false;
      const count = needsList().length;
      if (count !== store.badge) { store.badge = count; postNative({ type: "badge", count }); }
    } catch (err) {
      if (err.status !== 401) {
        if (!store.stateError || store.stateError !== err.message) toast(err.message, { error: true });
        store.stateError = err.message;
      }
    } finally {
      stateInflight = null;
      updateScreen();
    }
  })();
  return stateInflight;
}

let nativeSignInRequested = false;
function handleSignedOut() {
  const was = store.signedOut;
  store.signedOut = true;
  if (NATIVE && !nativeSignInRequested) { nativeSignInRequested = true; postNative({ type: "signIn" }); }
  if (!was) mountScreen();
}

/* ------------------------------------------------------------------ */
/* Toasts + dialogs                                                    */
/* ------------------------------------------------------------------ */

const toastBox = document.getElementById("toasts");
let lastToast = { text: "", at: 0 };
function toast(text, { error = false } = {}) {
  if (text === lastToast.text && Date.now() - lastToast.at < 2500) return;
  lastToast = { text, at: Date.now() };
  const el = h(`<div class="toast${error ? " error" : ""}"></div>`);
  el.textContent = text;
  toastBox.appendChild(el);
  setTimeout(() => { el.classList.add("out"); setTimeout(() => el.remove(), 220); }, error ? 4200 : 2800);
}

function confirmDialog({ title, body, confirm = "Confirm", danger = false }) {
  return new Promise((resolve) => {
    const dlg = h(`<dialog aria-labelledby="dlg-t"><div class="dialog">
      <h3 id="dlg-t"></h3><p></p>
      <div class="d-actions"><button type="button" class="btn" data-v="0">Cancel</button>
      <button type="button" class="btn ${danger ? "confirm-danger" : "primary"}" data-v="1"></button></div></div></dialog>`);
    dlg.querySelector("h3").textContent = title;
    dlg.querySelector("p").textContent = body;
    dlg.querySelector('[data-v="1"]').textContent = confirm;
    let result = false;
    dlg.addEventListener("click", (e) => {
      const b = e.target.closest("[data-v]");
      if (b) { result = b.dataset.v === "1"; dlg.close(); }
      else if (e.target === dlg) dlg.close();
    });
    dlg.addEventListener("close", () => { dlg.remove(); resolve(result); });
    document.body.appendChild(dlg);
    dlg.showModal();
  });
}

async function openProjectRepo(repo, dlg) {
  try {
    const res = await api("/api/app/open", { method: "POST", body: { repo } });
    if (res?.home && store.state) store.state.home = res.home;
    dlg?.close();
    toast(`Opening ${repo}`);
    await refreshState();
    const ws = res?.workspace?.workspaceId || res?.workspace?.id;
    if (ws) navigate(projectPath(ws));
  } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
}

function openAddProject() {
  const dlg = h(`<dialog class="sheet add-sheet" aria-label="Add project"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Add project</h3><button type="button" class="btn small new-btn">${icon("plus")}<span>New repository</span></button></div>
    <div class="owner-chips" role="tablist" aria-label="Owner"></div>
    <form class="new-repo-form" hidden>
      <div class="new-under">New repository under <strong class="new-owner"></strong></div>
      <input class="input" name="name" placeholder="repository-name" aria-label="Repository name" autocapitalize="off" autocorrect="off" spellcheck="false">
      <label class="check"><input type="checkbox" name="private" checked> Private</label>
      <div class="row"><button type="button" class="btn" data-act="cancel-new">Cancel</button><button type="submit" class="btn primary">Create and open</button></div>
    </form>
    <input class="input repo-search" type="search" placeholder="Search, or paste owner/name" aria-label="Search repositories"
      autocapitalize="off" autocorrect="off" spellcheck="false">
    <div class="repo-list" role="list"><div class="repo-note">Loading your GitHub repos…</div></div>
    <button type="button" class="btn block" style="margin-top:10px;border:0;background:transparent;color:var(--muted)" data-act="close">Close</button>
  </div></dialog>`);
  const search = dlg.querySelector(".repo-search");
  const listEl = dlg.querySelector(".repo-list");
  const chipsEl = dlg.querySelector(".owner-chips");
  const form = dlg.querySelector(".new-repo-form");
  const newBtn = dlg.querySelector(".new-btn");
  let repos = [];
  let owners = [];
  let owner = ls.get("rudder.addOwner") || "";
  const ownerOf = (repo) => repo.split("/")[0];

  function renderChips() {
    const all = ["", ...owners];
    chipsEl.replaceChildren(...all.map((o) => {
      const b = h(`<button type="button" role="tab" class="owner-chip"></button>`);
      b.setAttribute("aria-selected", String(o === owner));
      if (o) {
        const img = document.createElement("img");
        img.src = `https://github.com/${encodeURIComponent(o)}.png?size=64`;
        img.alt = ""; img.loading = "lazy";
        b.appendChild(img);
      }
      const label = document.createElement("span");
      label.textContent = o || "All";
      b.appendChild(label);
      b.addEventListener("click", () => { owner = o; ls.set("rudder.addOwner", owner); renderChips(); render(); syncForm(); });
      return b;
    }));
  }
  function syncForm() {
    const target = owner || owners[0] || "";
    form.querySelector(".new-owner").textContent = target;
    form.dataset.owner = target;
  }
  function render() {
    const q = search.value.trim().toLowerCase().replace(/^https?:\/\/github\.com\//, "").replace(/\.git$/, "");
    const shown = repos
      .filter((r) => !owner || ownerOf(r.repo).toLowerCase() === owner.toLowerCase())
      .filter((r) => !q || r.repo.toLowerCase().includes(q) || (r.description || "").toLowerCase().includes(q))
      .slice(0, 60);
    listEl.replaceChildren();
    const typed = /^[\w.-]+\/[\w.-]+$/.test(q) && !repos.some((r) => r.repo.toLowerCase() === q);
    if (typed) {
      const b = h(`<button type="button" class="repo-row" role="listitem"><span class="rn"></span><span class="rd">Open this repo</span></button>`);
      b.querySelector(".rn").textContent = q;
      b.addEventListener("click", () => openProjectRepo(q, dlg));
      listEl.appendChild(b);
    }
    for (const r of shown) {
      const b = h(`<button type="button" class="repo-row" role="listitem"><span class="rn"></span><span class="rd"></span></button>`);
      b.querySelector(".rn").textContent = owner ? r.repo.slice(owner.length + 1) : r.repo;
      b.querySelector(".rd").textContent = [r.open ? "on your cloud machine" : null, r.private ? "private" : "public", r.updatedAt ? rel(toMs(r.updatedAt)) : null, r.description].filter(Boolean).join(" · ");
      b.addEventListener("click", () => openProjectRepo(r.repo, dlg));
      listEl.appendChild(b);
    }
    if (!typed && !shown.length) {
      listEl.appendChild(h(`<div class="repo-note">${repos.length ? "No matching repos." : "No repos found."}</div>`));
    }
  }
  newBtn.addEventListener("click", () => {
    syncForm();
    form.hidden = false; newBtn.hidden = true;
    form.name.focus();
  });
  form.querySelector('[data-act="cancel-new"]').addEventListener("click", () => { form.hidden = true; newBtn.hidden = false; });
  search.addEventListener("input", render);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = form.name.value.trim();
    const target = form.dataset.owner;
    if (!target) { toast("Pick an owner first", { error: true }); return; }
    if (!/^[A-Za-z0-9._-]+$/.test(name)) { toast("Use letters, digits, . _ - for the name", { error: true }); return; }
    const btn = form.querySelector('button[type="submit"]');
    btn.disabled = true;
    try {
      const res = await api("/api/app/create", { method: "POST", body: { owner: target, name, private: form.private.checked } });
      dlg.close();
      toast(`Created ${target}/${name}`);
      await refreshState();
      const ws = res?.workspace?.workspaceId || res?.workspace?.id;
      if (ws) navigate(projectPath(ws));
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { btn.disabled = false; }
  });
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  renderChips();
  api("/api/app/repos").then((data) => {
    repos = data?.repos || [];
    owners = data?.owners || [];
    if (owner && !owners.includes(owner)) owner = "";
    renderChips(); syncForm(); render();
  }).catch((err) => {
    listEl.replaceChildren(h(`<div class="repo-note err"></div>`));
    listEl.firstChild.textContent = err.status === 401 ? "Signed out." : err.message;
  });
}

// `/model` in a project's task box: pick which agent and model new agents
// start on, from the same lists as the dashboard's picker. It sends the
// dashboard `/model <agent> <model>`, exactly what typing it there does.
function openModelPicker(ws) {
  const dlg = h(`<dialog class="sheet add-sheet" aria-label="Model for new agents"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Model for new agents</h3></div>
    <div class="owner-chips" role="tablist" aria-label="Agent"></div>
    <div class="repo-list" role="list"><div class="repo-note">Loading models…</div></div>
    <button type="button" class="btn block" style="margin-top:10px;border:0;background:transparent;color:var(--muted)" data-act="close">Close</button>
  </div></dialog>`);
  const chipsEl = dlg.querySelector(".owner-chips");
  const listEl = dlg.querySelector(".repo-list");
  let data = null;
  let backend = "claude";
  function render() {
    chipsEl.replaceChildren(...["claude", "codex"].map((b) => {
      const chip = h(`<button type="button" role="tab" class="owner-chip"><span></span></button>`);
      chip.querySelector("span").textContent = BACKEND[b];
      chip.setAttribute("aria-selected", String(b === backend));
      chip.addEventListener("click", () => { backend = b; render(); });
      return chip;
    }));
    if (!data) return;
    const options = (data[backend] || []).filter((o) => o.value);
    listEl.replaceChildren();
    for (const o of options) {
      const current = data.current?.backend === backend && data.current?.model === o.value;
      const b = h(`<button type="button" class="repo-row" role="listitem"><span class="rn"></span><span class="rd"></span></button>`);
      b.querySelector(".rn").textContent = `${o.label || o.value}${current ? "  ✓" : ""}`;
      b.querySelector(".rd").textContent = [o.value !== o.label ? o.value : null, o.detail].filter(Boolean).join(" · ");
      b.addEventListener("click", async () => {
        try {
          await api("/api/app/task", { method: "POST", body: { workspace: ws, text: `/model ${backend} ${o.value}` } });
          dlg.close();
          toast(`New agents start on ${BACKEND[backend]} · ${o.label || o.value}`);
        } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
      });
      listEl.appendChild(b);
    }
    if (!options.length) listEl.appendChild(h(`<div class="repo-note">No ${BACKEND[backend]} models found on the cloud machine.</div>`));
  }
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  render();
  api("/api/app/models").then((res) => {
    data = res || {};
    if (data.current?.backend === "codex" || data.current?.backend === "claude") backend = data.current.backend;
    render();
  }).catch((err) => {
    listEl.replaceChildren(h(`<div class="repo-note err"></div>`));
    listEl.firstChild.textContent = err.status === 401 ? "Signed out." : err.message;
  });
}

function openSettings() {
  const s = store.state || {};
  const home = s.home;
  const rows = [
    ["Account", s.email || "Signed in"],
    ["Cloud machine", home ? machineLabel(home).label : "None"],
  ];
  if (home?.tailnet?.dnsName) rows.push(["Tailnet", home.tailnet.dnsName]);
  if (NATIVE) rows.push(["App", "iOS"]);
  const dlg = h(`<dialog class="sheet" aria-label="Settings"><div class="dialog">
    <div class="sheet-grab"></div><h3>Settings</h3>
    <div class="kvs" style="margin:6px 0 20px"></div>
    <button type="button" class="btn block danger" data-act="signout">Sign out</button>
    <button type="button" class="btn block" style="margin-top:10px;border:0;background:transparent;color:var(--muted)" data-act="close">Close</button>
  </div></dialog>`);
  const kvs = dlg.querySelector(".kvs");
  for (const [k, v] of rows) {
    const r = h(`<div class="kv"><span></span><span></span></div>`);
    r.children[0].textContent = k; r.children[1].textContent = v;
    kvs.appendChild(r);
  }
  dlg.addEventListener("click", async (e) => {
    const act = e.target.closest("[data-act]")?.dataset.act;
    if (e.target === dlg || act === "close") dlg.close();
    if (act === "signout") { dlg.close(); await signOut(); }
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
}

async function signOut() {
  if (NATIVE) { postNative({ type: "signOut" }); return; }
  ls.set("rudder.token", null);
  try {
    await fetch("/api/auth/sign-out", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: "{}" });
  } catch { /* reload anyway */ }
  ls.set("rudder.lastRoute", null);
  location.reload();
}

/* ------------------------------------------------------------------ */
/* Router                                                              */
/* ------------------------------------------------------------------ */

function parseRoute(path) {
  const parts = path.replace(/\/+$/, "").split("/").filter(Boolean); // ["app","p",ws,"a",run]
  if (parts[0] !== "app") return { name: "home", path: "/app" };
  if (parts[1] === "p" && parts[2]) {
    const ws = decodeURIComponent(parts[2]);
    if (parts[3] === "a" && parts[4]) return { name: "agent", ws, run: decodeURIComponent(parts[4]), path };
    return { name: "project", ws, path };
  }
  return { name: "home", path: "/app" };
}
const projectPath = (ws) => `/app/p/${encodeURIComponent(ws)}`;
const agentPath = (ws, run) => `${projectPath(ws)}/a/${encodeURIComponent(run)}`;

let depth = history.state?.depth || 0;
function navigate(path, { replace = false } = {}) {
  if (path === location.pathname && !replace) return;
  if (replace) history.replaceState({ depth }, "", path);
  else { depth += 1; history.pushState({ depth }, "", path); }
  mountScreen("forward");
}
window.addEventListener("popstate", (e) => {
  const d = e.state?.depth ?? 0;
  const dir = d < depth ? "back" : "forward";
  depth = d;
  mountScreen(dir);
});
function goBack(fallback) {
  if (depth > 0) history.back();
  else navigate(fallback, { replace: true });
}

document.addEventListener("click", (e) => {
  const a = e.target.closest("a");
  if (!a) return;
  if (a.hasAttribute("data-link")) {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
    e.preventDefault();
    navigate(a.getAttribute("href"));
    return;
  }
  const href = a.getAttribute("href") || "";
  const external = a.target === "_blank" || /^https?:/i.test(href) && new URL(href, location.href).origin !== location.origin;
  if (external && NATIVE) { e.preventDefault(); openExternal(a.href); }
  else if (external && a.target !== "_blank") { a.target = "_blank"; a.rel = "noopener noreferrer"; }
});

/* ------------------------------------------------------------------ */
/* Screen mounting                                                     */
/* ------------------------------------------------------------------ */

const viewEl = document.getElementById("view");
let screen = null;

function mountScreen(dir = "forward") {
  if (screen) { screen.destroy?.(); screen = null; }
  viewEl.textContent = "";
  if (store.signedOut) { screen = SignInScreen(); }
  else {
    const route = parseRoute(location.pathname);
    ls.set("rudder.lastRoute", { path: route.path, at: Date.now() });
    if (route.name === "project") screen = ProjectScreen(route.ws);
    else if (route.name === "agent") screen = AgentScreen(route.ws, route.run);
    else screen = HomeScreen();
  }
  if (dir === "back") screen.el.classList.add("back-nav");
  viewEl.appendChild(screen.el);
  screen.update?.();
}
function updateScreen() {
  if (store.signedOut && !(screen?.isSignIn)) return mountScreen();
  if (!store.signedOut && screen?.isSignIn) return mountScreen();
  screen?.update?.();
}

/* Shared pieces ------------------------------------------------------ */

function topbarScrollShadow(scroller, bar) {
  scroller.addEventListener("scroll", () => bar.classList.toggle("scrolled", scroller.scrollTop > 2), { passive: true });
}

function pullToRefresh(scroller, onRefresh) {
  const ptr = scroller.querySelector(".ptr");
  if (!ptr || !coarse) return;
  let startY = null, dist = 0, busy = false;
  scroller.addEventListener("touchstart", (e) => {
    if (busy || scroller.scrollTop > 0) { startY = null; return; }
    startY = e.touches[0].clientY; dist = 0;
  }, { passive: true });
  scroller.addEventListener("touchmove", (e) => {
    if (startY == null) return;
    dist = Math.max(0, e.touches[0].clientY - startY);
    if (scroller.scrollTop > 0) dist = 0;
    ptr.style.transition = "none";
    ptr.style.height = `${Math.min(dist * 0.45, 64)}px`;
    ptr.classList.toggle("armed", dist * 0.45 > 52);
  }, { passive: true });
  scroller.addEventListener("touchend", async () => {
    if (startY == null) return;
    startY = null;
    ptr.style.transition = "";
    if (dist * 0.45 > 52) {
      busy = true;
      ptr.classList.remove("armed"); ptr.classList.add("loading");
      ptr.style.height = "44px";
      try { await onRefresh(); } finally {
        busy = false; ptr.classList.remove("loading"); ptr.style.height = "0px";
      }
    } else ptr.style.height = "0px";
  });
}

function refreshButton(onRefresh) {
  const b = h(`<button type="button" class="icon-btn" aria-label="Refresh">${icon("refresh")}</button>`);
  b.addEventListener("click", async () => {
    b.classList.add("spinning");
    const t0 = Date.now();
    try { await onRefresh(); } finally {
      setTimeout(() => b.classList.remove("spinning"), Math.max(0, 500 - (Date.now() - t0)));
    }
  });
  return b;
}

function createComposer({ placeholder, draftKey, onSubmit, hint }) {
  const el = h(`<div class="composer"><form autocomplete="off">
      <textarea rows="1" enterkeyhint="${coarse ? "enter" : "send"}" aria-label="${escapeHtml(placeholder)}" placeholder="${escapeHtml(placeholder)}"></textarea>
      <button type="submit" class="send" aria-label="Send" disabled>${icon("send")}</button>
    </form>${hint ? `<div class="c-hint">${escapeHtml(hint)}</div>` : ""}</div>`);
  const form = el.querySelector("form");
  const ta = el.querySelector("textarea");
  const btn = el.querySelector(".send");
  ta.value = store.drafts[draftKey] || "";
  const grow = () => {
    ta.style.height = "auto";
    ta.style.height = `${Math.min(ta.scrollHeight, 168)}px`;
    btn.disabled = !ta.value.trim();
  };
  const saveDraft = () => {
    if (ta.value) store.drafts[draftKey] = ta.value; else delete store.drafts[draftKey];
    ls.set("rudder.drafts", store.drafts);
  };
  let busy = false;
  const submit = async () => {
    const text = ta.value.trim();
    if (!text || busy) return;
    busy = true; btn.disabled = true;
    const ok = await onSubmit(text);
    busy = false;
    if (ok !== false) { ta.value = ""; saveDraft(); }
    grow();
  };
  ta.addEventListener("input", () => { grow(); saveDraft(); });
  ta.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" || e.isComposing) return;
    if (e.metaKey || e.ctrlKey || (!coarse && !e.shiftKey)) { e.preventDefault(); submit(); }
  });
  form.addEventListener("submit", (e) => { e.preventDefault(); submit(); });
  requestAnimationFrame(grow);
  return { el, textarea: ta };
}

function agentRow({ showProject = false } = {}) {
  return (item) => {
    const el = h(`<a class="row agent tap" data-link>
      <span class="a-dot"><span class="dot"></span></span>
      <div class="a-body">
        <div class="a-top"><span class="a-title"></span><span class="a-time"></span></div>
        <div class="a-meta"><span class="status-text"></span><span class="chip c-proj"></span><span class="chip c-model"></span><span class="chip mono c-phase"></span></div>
        <div class="a-msg"></div>
      </div></a>`);
    el._showProject = showProject;
    enableSwipeDelete(el);
    return el;
  };
}

// Swipe a row left to delete its agent (the same delete as dd on the
// dashboard, which takes it off every screen). Past the threshold the red edge
// says Delete; letting go there asks first, anywhere else snaps back.
const SWIPE_DELETE_PX = 90;
function enableSwipeDelete(el) {
  let x0 = 0, y0 = 0, dx = 0, tracking = false, swiping = false;
  el.addEventListener("touchstart", (e) => {
    if (e.touches.length !== 1 || !el._target || el._target.a.pending) return;
    x0 = e.touches[0].clientX; y0 = e.touches[0].clientY; dx = 0; tracking = true; swiping = false;
  }, { passive: true });
  el.addEventListener("touchmove", (e) => {
    if (!tracking) return;
    const mx = e.touches[0].clientX - x0, my = e.touches[0].clientY - y0;
    if (!swiping) {
      if (Math.abs(mx) < 10 && Math.abs(my) < 10) return;
      if (Math.abs(my) > Math.abs(mx) || mx > 0) { tracking = false; return; }
      swiping = true; el.classList.add("swiping");
    }
    dx = Math.max(-150, Math.min(0, mx));
    el.style.transform = `translateX(${dx}px)`;
    el.classList.toggle("swipe-armed", dx <= -SWIPE_DELETE_PX);
  }, { passive: true });
  const end = () => {
    tracking = false;
    if (!swiping) return;
    swiping = false;
    const armed = dx <= -SWIPE_DELETE_PX;
    el._justSwiped = true;
    setTimeout(() => { el._justSwiped = false; }, 400);
    el.classList.remove("swiping", "swipe-armed");
    el.style.transform = "";
    if (armed && el._target) deleteAgent(el._target.p.workspaceId, el._target.a);
  };
  el.addEventListener("touchend", end);
  el.addEventListener("touchcancel", end);
  // The touch that swiped must not also open the agent.
  el.addEventListener("click", (e) => {
    if (el._justSwiped || el._target?.a.pending) { e.preventDefault(); e.stopPropagation(); }
  }, true);
}

async function deleteAgent(ws, a, { after } = {}) {
  const live = a.status === "working" || a.status === "waiting";
  const ok = await confirmDialog({
    title: "Delete this agent?",
    body: `"${a.title || "This agent"}"${live ? " stops, and its" : "'s"} conversation and workspace are deleted from your cloud machine and every screen. This cannot be undone.`,
    confirm: "Delete",
    danger: true,
  });
  if (!ok) return false;
  try {
    await api("/api/app/action", { method: "POST", body: { workspace: ws, run: a.id, action: "delete" } });
    // Gone now: the dashboard deleted its record, so the next state agrees.
    const p = findProject(ws);
    if (p?.agents) p.agents = p.agents.filter((x) => x.id !== a.id);
    toast("Agent deleted");
    after?.();
    updateScreen();
    refreshState();
    return true;
  } catch (err) {
    if (err.status !== 401) toast(err.message, { error: true });
    return false;
  }
}

function updateAgentRow(el, { p, a }) {
  const st = agentStatus(a);
  el._target = { p, a };
  if (a.pending) el.removeAttribute("href"); else el.href = agentPath(p.workspaceId, a.id);
  el.className = `row agent tap ${st.cls}`;
  el.querySelector(".dot").classList.toggle("live", !!st.live);
  setText(el.querySelector(".a-title"), a.title || a.task || "Untitled agent");
  setText(el.querySelector(".a-time"), rel(a.updatedAt));
  setText(el.querySelector(".status-text"), st.label);
  const proj = el.querySelector(".c-proj");
  proj.hidden = !el._showProject; setText(proj, p.name);
  const model = el.querySelector(".c-model");
  setText(model, backendLabel(a)); model.hidden = el._showProject || !backendLabel(a);
  const phase = el.querySelector(".c-phase");
  const phaseText = a.nodeId ? a.nodeId : (a.phase && a.phase !== a.status ? a.phase : "");
  setText(phase, phaseText); phase.hidden = !phaseText || el._showProject;
  const msg = (a.lastMessage || a.summary || a.task || "").replace(/\s+/g, " ").trim();
  const m = el.querySelector(".a-msg");
  setText(m, msg); m.hidden = !msg;
  el.setAttribute("aria-label", `${a.title || "Agent"}, ${st.label}, ${p.name}`);
}

function machineLabel(home) {
  if (home.status === "running" && !home.connected) return { label: "Connecting", cls: "s-needs", live: true };
  return ({
    running: { label: "Running", cls: "s-done" },
    queued: { label: "Starting", cls: "s-accent", live: true },
    stopped: { label: "Waking", cls: "s-accent", live: true },
    failed: { label: "Failed", cls: "s-failed" },
    deleting: { label: "Deleting", cls: "s-idle" },
  })[home.status] || { label: home.status, cls: "s-idle" };
}

function emptyState({ ic, title, body, action }) {
  const el = h(`<div class="empty"><div class="e-icon">${icon(ic)}</div><h3></h3><p></p></div>`);
  setText(el.querySelector("h3"), title);
  el.querySelector("p").innerHTML = body; // trusted static copy only
  if (action) el.appendChild(action);
  return el;
}

/* ------------------------------------------------------------------ */
/* Sign in                                                             */
/* ------------------------------------------------------------------ */

function SignInScreen() {
  const el = h(`<section class="screen"><div class="signin">
    <img class="logo" src="/app/icon.svg" alt="">
    <h1>Rudder</h1>
    <p>Your coding agents run on a cloud machine, around the clock, laptop open or not. Sign in to steer them from anywhere.</p>
    <div class="actions"></div></div></section>`);
  const actions = el.querySelector(".actions");
  // A paired device needs no OAuth: `rudder cloud app` on the laptop prints a
  // one-time code that signs this browser in to the same account.
  const pair = h(`<form class="pair" autocomplete="off">
      <label for="pair-code">Code from <code>rudder cloud app</code></label>
      <div class="pair-row"><input id="pair-code" inputmode="text" autocapitalize="characters" spellcheck="false"
        maxlength="9" placeholder="ABCD-EFGH" aria-label="Pairing code"><button class="btn primary" type="submit">Pair</button></div>
      <p class="pair-error" role="alert" hidden></p></form>`);
  pair.addEventListener("submit", (event) => {
    event.preventDefault();
    redeemPairCode(pair.querySelector("input").value, pair.querySelector(".pair-error"));
  });
  if (NATIVE) {
    const b = h(`<button type="button" class="btn primary block">Sign in</button>`);
    b.addEventListener("click", () => postNative({ type: "signIn" }));
    actions.appendChild(b);
  } else {
    actions.appendChild(pair);
    const g = h(`<button type="button" class="btn primary block">${icon("google")}<span>Continue with Google</span></button>`);
    g.addEventListener("click", () => { location.href = "/app/login"; });
    const gh = h(`<div class="alt"><button type="button">Use GitHub instead</button></div>`);
    gh.querySelector("button").addEventListener("click", () => { location.href = "/app/login?provider=github"; });
    actions.append(g, gh);
  }
  return { el, isSignIn: true };
}

async function redeemPairCode(code, errorEl) {
  if (errorEl) errorEl.hidden = true;
  try {
    const res = await fetch("/api/app/pair/finish", {
      method: "POST", credentials: "same-origin", cache: "no-store",
      headers: { "Content-Type": "application/json" }, body: JSON.stringify({ code }),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok || !data.token) throw new Error(data.error || "That code didn't work.");
    ls.set("rudder.token", data.token);
    store.signedOut = false;
    history.replaceState({ depth: 0 }, "", "/app");
    await refreshState();
    mountScreen();
  } catch (error) {
    if (errorEl) { errorEl.textContent = error.message; errorEl.hidden = false; }
    else toast(error.message);
  }
}

/* ------------------------------------------------------------------ */
/* Home: projects                                                      */
/* ------------------------------------------------------------------ */

function HomeScreen() {
  const el = h(`<section class="screen">
    <header class="topbar">
      <div class="brand"><img src="/app/icon.svg" alt=""><span>Rudder</span></div>
      <span class="tb-actions" style="display:flex;align-items:center"></span>
    </header>
    <div class="scroller">
      <div class="ptr">${icon("down")}</div>
      <div class="section projects-sec">
        <div class="section-head"><h2>Projects</h2><span class="count"></span></div>
        <div class="projects-body"><div class="list projects"></div></div>
        <div class="add-slot" style="margin-top:10px"></div>
      </div>
      <div class="section needs-sec" hidden>
        <div class="section-head"><h2>Needs you</h2><span class="count"></span></div>
        <div class="list grouped card needs"></div>
      </div>
      <div class="section machine-slot"></div>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  const acts = el.querySelector(".tb-actions");
  acts.appendChild(refreshButton(refreshState));
  const avatarBtn = h(`<button type="button" class="icon-btn" aria-label="Settings"><span class="avatar"></span></button>`);
  avatarBtn.addEventListener("click", openSettings);
  acts.appendChild(avatarBtn);
  topbarScrollShadow(scroller, bar);
  pullToRefresh(scroller, refreshState);

  const machineSlot = el.querySelector(".machine-slot");
  const needsSec = el.querySelector(".needs-sec");
  const needsListEl = el.querySelector(".needs");
  const projectsSec = el.querySelector(".projects-sec");
  const projectsEl = el.querySelector(".projects");
  const projectsBody = el.querySelector(".projects-body");
  const addSlot = el.querySelector(".add-slot");

  // Machine card (built once)
  const machine = h(`<div class="card machine">
      <div class="m-icon">${icon("server")}</div>
      <div class="m-body"><div class="m-title"><span>Cloud machine</span><span class="pill"><span class="dot"></span><span class="pl"></span></span></div>
      <div class="m-sub"></div></div>
      <button type="button" class="btn small accent wake" hidden>${icon("power")}<span>Wake</span></button>
    </div>`);
  const wakeBtn = machine.querySelector(".wake");
  wakeBtn.addEventListener("click", async () => {
    wakeBtn.disabled = true;
    try {
      const res = await api("/api/app/wake", { method: "POST", body: {} });
      if (res?.home && store.state) store.state.home = res.home;
      toast("Waking your cloud machine. This can take up to a minute.");
      update();
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { wakeBtn.disabled = false; }
  });

  // Add project: a sheet listing the GitHub repos the home machine can open,
  // plus "New repository".
  const ghost = h(`<button type="button" class="ghost-add">${icon("plus")}<span>Add project</span></button>`);
  ghost.addEventListener("click", () => openAddProject());
  function renderAdd() {
    if (addSlot.firstElementChild !== ghost) addSlot.replaceChildren(ghost);
  }

  let emptyEl = null;
  function update() {
    const s = store.state;
    setText(avatarBtn.querySelector(".avatar"), (s?.email || "?").slice(0, 1));
    if (!s) {
      if (!machineSlot.firstElementChild) {
        machineSlot.innerHTML = `<div class="sk sk-card" style="height:66px;margin-top:6px"></div>`;
        projectsEl.innerHTML = `<div class="sk sk-card"></div><div class="sk sk-card"></div><div class="sk sk-card" style="opacity:.6"></div>`;
      }
      if (store.stateError && !projectsEl.dataset.err) {
        projectsEl.dataset.err = "1";
        projectsEl.replaceChildren(emptyState({ ic: "alert", title: "Can't reach Rudder", body: escapeHtml(store.stateError) }));
      }
      return;
    }
    delete projectsEl.dataset.err;
    projectsEl.querySelectorAll(".sk, .empty").forEach((n) => n.remove());

    // Machine
    const home = s.home;
    if (home) {
      if (machineSlot.firstElementChild !== machine) machineSlot.replaceChildren(machine);
      const ml = machineLabel(home);
      const pill = machine.querySelector(".pill");
      pill.className = `pill ${ml.cls}`;
      pill.querySelector(".dot").classList.toggle("live", !!ml.live);
      setText(pill.querySelector(".pl"), ml.label);
      const sub = machine.querySelector(".m-sub");
      let subText;
      if (home.status === "failed") subText = home.failureReason || "The machine failed to start.";
      else if (home.status === "stopped") subText = "Was asleep to save cost. Waking it now; agents reopen in about a minute.";
      else if (home.status === "queued") subText = "Booting. This can take up to a minute.";
      else if (home.status === "deleting") subText = "Being removed.";
      else {
        const bits = [];
        if (home.tailnet?.dnsName) bits.push(home.tailnet.dnsName);
        else if (home.tailnet?.hostname) bits.push(home.tailnet.hostname);
        bits.push(`${home.activeAgents || 0} active ${home.activeAgents === 1 ? "agent" : "agents"}`);
        // It is not the user's computer: say where it runs and why that matters.
        subText = `Runs in the cloud, so agents keep going with your laptop closed · ${bits.join(" · ")}`;
      }
      // A login the cloud machine can't use stops agents cold ("Login expired"):
      // say which, and the fix, before anyone starts a task.
      const creds = home.credentials || {};
      const broken = home.status === "running"
        ? ["claude", "codex", "github"].filter((tool) => creds[tool] && creds[tool].ok === false)
        : [];
      const credNames = { claude: "Claude", codex: "Codex", github: "GitHub" };
      if (broken.length) {
        subText = `${broken.map((tool) => credNames[tool]).join(", ")} can't sign in on this machine (${creds[broken[0]].detail || "not logged in"}). Run \`rudder cloud doctor\` on your laptop for the fix.`;
      }
      setText(sub, subText);
      const trouble = home.status === "failed" || broken.length > 0;
      sub.classList.toggle("err", trouble);
      // Projects lead; the machine card only comes up above them while it is
      // the reason nothing works.
      const anchor = trouble ? projectsSec : null;
      if (machineSlot.nextElementSibling !== anchor) scroller.insertBefore(machineSlot, anchor);
      wakeBtn.hidden = !(home.status === "stopped" || home.status === "failed");
      setText(wakeBtn.querySelector("span"), home.status === "failed" ? "Retry" : "Wake");
    } else {
      machineSlot.replaceChildren();
    }

    // Needs you
    const needs = needsList();
    needsSec.hidden = needs.length === 0;
    setText(needsSec.querySelector(".count"), needs.length ? String(needs.length) : "");
    reconcile(needsListEl, needs, (x) => `${x.p.workspaceId}|${x.a.id}`, agentRow({ showProject: true }), updateAgentRow);

    // Projects: pinned ones first, in the order they were pinned, then the
    // rest by last activity.
    const projects = (s.projects || []).slice().sort((a, b) => {
      const pa = pinnedAt(a), pb = pinnedAt(b);
      if (!!pa !== !!pb) return pa ? -1 : 1;
      if (pa) return pa < pb ? -1 : pa > pb ? 1 : 0;
      return (toMs(b.lastActivityAt) || 0) - (toMs(a.lastActivityAt) || 0);
    });
    setText(projectsSec.querySelector(".count"), projects.length ? String(projects.length) : "");
    reconcile(projectsEl, projects, (p) => p.workspaceId, createProjectCard, updateProjectCard);

    if (!projects.length) {
      const body = home
        ? "Add a GitHub repo to clone it onto your cloud machine, then start agents on it from here."
        : "No cloud machine yet. Run <code>rudder cloud</code> in a repo on your laptop, or add a project.";
      const key = home ? "h" : "n";
      if (!emptyEl || emptyEl.dataset.k !== key) {
        emptyEl = emptyState({ ic: home ? "folder" : "server", title: home ? "No projects yet" : "Welcome to Rudder", body });
        emptyEl.dataset.k = key;
      }
      if (emptyEl.parentNode !== projectsBody) projectsBody.appendChild(emptyEl);
    } else if (emptyEl) { emptyEl.remove(); emptyEl = null; }
    renderAdd();
  }

  return { el, update };
}

// A pin tapped on this phone wins until the server's state agrees: a poll
// already in flight would otherwise flip the card back for a moment.
const pinOverrides = new Map();
function pinnedAt(p) {
  if (pinOverrides.has(p.workspaceId)) {
    const want = pinOverrides.get(p.workspaceId);
    if (!!p.pinnedAt !== !!want) return want;
    pinOverrides.delete(p.workspaceId);
  }
  return p.pinnedAt || null;
}
async function setPinned(ws, pinned) {
  const p = findProject(ws);
  if (!p) return;
  pinOverrides.set(ws, pinned ? pinnedAt(p) || new Date().toISOString() : null);
  updateScreen();
  try {
    const res = await api("/api/app/pin", { method: "POST", body: { workspace: ws, pinned } });
    const cur = findProject(ws);
    if (cur) cur.pinnedAt = res?.pinnedAt ?? null;
  } catch (err) {
    pinOverrides.delete(ws);
    if (err.status !== 401) toast(err.message, { error: true });
  }
  updateScreen();
}

function createProjectCard() {
  const el = h(`<a class="card project tap" data-link>
    <div class="p-top"><span class="p-name"></span><span class="pill p-dash"></span><span class="dot live s-working p-live"></span>
      <button type="button" class="p-pin" aria-pressed="false">${icon("pin")}</button></div>
    <div class="p-repo"></div>
    <div class="p-stats">
      <span class="stat st-w s-working"><span class="dot"></span><b></b> working</span>
      <span class="stat st-n s-needs"><span class="dot"></span><b></b> need you</span>
      <span class="stat st-d s-done"><span class="dot"></span><b></b> done</span>
      <span class="p-time"></span>
    </div>
    <div class="p-err"></div></a>`);
  el.querySelector(".p-pin").addEventListener("click", (e) => {
    // The card is a link; the pin must not open the project.
    e.preventDefault();
    e.stopPropagation();
    setPinned(el.dataset.key, !el.classList.contains("pinned"));
  });
  return el;
}
const DASH = {
  starting: { label: "Starting", cls: "s-accent" }, cloning: { label: "Cloning", cls: "s-accent" },
  stopped: { label: "Stopped", cls: "s-idle" }, failed: { label: "Failed", cls: "s-failed" },
};
function updateProjectCard(el, p) {
  el.href = projectPath(p.workspaceId);
  const agents = p.agents || [];
  const w = agents.filter((a) => a.status === "working").length;
  const n = agents.filter(needsYou).length;
  const d = agents.filter((a) => a.status === "done" || a.status === "merged").length;
  setText(el.querySelector(".p-name"), p.name || p.repo || "Project");
  setText(el.querySelector(".p-repo"), [p.repo, p.ref].filter(Boolean).join(" @ "));
  for (const [sel, v] of [[".st-w", w], [".st-n", n], [".st-d", d]]) {
    const s = el.querySelector(sel);
    setText(s.querySelector("b"), String(v));
    s.classList.toggle("zero", v === 0);
  }
  setText(el.querySelector(".st-n").lastChild, n === 1 ? " needs you" : " need you");
  el.querySelector(".p-live").hidden = w === 0;
  const dash = DASH[p.dashboard] || (p.status === "failed" ? DASH.failed : null);
  const dp = el.querySelector(".p-dash");
  dp.hidden = !dash;
  if (dash) { dp.className = `pill p-dash ${dash.cls}`; setText(dp, dash.label); }
  setText(el.querySelector(".p-time"), rel(p.lastActivityAt));
  const pinned = !!pinnedAt(p);
  el.classList.toggle("pinned", pinned);
  const pin = el.querySelector(".p-pin");
  pin.setAttribute("aria-pressed", String(pinned));
  pin.setAttribute("aria-label", pinned ? `Unpin ${p.name || "project"}` : `Pin ${p.name || "project"} to the top`);
  const err = el.querySelector(".p-err");
  setText(err, p.failureReason || ""); err.hidden = !p.failureReason;
  el.setAttribute("aria-label", `${p.name}, ${w} working, ${n} need you, ${d} done`);
}

/* ------------------------------------------------------------------ */
/* Project: agents                                                     */
/* ------------------------------------------------------------------ */

const GROUPS = [
  { id: "working", label: "Working" },
  { id: "needs", label: "Needs you" },
  { id: "done", label: "Done" },
  { id: "earlier", label: "Earlier" },
];
function groupOf(a) {
  if (pendingAsks(a).length) return "needs";
  if (a.status === "working" || a.status === "starting") return "working";
  if (needsYou(a)) return "needs";
  if (a.status === "done" && Date.now() - (a.updatedAt || 0) < 36 * 3600 * 1000) return "done";
  return "earlier";
}

function ProjectScreen(ws) {
  const el = h(`<section class="screen">
    <header class="topbar">
      <a class="icon-btn back" href="/app" aria-label="Back to projects">${icon("back")}</a>
      <div class="titles"><div class="title"></div><div class="subtitle"><span class="repo"></span></div></div>
    </header>
    <div class="scroller has-composer">
      <div class="ptr">${icon("down")}</div>
      <div class="status-slot"></div>
      <div class="section ports-sec" hidden><div class="section-head"><h2>Previews</h2></div><div class="ports"></div></div>
      <div class="groups"></div>
      <div class="p-empty"></div>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  el.querySelector(".back").addEventListener("click", (e) => { e.preventDefault(); goBack("/app"); });
  bar.appendChild(refreshButton(refreshState));
  topbarScrollShadow(scroller, bar);
  pullToRefresh(scroller, refreshState);

  const composer = createComposer({
    placeholder: "Start a new task, or /model",
    draftKey: `task:${ws}`,
    hint: coarse ? "" : "Enter to start · Shift+Enter for a new line",
    onSubmit: async (text) => {
      if (/^\/model\s*$/i.test(text)) {
        openModelPicker(ws);
        return true;
      }
      try {
        const res = await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
        toast(text.startsWith("/") ? "Sent to the dashboard" : "Task started");
        await refreshState();
        const run = res?.run || res?.runId;
        if (run) navigate(agentPath(ws, run));
        return true;
      } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); return false; }
    },
  });
  el.appendChild(composer.el);

  const groupsEl = el.querySelector(".groups");
  const portsSec = el.querySelector(".ports-sec");
  const portsEl = el.querySelector(".ports");
  const emptySlot = el.querySelector(".p-empty");
  const statusSlot = el.querySelector(".status-slot");

  function update() {
    const s = store.state;
    const p = findProject(ws);
    if (!s) {
      setText(el.querySelector(".title"), " ");
      if (!groupsEl.firstElementChild && !emptySlot.firstElementChild) {
        emptySlot.innerHTML = `<div class="section"><div class="sk sk-line" style="width:30%"></div></div><div class="sk sk-card" style="height:84px"></div><div class="sk sk-card" style="height:84px"></div><div class="sk sk-card" style="height:84px;opacity:.6"></div>`;
      }
      return;
    }
    if (!p) {
      setText(el.querySelector(".title"), "Project");
      groupsEl.replaceChildren();
      if (!emptySlot.querySelector(".empty")) {
        const back = h(`<a class="btn" href="/app" data-link>All projects</a>`);
        emptySlot.replaceChildren(emptyState({ ic: "folder", title: "Project not found", body: "It may still be cloning onto your cloud machine, or it was removed." , action: back }));
      }
      composer.el.hidden = true;
      return;
    }
    composer.el.hidden = false;
    setText(el.querySelector(".title"), p.name || p.repo);
    setText(el.querySelector(".repo"), [p.repo, p.ref].filter(Boolean).join(" @ "));

    // Dashboard status banner
    const dash = DASH[p.dashboard];
    if (dash || p.failureReason) {
      let banner = statusSlot.firstElementChild;
      if (!banner) { banner = h(`<div class="card machine"><div class="m-icon">${icon("server")}</div><div class="m-body"><div class="m-title"><span class="pill"><span class="dot"></span><span class="pl"></span></span></div><div class="m-sub"></div></div></div>`); statusSlot.appendChild(banner); }
      const pill = banner.querySelector(".pill");
      const d = dash || DASH.failed;
      pill.className = `pill ${d.cls}`;
      pill.querySelector(".dot").classList.toggle("live", p.dashboard === "starting" || p.dashboard === "cloning");
      setText(pill.querySelector(".pl"), d.label);
      const sub = banner.querySelector(".m-sub");
      setText(sub, p.failureReason || (p.dashboard === "cloning" ? `Cloning ${p.repo} onto your cloud machine.` : p.dashboard === "starting" ? "Starting the Rudder dashboard for this project." : "The Rudder dashboard for this project is not running."));
      sub.classList.toggle("err", !!p.failureReason);
    } else statusSlot.replaceChildren();

    // Ports
    const ports = p.ports || [];
    portsSec.hidden = !ports.length;
    reconcile(portsEl, ports, (x) => x.port,
      () => h(`<a class="port" target="_blank" rel="noopener noreferrer">${icon("globe")}<span class="pt"></span><small></small>${icon("external")}</a>`),
      (a, x) => {
        a.href = x.url;
        setText(a.querySelector(".pt"), x.label || "Preview");
        setText(a.querySelector("small"), `:${x.port}`);
        a.setAttribute("aria-label", `Open preview ${x.label || ""} on port ${x.port}`);
      });

    // Groups
    const items = (p.agents || []).map((a) => ({ p, a }));
    const groups = GROUPS.map((g) => ({
      ...g,
      rows: items.filter((x) => groupOf(x.a) === g.id).sort((x, y) => (y.a.updatedAt || 0) - (x.a.updatedAt || 0)),
    })).filter((g) => g.rows.length);
    emptySlot.querySelectorAll(".sk").forEach((n) => n.remove());
    reconcile(groupsEl, groups, (g) => g.id,
      () => h(`<div class="section"><div class="section-head"><h2></h2><span class="count"></span></div><div class="list grouped card"></div></div>`),
      (sec, g) => {
        setText(sec.querySelector("h2"), g.label);
        setText(sec.querySelector(".count"), String(g.rows.length));
        reconcile(sec.querySelector(".list"), g.rows, (x) => x.a.id, agentRow(), updateAgentRow);
      });
    if (!groups.length) {
      if (!emptySlot.querySelector(".empty")) {
        emptySlot.replaceChildren(emptyState({ ic: "terminal", title: "No agents yet", body: "Describe a task below and Rudder starts an agent on this project." }));
      }
    } else emptySlot.replaceChildren();
  }
  return { el, update };
}

/* ------------------------------------------------------------------ */
/* Agent: conversation                                                 */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/* Ask card: answer an agent's permission prompt, question or plan     */
/* ------------------------------------------------------------------ */

const ASK_DIFF_LINES = 40;
const ASK_WRITE_LINES = 20;

// `onAnswer(answer)` resolves true when the answer was delivered. The card
// keeps its own selections, so it is built once per ask and never repainted.
function createAskCard(ask, { onAnswer }) {
  const input = ask.input && typeof ask.input === "object" ? ask.input : {};
  const tool = ask.tool || "Tool";
  const kind = ask.kind || "permission";
  const title = kind === "question" ? "Question" : kind === "plan" ? "Plan ready for review" : `Allow ${tool}?`;
  const el = h(`<section class="ask-card" role="region" aria-live="polite">
    <div class="ask-head"><span class="ask-icon"></span><span class="ask-title"></span><span class="ask-count"></span></div>
    <div class="ask-body"></div>
    <div class="ask-actions"></div></section>`);
  el.setAttribute("aria-label", kind === "question" ? "The agent has a question" : kind === "plan" ? "The agent's plan" : `Permission request: ${tool}`);
  el.querySelector(".ask-icon").innerHTML = icon(kind === "question" ? "inbox" : kind === "plan" ? "task" : toolIcon(tool));
  setText(el.querySelector(".ask-title"), title);
  const body = el.querySelector(".ask-body");
  const actions = el.querySelector(".ask-actions");

  let busy = false;
  const setBusy = (on) => {
    busy = on;
    el.classList.toggle("busy", on);
    for (const b of el.querySelectorAll("button, input, textarea")) {
      if (on) { b.dataset.wasDisabled = b.disabled ? "1" : ""; b.disabled = true; }
      else { b.disabled = b.dataset.wasDisabled === "1"; delete b.dataset.wasDisabled; }
    }
  };
  const answer = async (value) => {
    if (busy) return;
    setBusy(true);
    const ok = await onAnswer(value);
    if (!ok) { setBusy(false); refreshValidity(); }
  };
  let refreshValidity = () => {};

  const btn = (label, cls, fn, aria) => {
    const b = h(`<button type="button" class="btn small ${cls}"></button>`);
    b.textContent = label;
    if (aria) b.setAttribute("aria-label", aria);
    b.addEventListener("click", fn);
    return b;
  };
  const pre = (text, cls = "") => {
    const x = h(`<pre class="ask-pre ${cls}"></pre>`);
    x.textContent = text;
    return x;
  };
  const note = (text, cls = "ask-desc") => {
    const x = h(`<div class="${cls}"></div>`);
    x.textContent = text;
    return x;
  };
  // A reason box behind a button ("Deny" -> optional reason, "Revise" -> what to change).
  const reasonFlow = ({ openLabel, openCls, placeholder, sendLabel, required, build }) => {
    const open = btn(openLabel, openCls, () => {
      actions.hidden = true;
      box.hidden = false;
      ta.focus();
    });
    const box = h(`<div class="ask-reason" hidden><textarea rows="3"></textarea><div class="ask-actions"></div></div>`);
    const ta = box.querySelector("textarea");
    ta.placeholder = placeholder;
    ta.setAttribute("aria-label", placeholder);
    const send = btn(sendLabel, openCls === "danger" ? "danger-fill" : "primary", () => answer(build(ta.value.trim())));
    const cancel = btn("Cancel", "", () => { box.hidden = true; actions.hidden = false; open.focus(); });
    const sync = () => { if (!busy) send.disabled = required && !ta.value.trim(); };
    ta.addEventListener("input", sync);
    sync();
    box.querySelector(".ask-actions").append(cancel, send);
    el.appendChild(box);
    return open;
  };

  if (kind === "plan") {
    const md = h(`<div class="ask-plan md"></div>`);
    renderMarkdown(md, String(input.plan || "The agent did not include the plan text."));
    md.dataset.sig = `${markdownReady()}`;
    el._repaintMarkdown = () => {
      if (md.dataset.sig === `${markdownReady()}`) return;
      renderMarkdown(md, String(input.plan || "")); md.dataset.sig = `${markdownReady()}`;
    };
    body.appendChild(md);
    actions.append(
      reasonFlow({
        openLabel: "Revise…", openCls: "", placeholder: "What should change in the plan?", sendLabel: "Send revision",
        required: true, build: (message) => ({ approve: false, message }),
      }),
      btn("Approve", "accent", () => answer({ approve: true }), "Approve the plan"),
    );
    return el;
  }

  if (kind === "question") {
    const questions = Array.isArray(input.questions) ? input.questions : [];
    const safeId = String(ask.id || "ask").replace(/[^a-zA-Z0-9_-]/g, "");
    const states = questions.map((q, i) => {
      const multi = !!q.multiSelect;
      const type = multi ? "checkbox" : "radio";
      const name = `ask-${safeId}-${i}`;
      const field = h(`<fieldset class="ask-q"><legend><span class="ask-q-head"></span><span class="ask-q-text"></span></legend><div class="ask-opts"></div></fieldset>`);
      setText(field.querySelector(".ask-q-head"), q.header || "");
      field.querySelector(".ask-q-head").hidden = !q.header;
      setText(field.querySelector(".ask-q-text"), q.question || "");
      const opts = field.querySelector(".ask-opts");
      for (const o of Array.isArray(q.options) ? q.options : []) {
        const row = h(`<label class="ask-opt"><input type="${type}" name="${name}"><span class="ask-opt-t"><b></b><small></small></span></label>`);
        row.querySelector("input").value = o.label || "";
        setText(row.querySelector("b"), o.label || "");
        const d = row.querySelector("small");
        setText(d, o.description || ""); d.hidden = !o.description;
        opts.appendChild(row);
      }
      const other = h(`<label class="ask-opt ask-opt-other"><input type="${type}" name="${name}" data-other="1"><span class="ask-opt-t"><b>Other…</b></span></label>`);
      const otherText = h(`<input type="text" class="ask-other-text" placeholder="Type something" hidden>`);
      otherText.setAttribute("aria-label", `Your own answer to: ${q.question || "this question"}`);
      opts.append(other, otherText);
      body.appendChild(field);
      const st = { q, multi, field, otherText };
      field.addEventListener("change", () => {
        const otherOn = other.querySelector("input").checked;
        otherText.hidden = !otherOn;
        if (otherOn && document.activeElement !== otherText) otherText.focus();
        refreshValidity();
      });
      otherText.addEventListener("input", () => refreshValidity());
      st.value = () => {
        const checked = [...field.querySelectorAll("input:checked")];
        const labels = checked.filter((x) => !x.dataset.other).map((x) => x.value);
        const own = checked.some((x) => x.dataset.other) ? otherText.value.trim() : "";
        if (multi) { const all = own ? [...labels, own] : labels; return all.length ? all : null; }
        return own || labels[0] || null;
      };
      return st;
    });
    const submit = btn("Submit", "accent", () => {
      const answers = {};
      for (const st of states) answers[st.q.question || ""] = st.value();
      answer({ answers });
    }, "Submit answers");
    refreshValidity = () => { if (!busy) submit.disabled = !states.every((st) => st.value() != null); };
    refreshValidity();
    actions.append(submit);
    return el;
  }

  // Permission
  if (tool === "Bash") {
    if (input.description) body.appendChild(note(String(input.description)));
    body.appendChild(pre(String(input.command || ""), "mono"));
  } else if ((tool === "Edit" || tool === "MultiEdit") && input.file_path) {
    body.appendChild(note(String(input.file_path), "ask-path"));
    const edits = Array.isArray(input.edits) ? input.edits : [{ old_string: input.old_string, new_string: input.new_string }];
    const diff = h(`<pre class="ask-pre ask-diff"></pre>`);
    let shown = 0, extra = 0;
    for (const e of edits) {
      for (const [sign, text] of [["-", e.old_string], ["+", e.new_string]]) {
        for (const line of String(text ?? "").split("\n")) {
          if (shown >= ASK_DIFF_LINES) { extra += 1; continue; }
          const span = document.createElement("span");
          span.className = sign === "-" ? "del" : "add";
          span.textContent = `${sign} ${line}\n`;
          diff.appendChild(span); shown += 1;
        }
      }
    }
    body.appendChild(diff);
    if (extra) body.appendChild(note(`${extra} more lines`, "ask-more"));
  } else if (tool === "Write" && input.file_path) {
    body.appendChild(note(String(input.file_path), "ask-path"));
    const lines = String(input.content ?? "").split("\n");
    body.appendChild(pre(lines.slice(0, ASK_WRITE_LINES).join("\n"), "mono"));
    if (lines.length > ASK_WRITE_LINES) body.appendChild(note(`${lines.length - ASK_WRITE_LINES} more lines`, "ask-more"));
  } else {
    let json = "";
    try { json = JSON.stringify(input, null, 2); } catch { json = String(input); }
    body.appendChild(pre(json.length > 4000 ? `${json.slice(0, 4000)}\n…` : json, "mono"));
  }
  const suggestions = Array.isArray(ask.suggestions) ? ask.suggestions : [];
  actions.append(reasonFlow({
    openLabel: "Deny", openCls: "danger", placeholder: "Tell the agent why, or what to do instead (optional)", sendLabel: "Deny",
    required: false, build: (message) => (message ? { behavior: "deny", message } : { behavior: "deny" }),
  }));
  if (suggestions.length) actions.append(btn("Allow always", "", () => answer({ behavior: "allow", always: true }), `Always allow ${tool}`));
  actions.append(btn("Allow", "accent", () => answer({ behavior: "allow" }), `Allow ${tool}`));
  return el;
}

const OUT_LIMIT = 2400;

function AgentScreen(ws, run) {
  const key = `${ws}|${run}`;
  const el = h(`<section class="screen">
    <header class="topbar">
      <a class="icon-btn back" aria-label="Back to project">${icon("back")}</a>
      <div class="titles"><div class="title"></div>
        <div class="subtitle"><span class="dot"></span><span class="st"></span><span class="sep">·</span><span class="bk"></span></div></div>
      <button type="button" class="icon-btn term-toggle" aria-label="Show the agent's terminal" aria-pressed="false">${icon("terminal")}</button>
      <button type="button" class="icon-btn more" aria-label="Agent actions" aria-haspopup="menu">${icon("more")}</button>
    </header>
    <div class="term-panel" hidden>
      <pre class="term-screen" aria-label="Agent terminal" aria-live="off">Loading the terminal…</pre>
      <div class="key-strip" role="toolbar" aria-label="Keys">
        <button type="button" data-key="esc">Esc</button><button type="button" data-key="up" aria-label="Up">↑</button>
        <button type="button" data-key="down" aria-label="Down">↓</button><button type="button" data-key="left" aria-label="Left">←</button>
        <button type="button" data-key="right" aria-label="Right">→</button><button type="button" data-key="enter">Enter</button>
        <button type="button" data-key="tab">Tab</button><button type="button" data-key="shift-tab" aria-label="Shift Tab">⇧Tab</button>
        <button type="button" data-key="y">y</button><button type="button" data-key="n">n</button>
        <button type="button" data-key="ctrl-c" class="danger">^C</button>
      </div>
    </div>
    <div class="chat-wrap">
      <div class="scroller has-composer"><div class="chat" role="log" aria-label="Conversation"><div class="truncated-note" hidden>Showing the most recent messages</div><div class="msgs"></div><div class="typing" hidden aria-label="Agent is working"><i></i><i></i><i></i></div></div></div>
      <button type="button" class="new-pill">${icon("down")}<span>New messages</span></button>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  const msgsEl = el.querySelector(".msgs");
  const typing = el.querySelector(".typing");
  const pill = el.querySelector(".new-pill");
  const back = el.querySelector(".back");
  back.href = projectPath(ws);
  back.addEventListener("click", (e) => { e.preventDefault(); goBack(projectPath(ws)); });
  topbarScrollShadow(scroller, bar);

  const composer = createComposer({
    placeholder: "Message the agent",
    draftKey: `msg:${key}`,
    hint: coarse ? "" : "Enter to send · Shift+Enter for a new line",
    onSubmit: (text) => { send(text); return true; },
  });
  el.querySelector(".chat-wrap").after(composer.el);

  // Pinned above the composer: the oldest prompt the agent is blocked on.
  const askSlot = h(`<div class="ask-slot"></div>`);
  composer.el.before(askSlot);
  let askCard = null;
  function paintAsk() {
    const asks = pendingAsks(findAgent(ws, run));
    const ask = asks[0];
    if (!ask) { if (askCard) { askCard.remove(); askCard = null; } return; }
    if (askCard?._askId !== ask.id) {
      askCard?.remove();
      askCard = createAskCard(ask, { onAnswer: (answer) => answerAsk(ask, answer) });
      askCard._askId = ask.id;
      askSlot.appendChild(askCard);
    }
    askCard._repaintMarkdown?.();
    setText(askCard.querySelector(".ask-count"), asks.length > 1 ? `1 of ${asks.length}` : "");
  }
  async function answerAsk(ask, answer) {
    try {
      await api("/api/app/answer", { method: "POST", body: { workspace: ws, run, ask: ask.id, answer } });
      answeredAsks.add(ask.id);
      postNative({ type: "haptic" });
      paint();
      refreshState();
      setTimeout(loadTranscript, 800);
      return true;
    } catch (err) {
      if (err.status !== 401) toast(err.message, { error: true });
      return false;
    }
  }

  // Interrupt: Esc into the agent's terminal, like pressing Esc at the
  // dashboard. The agent stays alive (unlike "Stop agent" in the menu).
  const interruptBtn = h(`<button type="button" class="interrupt" aria-label="Interrupt the agent" title="Interrupt" hidden>${icon("stop")}</button>`);
  composer.el.querySelector(".send").before(interruptBtn);
  interruptBtn.addEventListener("click", async () => {
    if (interruptBtn.disabled) return;
    interruptBtn.disabled = true;
    postNative({ type: "haptic" });
    try {
      await api("/api/app/keys", { method: "POST", body: { workspace: ws, run, keys: "esc" } });
      toast("Interrupted");
      setTimeout(() => { loadTranscript(); refreshState(); }, 600);
    } catch (err) {
      if (err.status !== 401) toast(err.message, { error: true });
    } finally {
      interruptBtn.disabled = false;
    }
  });
  const isRunning = (status) => status === "running" || !!statusOf(status).live;
  fastStatePoll = () => {
    const a = findAgent(ws, run);
    return !!a && (isRunning(a.status) || pendingAsks(a).length > 0);
  };
  pollState();

  // `/` commands: the message is typed into the agent's own terminal, so its
  // commands work. Suggest the common ones for this backend.
  const slashBox = h(`<div class="slash-suggest" role="listbox" hidden></div>`);
  composer.el.prepend(slashBox);
  const SLASH = {
    claude: [["/compact", "summarize and free context"], ["/clear", "start a fresh conversation"], ["/model opus", "switch model (or sonnet, haiku)"], ["/effort high", "reasoning effort"], ["/cost", "usage so far"], ["/context", "what fills the context"], ["/review", "review the changes"], ["/init", "write CLAUDE.md"], ["/resume", "pick a past session (use the terminal keys)"], ["/model", "model picker (use the terminal keys)"]],
    codex: [["/compact", "summarize and free context"], ["/new", "start a fresh conversation"], ["/model", "model picker (use the terminal keys)"], ["/status", "session and usage"], ["/review", "review the changes"], ["/diff", "show the diff"], ["/init", "write AGENTS.md"], ["/approvals", "approval mode (use the terminal keys)"]],
  };
  const renderSlash = () => {
    const value = composer.textarea.value;
    const backend = findAgent(ws, run)?.backend === "codex" ? "codex" : "claude";
    if (!value.startsWith("/") || value.includes("\n")) { slashBox.hidden = true; return; }
    const q = value.toLowerCase();
    const hits = SLASH[backend].filter(([cmd]) => cmd.startsWith(q) || (q.length > 1 && cmd.includes(q.slice(1)))).slice(0, 6);
    slashBox.replaceChildren(...hits.map(([cmd, what]) => {
      const b = h(`<button type="button" role="option"><code></code><span></span></button>`);
      b.querySelector("code").textContent = cmd; b.querySelector("span").textContent = what;
      b.addEventListener("click", () => {
        composer.textarea.value = cmd; composer.textarea.dispatchEvent(new Event("input")); composer.textarea.focus();
        slashBox.hidden = true;
        if (what.includes("terminal keys")) setTerminal(true);
      });
      return b;
    }));
    slashBox.hidden = hits.length === 0;
  };
  composer.textarea.addEventListener("input", renderSlash);

  // Live terminal: what the agent's screen shows right now (pickers, prompts),
  // with real keys to drive it.
  const termPanel = el.querySelector(".term-panel");
  const termScreen = el.querySelector(".term-screen");
  const termToggle = el.querySelector(".term-toggle");
  let termOn = false;
  let termTimer = null;
  const pollScreen = async () => {
    termTimer = null;
    if (!termOn) return;
    if (!document.hidden) {
      try {
        const res = await api(`/api/app/screen?workspace=${encodeURIComponent(ws)}&run=${encodeURIComponent(run)}`);
        if (res?.text != null) {
          const lines = res.text.split("\n");
          while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
          termScreen.textContent = lines.join("\n") || " ";
        } else if (termScreen.textContent === "Loading the terminal…") {
          termScreen.textContent = "Waiting for the agent's screen…";
        }
      } catch (err) {
        termScreen.textContent = err.status === 401 ? "Signed out." : err.waking ? "Waking the cloud machine…" : err.message;
      }
    }
    if (termOn) termTimer = setTimeout(pollScreen, 1000);
  };
  function setTerminal(on) {
    termOn = on;
    termPanel.hidden = !on;
    termToggle.setAttribute("aria-pressed", String(on));
    termToggle.classList.toggle("on", on);
    clearTimeout(termTimer);
    if (on) pollScreen();
  }
  termToggle.addEventListener("click", () => setTerminal(!termOn));
  el.querySelector(".key-strip").addEventListener("click", async (e) => {
    const b = e.target.closest("[data-key]");
    if (!b) return;
    postNative({ type: "haptic" });
    try {
      await api("/api/app/keys", { method: "POST", body: { workspace: ws, run, keys: b.dataset.key } });
      clearTimeout(termTimer); setTimeout(pollScreen, 250);
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
  });

  let atBottom = true;
  let firstPaint = true;
  let lastCount = 0;
  const isAtBottom = () => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 72;
  const toBottom = (smooth) => scroller.scrollTo({ top: scroller.scrollHeight, behavior: smooth ? "smooth" : "auto" });
  scroller.addEventListener("scroll", () => {
    atBottom = isAtBottom();
    if (atBottom) pill.classList.remove("show");
  }, { passive: true });
  pill.addEventListener("click", () => { toBottom(true); pill.classList.remove("show"); });
  // Keep the bottom pinned when the keyboard resizes the viewport.
  const ro = new ResizeObserver(() => { if (atBottom) toBottom(false); });
  ro.observe(scroller);

  // Copy buttons + tool expansion (delegated)
  msgsEl.addEventListener("click", async (e) => {
    const copy = e.target.closest(".copy-btn");
    if (copy) {
      const code = copy.closest(".codeblock")?.querySelector("code");
      if (code && await copyText(code.textContent)) {
        copy.innerHTML = `${icon("check")}<span>Copied</span>`;
        setTimeout(() => { copy.innerHTML = `${icon("copy")}<span>Copy</span>`; }, 1600);
      }
      return;
    }
    const stepsRow = e.target.closest(".steps-row");
    if (stepsRow) {
      const node = stepsRow.closest(".steps");
      const k = `${key}|${node.dataset.key}`;
      if (store.expanded.has(k)) store.expanded.delete(k); else store.expanded.add(k);
      paintSteps(node, node._steps);
      return;
    }
    const row = e.target.closest(".tool-row");
    if (row) {
      const box = row.closest(".tool");
      const k = `${key}|${box.dataset.key}`;
      if (store.expanded.has(k)) store.expanded.delete(k); else store.expanded.add(k);
      paintTool(box, box._msg);
      return;
    }
    const more = e.target.closest(".show-more");
    if (more) {
      const box = more.closest(".tool");
      const k = `${key}|${box.dataset.key}`;
      if (store.showMore.has(k)) store.showMore.delete(k); else store.showMore.add(k);
      paintTool(box, box._msg);
      return;
    }
    const retry = e.target.closest(".retry");
    if (retry) {
      const id = retry.closest(".msg").dataset.key;
      const list = store.pending.get(key) || [];
      const item = list.find((x) => x.id === id);
      if (item) { store.pending.set(key, list.filter((x) => x !== item)); send(item.text); }
    }
  });

  // Overflow menu
  const moreBtn = el.querySelector(".more");
  let menuEl = null;
  function closeMenu() { menuEl?.remove(); menuEl = null; moreBtn.setAttribute("aria-expanded", "false"); }
  moreBtn.addEventListener("click", () => {
    if (menuEl) return closeMenu();
    const a = findAgent(ws, run);
    const status = a?.status || store.transcripts.get(key)?.status;
    const canStop = !["stopped", "merged", "failed", "orphaned"].includes(status);
    const canMerge = status === "done" || status === "review";
    menuEl = h(`<div><div class="scrim"></div><div class="menu" role="menu"></div></div>`);
    const menu = menuEl.querySelector(".menu");
    const add = (html, fn) => { const b = h(html); b.addEventListener("click", () => { closeMenu(); fn(); }); menu.appendChild(b); };
    if (canMerge) add(`<button type="button" role="menuitem">${icon("merge")}<span>Merge into main</span></button>`, () => act("merge"));
    if (a?.remoteUrl) add(`<button type="button" role="menuitem">${icon("external")}<span>Open in Claude</span></button>`, () => openExternal(a.remoteUrl));
    add(`<button type="button" role="menuitem">${icon("refresh")}<span>Refresh</span></button>`, () => { loadTranscript(); refreshState(); });
    menu.appendChild(h("<hr>"));
    if (canStop) {
      add(`<button type="button" role="menuitem" class="danger">${icon("stop")}<span>Stop agent</span></button>`, () => act("stop"));
    }
    add(`<button type="button" role="menuitem" class="danger">${icon("trash")}<span>Delete agent</span></button>`,
      () => deleteAgent(ws, a || { id: run, title: store.transcripts.get(key)?.title, status }, { after: () => goBack(projectPath(ws)) }));
    menuEl.querySelector(".scrim").addEventListener("click", closeMenu);
    el.appendChild(menuEl);
    moreBtn.setAttribute("aria-expanded", "true");
    menu.querySelector("button")?.focus();
  });
  el.addEventListener("keydown", (e) => { if (e.key === "Escape" && menuEl) { closeMenu(); moreBtn.focus(); } });

  async function act(action) {
    const a = findAgent(ws, run);
    const name = a?.title || "this agent";
    const ok = await confirmDialog(action === "merge"
      ? { title: "Merge this agent's work?", body: `Rudder merges "${name}" into the project's main checkout.`, confirm: "Merge" }
      : { title: "Stop this agent?", body: `"${name}" stops now. Its workspace is kept, so you can resume later.`, confirm: "Stop", danger: true });
    if (!ok) return;
    try {
      await api("/api/app/action", { method: "POST", body: { workspace: ws, run, action } });
      toast(action === "merge" ? "Merge started" : "Agent stopped");
      refreshState(); loadTranscript();
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
  }

  let seq = 0;
  async function send(text) {
    const item = { id: `pending-${Date.now()}-${seq++}`, text, ts: Date.now(), state: "sending" };
    store.pending.set(key, [...(store.pending.get(key) || []), item]);
    atBottom = true;
    paint();
    toBottom(true);
    try {
      await api("/api/app/send", { method: "POST", body: { workspace: ws, run, text } });
      item.state = "sent";
      setTimeout(loadTranscript, 1200);
    } catch (err) {
      item.state = "failed";
      if (err.status !== 401) toast(err.message, { error: true });
    }
    paint();
  }

  let inflight = false;
  let lastErr = null;
  let waking = false;
  async function loadTranscript() {
    if (inflight) return;
    inflight = true;
    try {
      const t = await api(`/api/app/transcript?workspace=${encodeURIComponent(ws)}&run=${encodeURIComponent(run)}&limit=300`);
      store.transcripts.set(key, t);
      lastErr = null;
      waking = false;
      reconcilePending(t);
    } catch (err) {
      // A sleeping home is being woken by the server: say so in place, no toast.
      if (err.waking) {
        waking = true;
        const prev = store.transcripts.get(key);
        store.transcripts.set(key, { ...(prev || { messages: [] }), waking: true });
        return;
      }
      if (err.status !== 401 && err.message !== lastErr) { toast(err.message, { error: true }); lastErr = err.message; }
      if (!store.transcripts.has(key)) store.transcripts.set(key, { messages: [], error: err.message });
    } finally {
      inflight = false;
      paint();
    }
  }

  function reconcilePending(t) {
    const list = store.pending.get(key);
    if (!list?.length) return;
    const users = (t.messages || []).filter((m) => m.role === "user");
    const kept = list.filter((p) => {
      if (p.state === "failed") return true;
      if (Date.now() - p.ts > 5 * 60 * 1000) return false;
      return !users.some((m) => (m.text || "").trim() === p.text.trim() && (m.ts == null || m.ts >= p.ts - 120000));
    });
    store.pending.set(key, kept);
  }

  function header() {
    const a = findAgent(ws, run);
    const t = store.transcripts.get(key);
    const title = a?.title || t?.title || "Agent";
    setText(el.querySelector(".title"), title);
    document.title = `${title} · Rudder`;
    const status = a?.status || t?.status;
    const st = a ? agentStatus(a) : statusOf(status);
    const sub = el.querySelector(".subtitle");
    sub.className = `subtitle ${st.cls}`;
    sub.querySelector(".dot").classList.toggle("live", !!st.live);
    const phase = a?.phase && a.phase.toLowerCase() !== st.label.toLowerCase() && a.phase.toLowerCase() !== a.status ? a.phase : "";
    setText(sub.querySelector(".st"), phase ? `${st.label} · ${phase}` : st.label);
    const bk = backendLabel(a || { backend: t?.backend });
    setText(sub.querySelector(".bk"), bk);
    sub.querySelector(".sep").hidden = !bk;
    return status;
  }

  function paint() {
    const status = header();
    paintAsk();
    interruptBtn.hidden = !isRunning(status);
    const t = store.transcripts.get(key);
    if (!t) {
      if (!msgsEl.firstElementChild) {
        msgsEl.innerHTML = `<div class="sk-wrap" data-key="sk">
          <div class="sk sk-line" style="width:55%;margin-left:auto;height:38px;border-radius:18px"></div>
          <div class="sk sk-line" style="width:92%;margin-top:22px"></div><div class="sk sk-line" style="width:84%"></div><div class="sk sk-line" style="width:60%"></div>
          <div class="sk sk-line" style="width:70%;height:38px;margin-top:18px;border-radius:12px"></div>
          <div class="sk sk-line" style="width:88%;margin-top:18px"></div><div class="sk sk-line" style="width:40%"></div></div>`;
      }
      return;
    }
    const wasBottom = firstPaint || atBottom;
    const items = [];
    let lastDay = null;
    // A run of tool calls between two things the agent SAYS is one collapsed
    // "Worked · N steps" line: a long turn was otherwise a wall of tool rows
    // with the actual replies lost between them.
    let steps = null;
    for (const m of t.messages || []) {
      if (m.ts) {
        const d = new Date(m.ts).toDateString();
        if (d !== lastDay) { items.push({ kind: "day", id: `day-${d}`, label: dayLabel(m.ts) }); lastDay = d; steps = null; }
      }
      if (m.role === "tool") {
        if (!steps) { steps = { kind: "steps", id: `steps-${m.id}`, tools: [] }; items.push(steps); }
        steps.tools.push(m);
        continue;
      }
      steps = null;
      items.push({ kind: "msg", id: m.id, m });
    }
    for (let i = 0; i < items.length; i += 1) {
      if (items[i].kind === "steps" && items[i].tools.length === 1) {
        const m = items[i].tools[0];
        items[i] = { kind: "msg", id: m.id, m };
      }
    }
    for (const p of store.pending.get(key) || []) items.push({ kind: "pending", id: p.id, p });

    if (!items.length && t.waking) {
      items.push({ kind: "error", id: "waking", text: "Waking the cloud machine. Your agents reopen in about a minute." });
    } else if (!items.length && !t.error) {
      items.push({ kind: "empty", id: "empty" });
    } else if (!items.length && t.error) {
      items.push({ kind: "error", id: "error", text: t.error });
    }

    el.querySelector(".truncated-note").hidden = !t.truncated;
    reconcile(msgsEl, items, (x) => x.id, createItem, updateItem);
    typing.hidden = status !== "working";

    const count = (t.messages || []).length;
    if (wasBottom) { toBottom(false); requestAnimationFrame(() => toBottom(false)); }
    else if (count > lastCount && lastCount > 0) pill.classList.add("show");
    lastCount = count;
    firstPaint = false;
    atBottom = isAtBottom() || wasBottom;
  }

  function createItem(x) {
    if (x.kind === "day") return h(`<div class="day"></div>`);
    if (x.kind === "empty") return emptyState({ ic: "inbox", title: "No messages yet", body: "This agent has not said anything yet. Its conversation shows up here as it works." });
    if (x.kind === "error") return emptyState({ ic: "alert", title: "Couldn't load the conversation", body: escapeHtml(x.text) });
    if (x.kind === "pending") return h(`<div class="msg user"><div class="bubble md"></div><div class="m-foot"></div></div>`);
    if (x.kind === "steps") {
      return h(`<div class="steps"><button type="button" class="tool-row steps-row" aria-expanded="false">
        <span class="t-icon"></span><span class="t-name"></span><span class="t-input"></span>${icon("chevron").replace("<svg", '<svg class="t-caret"')}</button><div class="steps-list" hidden></div></div>`);
    }
    const role = x.m.role;
    if (role === "tool") {
      const box = h(`<div class="tool"><button type="button" class="tool-row" aria-expanded="false">
        <span class="t-icon"></span><span class="t-name"></span><span class="t-input"></span>${icon("chevron").replace("<svg", '<svg class="t-caret"')}</button></div>`);
      return box;
    }
    if (role === "user") return h(`<div class="msg user"><div class="bubble md"></div></div>`);
    if (role === "system") return h(`<div class="msg system"><div class="sys md"></div></div>`);
    return h(`<div class="msg assistant"><div class="md"></div></div>`);
  }

  function updateItem(node, x) {
    if (x.kind === "day") return setText(node, x.label);
    if (x.kind === "empty" || x.kind === "error") return;
    if (x.kind === "pending") {
      const p = x.p;
      node.className = `msg user ${p.state === "failed" ? "failed" : "pending"}`;
      const md = node.querySelector(".md");
      const sig = `${p.text}|${markdownReady()}`;
      if (md.dataset.sig !== sig) { renderMarkdown(md, p.text); md.dataset.sig = sig; }
      const foot = node.querySelector(".m-foot");
      const html = p.state === "failed" ? `<span>Not sent</span><button type="button" class="retry">Retry</button>` : `<span>${p.state === "sent" ? "Delivered" : "Sending"}</span>`;
      if (foot.dataset.h !== html) { foot.innerHTML = html; foot.dataset.h = html; }
      return;
    }
    if (x.kind === "steps") return paintSteps(node, x);
    const m = x.m;
    if (m.role === "tool") { node._msg = m; return paintTool(node, m); }
    const md = node.querySelector(".md");
    const sig = `${m.text}|${markdownReady()}`;
    if (md.dataset.sig !== sig) { renderMarkdown(md, m.text || ""); md.dataset.sig = sig; }
  }

  function paintSteps(node, x) {
    const open = store.expanded.has(`${key}|${x.id}`);
    node._steps = x;
    node.classList.toggle("open", open);
    const row = node.querySelector(".steps-row");
    row.setAttribute("aria-expanded", String(open));
    const failed = x.tools.filter((m) => m.tool?.isError).length;
    const last = x.tools[x.tools.length - 1].tool || {};
    row.querySelector(".t-icon").innerHTML = icon(failed ? "alert" : "check");
    setText(row.querySelector(".t-name"), `Worked · ${x.tools.length} steps${failed ? ` · ${failed} failed` : ""}`);
    setText(row.querySelector(".t-input"), open ? "" : `${last.name || "Tool"} ${(last.input || "").replace(/\s+/g, " ").trim()}`);
    const list = node.querySelector(".steps-list");
    list.hidden = !open;
    if (!open) { list.replaceChildren(); return; }
    reconcile(list, x.tools.map((m) => ({ kind: "msg", id: m.id, m })), (y) => y.id, createItem, updateItem);
  }

  function paintTool(box, m) {
    const tool = m.tool || {};
    const k = `${key}|${box.dataset.key}`;
    const open = store.expanded.has(k);
    const sig = `${tool.name}|${tool.input}|${tool.output?.length}|${tool.isError}|${open}|${store.showMore.has(k)}`;
    if (box.dataset.sig === sig) return;
    box.dataset.sig = sig;
    box.classList.toggle("error", !!tool.isError);
    box.classList.toggle("open", open);
    const row = box.querySelector(".tool-row");
    row.setAttribute("aria-expanded", String(open));
    row.querySelector(".t-icon").innerHTML = icon(tool.isError ? "alert" : toolIcon(tool.name));
    setText(row.querySelector(".t-name"), tool.name || "Tool");
    setText(row.querySelector(".t-input"), (tool.input || "").replace(/\s+/g, " ").trim());
    row.setAttribute("aria-label", `${tool.name || "Tool"}${tool.isError ? " (error)" : ""}: ${tool.input || ""}. ${open ? "Collapse" : "Expand"} output`);
    let out = box.querySelector(".tool-out");
    if (!open) { out?.remove(); return; }
    if (!out) { out = h(`<div class="tool-out"></div>`); box.appendChild(out); }
    const text = tool.output ?? "";
    if (!text) { out.innerHTML = `<div class="t-empty">No output</div>`; return; }
    const full = store.showMore.has(k);
    const long = text.length > OUT_LIMIT;
    out.innerHTML = `<pre></pre>${long ? `<div class="t-more"><span></span><button type="button" class="show-more"></button></div>` : ""}`;
    out.querySelector("pre").textContent = long && !full ? text.slice(0, OUT_LIMIT) : text;
    if (long) {
      const lines = text.split("\n").length;
      setText(out.querySelector(".t-more span"), full ? `${lines.toLocaleString()} lines` : `${Math.round((text.length - OUT_LIMIT) / 100) / 10}k more characters`);
      setText(out.querySelector(".show-more"), full ? "Show less" : "Show more");
    }
  }

  // Polling
  let timer = null;
  const tick = async () => {
    timer = null;
    if (!document.hidden) await loadTranscript();
    timer = setTimeout(tick, waking ? 5000 : 2000);
  };
  const onVis = () => { if (!document.hidden) { clearTimeout(timer); tick(); } };
  document.addEventListener("visibilitychange", onVis);
  const unReady = () => { if (screen?.el === el) paint(); };
  onMarkdownReady(unReady);
  tick();

  return {
    el,
    update: paint,
    destroy() {
      clearTimeout(timer); timer = -1;
      termOn = false; clearTimeout(termTimer);
      fastStatePoll = null;
      document.removeEventListener("visibilitychange", onVis);
      ro.disconnect();
      document.title = "Rudder";
    },
  };
}

/* ------------------------------------------------------------------ */
/* Viewport, polling, boot                                             */
/* ------------------------------------------------------------------ */

function syncViewport() {
  const vv = window.visualViewport;
  const root = document.documentElement;
  if (!vv) return;
  root.style.setProperty("--vvh", `${vv.height}px`);
  root.style.setProperty("--vvtop", `${vv.offsetTop}px`);
  document.body.classList.toggle("kb-open", window.innerHeight - vv.height > 120);
  if (window.scrollY !== 0 && !vv.offsetTop) window.scrollTo(0, 0);
}
window.visualViewport?.addEventListener("resize", syncViewport);
window.visualViewport?.addEventListener("scroll", syncViewport);
syncViewport();

let stateTimer = null;
// The open agent screen asks for a faster cadence while its agent is running
// or waiting on an answer, so a new prompt shows up promptly.
let fastStatePoll = null;
function pollState() {
  clearTimeout(stateTimer);
  stateTimer = setTimeout(async () => {
    if (!document.hidden) await refreshState();
    pollState();
  }, fastStatePoll?.() ? 1500 : 4000);
}
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) { refreshState(); pollState(); }
});
// Re-render relative times every 30s even when data holds still.
setInterval(() => { if (!document.hidden) screen?.update?.(); }, 30000);

window.rudderRefresh = () => {
  nativeSignInRequested = false;
  if (NATIVE && window.RUDDER_NATIVE?.token) NATIVE.token = window.RUDDER_NATIVE.token;
  return refreshState();
};

onMarkdownReady(() => { /* agent screen repaints itself */ });
loadMarkdown();

function restoreLastRoute() {
  const here = parseRoute(location.pathname);
  if (here.name !== "home" || history.state?.depth) return;
  const last = ls.get("rudder.lastRoute");
  if (!last || Date.now() - last.at > 24 * 3600 * 1000) return;
  const r = parseRoute(last.path || "");
  if (r.name === "home") return;
  history.replaceState({ depth: 0 }, "", "/app");
  depth = 1;
  history.pushState({ depth }, "", projectPath(r.ws));
  if (r.name === "agent") { depth = 2; history.pushState({ depth }, "", agentPath(r.ws, r.run)); }
}

if (!history.state) history.replaceState({ depth: 0 }, "", location.pathname + location.search);
// /app/pair?code=ABCD-EFGH (the link `rudder cloud app` prints) signs in here.
const pairFromLink = location.pathname === "/app/pair" ? new URLSearchParams(location.search).get("code") : null;
if (pairFromLink && !NATIVE) {
  history.replaceState({ depth: 0 }, "", "/app");
  redeemPairCode(pairFromLink, null);
} else {
  restoreLastRoute();
}
mountScreen();
refreshState().then(pollState);

if (!NATIVE && "serviceWorker" in navigator && (location.protocol === "https:" || location.hostname === "localhost")) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/app/sw.js", { scope: "/app" })
      .catch(() => navigator.serviceWorker.register("/app/sw.js", { scope: "/app/" }))
      .catch((err) => console.warn("service worker not registered", err));
  });
}
