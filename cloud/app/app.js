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

class ApiError extends Error { constructor(msg, status) { super(msg); this.status = status; } }

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
  if (!res.ok) throw new ApiError(data?.error || `${res.status} ${res.statusText || "Request failed"}`.trim(), res.status);
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
const needsYou = (a) => a.status === "waiting" || a.status === "review" || a.status === "failed";
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

function openSettings() {
  const s = store.state || {};
  const home = s.home;
  const rows = [
    ["Account", s.email || "Signed in"],
    ["Home machine", home ? machineLabel(home).label : "None"],
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
    return el;
  };
}
function updateAgentRow(el, { p, a }) {
  const st = statusOf(a.status);
  el.href = agentPath(p.workspaceId, a.id);
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
    stopped: { label: "Asleep", cls: "s-idle" },
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
    <p>Steer your coding agents from anywhere. Sign in to reach your home machine.</p>
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
      <div class="machine-slot"></div>
      <div class="section needs-sec" hidden>
        <div class="section-head"><h2>Needs you</h2><span class="count"></span></div>
        <div class="list grouped card needs"></div>
      </div>
      <div class="section projects-sec">
        <div class="section-head"><h2>Projects</h2><span class="count"></span></div>
        <div class="projects-body"><div class="list projects"></div></div>
        <div class="add-slot" style="margin-top:10px"></div>
      </div>
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
      <div class="m-body"><div class="m-title"><span>Home machine</span><span class="pill"><span class="dot"></span><span class="pl"></span></span></div>
      <div class="m-sub"></div></div>
      <button type="button" class="btn small accent wake" hidden>${icon("power")}<span>Wake</span></button>
    </div>`);
  const wakeBtn = machine.querySelector(".wake");
  wakeBtn.addEventListener("click", async () => {
    wakeBtn.disabled = true;
    try {
      const res = await api("/api/app/wake", { method: "POST", body: {} });
      if (res?.home && store.state) store.state.home = res.home;
      toast("Waking your home machine. This can take up to a minute.");
      update();
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { wakeBtn.disabled = false; }
  });

  // Add project
  let addOpen = false;
  const ghost = h(`<button type="button" class="ghost-add">${icon("plus")}<span>Add project</span></button>`);
  const addCard = h(`<div class="card add-card"><form>
      <input class="input" name="repo" placeholder="owner/name" aria-label="GitHub repository, owner/name" autocapitalize="off" autocorrect="off" spellcheck="false" inputmode="url">
      <button type="submit" class="btn primary">Add</button></form>
      <p class="hint">Clones the GitHub repo onto your home machine.</p></div>`);
  ghost.addEventListener("click", () => { addOpen = true; renderAdd(); addCard.querySelector("input").focus(); });
  addCard.querySelector("form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const input = addCard.querySelector("input");
    let repo = input.value.trim().replace(/\.git$/, "");
    const m = /github\.com[/:]([^/\s]+\/[^/\s]+)/i.exec(repo);
    if (m) repo = m[1];
    if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) { toast("Use the owner/name form, like vercel/next.js", { error: true }); input.focus(); return; }
    const btn = addCard.querySelector("button");
    btn.disabled = true;
    try {
      const res = await api("/api/app/open", { method: "POST", body: { repo } });
      if (res?.home && store.state) store.state.home = res.home;
      toast(`Cloning ${repo}`);
      input.value = ""; addOpen = false; renderAdd();
      await refreshState();
      const ws = res?.workspace?.workspaceId || res?.workspace?.id;
      if (ws) navigate(projectPath(ws));
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { btn.disabled = false; }
  });
  function renderAdd() {
    const want = addOpen ? addCard : ghost;
    if (addSlot.firstElementChild !== want) addSlot.replaceChildren(want);
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
      else if (home.status === "stopped") subText = "Asleep to save cost. Wake it to resume agents.";
      else if (home.status === "queued") subText = "Booting. This can take up to a minute.";
      else if (home.status === "deleting") subText = "Being removed.";
      else {
        const bits = [];
        if (home.tailnet?.dnsName) bits.push(home.tailnet.dnsName);
        else if (home.tailnet?.hostname) bits.push(home.tailnet.hostname);
        bits.push(`${home.activeAgents || 0} active ${home.activeAgents === 1 ? "agent" : "agents"}`);
        subText = bits.join(" · ");
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
      sub.classList.toggle("err", home.status === "failed" || broken.length > 0);
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

    // Projects
    const projects = (s.projects || []).slice().sort((a, b) => (toMs(b.lastActivityAt) || 0) - (toMs(a.lastActivityAt) || 0));
    setText(projectsSec.querySelector(".count"), projects.length ? String(projects.length) : "");
    reconcile(projectsEl, projects, (p) => p.workspaceId, createProjectCard, updateProjectCard);

    if (!projects.length) {
      const body = home
        ? "Add a GitHub repo to clone it onto your home machine, then start agents on it from here."
        : "No home machine yet. Run <code>rudder cloud</code> in a repo on your laptop, or add a project.";
      const key = home ? "h" : "n";
      if (!emptyEl || emptyEl.dataset.k !== key) {
        emptyEl = emptyState({ ic: home ? "folder" : "server", title: home ? "No projects yet" : "Welcome to Rudder", body });
        emptyEl.dataset.k = key;
      }
      if (emptyEl.parentNode !== projectsBody) projectsBody.appendChild(emptyEl);
      if (!addOpen) { addOpen = true; }
    } else if (emptyEl) { emptyEl.remove(); emptyEl = null; }
    renderAdd();
  }

  return { el, update };
}

function createProjectCard() {
  return h(`<a class="card project tap" data-link>
    <div class="p-top"><span class="p-name"></span><span class="pill p-dash"></span><span class="dot live s-working p-live"></span></div>
    <div class="p-repo"></div>
    <div class="p-stats">
      <span class="stat st-w s-working"><span class="dot"></span><b></b> working</span>
      <span class="stat st-n s-needs"><span class="dot"></span><b></b> need you</span>
      <span class="stat st-d s-done"><span class="dot"></span><b></b> done</span>
      <span class="p-time"></span>
    </div>
    <div class="p-err"></div></a>`);
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
  if (a.status === "working") return "working";
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
    placeholder: "Start a new task",
    draftKey: `task:${ws}`,
    hint: coarse ? "" : "Enter to start · Shift+Enter for a new line",
    onSubmit: async (text) => {
      try {
        const res = await api("/api/app/task", { method: "POST", body: { workspace: ws, text } });
        toast("Task started");
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
        emptySlot.replaceChildren(emptyState({ ic: "folder", title: "Project not found", body: "It may still be cloning onto your home machine, or it was removed." , action: back }));
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
      setText(sub, p.failureReason || (p.dashboard === "cloning" ? `Cloning ${p.repo} onto your home machine.` : p.dashboard === "starting" ? "Starting the Rudder dashboard for this project." : "The Rudder dashboard for this project is not running."));
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

const OUT_LIMIT = 2400;

function AgentScreen(ws, run) {
  const key = `${ws}|${run}`;
  const el = h(`<section class="screen">
    <header class="topbar">
      <a class="icon-btn back" aria-label="Back to project">${icon("back")}</a>
      <div class="titles"><div class="title"></div>
        <div class="subtitle"><span class="dot"></span><span class="st"></span><span class="sep">·</span><span class="bk"></span></div></div>
      <button type="button" class="icon-btn more" aria-label="Agent actions" aria-haspopup="menu">${icon("more")}</button>
    </header>
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
    if (canStop) {
      menu.appendChild(h("<hr>"));
      add(`<button type="button" role="menuitem" class="danger">${icon("stop")}<span>Stop agent</span></button>`, () => act("stop"));
    }
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
  async function loadTranscript() {
    if (inflight) return;
    inflight = true;
    try {
      const t = await api(`/api/app/transcript?workspace=${encodeURIComponent(ws)}&run=${encodeURIComponent(run)}&limit=300`);
      store.transcripts.set(key, t);
      lastErr = null;
      reconcilePending(t);
    } catch (err) {
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
    const st = statusOf(status);
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
    for (const m of t.messages || []) {
      if (m.ts) {
        const d = new Date(m.ts).toDateString();
        if (d !== lastDay) { items.push({ kind: "day", id: `day-${d}`, label: dayLabel(m.ts) }); lastDay = d; }
      }
      items.push({ kind: "msg", id: m.id, m });
    }
    for (const p of store.pending.get(key) || []) items.push({ kind: "pending", id: p.id, p });

    if (!items.length && !t.error) {
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
    const m = x.m;
    if (m.role === "tool") { node._msg = m; return paintTool(node, m); }
    const md = node.querySelector(".md");
    const sig = `${m.text}|${markdownReady()}`;
    if (md.dataset.sig !== sig) { renderMarkdown(md, m.text || ""); md.dataset.sig = sig; }
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
    timer = setTimeout(tick, 2000);
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
function pollState() {
  clearTimeout(stateTimer);
  stateTimer = setTimeout(async () => {
    if (!document.hidden) await refreshState();
    pollState();
  }, 4000);
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
