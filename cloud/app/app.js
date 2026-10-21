// Rudder phone app. Plain ES modules, no build step.
import {
  icon, toolIcon, h, setText, reconcile, renderMarkdown, loadMarkdown, onMarkdownReady,
  markdownReady, copyText, escapeHtml,
} from "/app/render.js";

import {
  app, NATIVE, PLATFORM, postNative, openExternal, ls, coarse, toMs, rel, ApiError, api, toast, confirmDialog, track, showError,
} from "/app/core.js";
import { PlanView, planOf } from "/app/plan.js";
import { openModelPicker, modelChip } from "/app/model-picker.js";
import { SettingsScreen, JoinScreen, SlackLinkScreen, openLinearIssues, linearAvailable, rememberPendingLink, takePendingLink } from "/app/org.js";
import { OnboardingScreen, onboardingDue } from "/app/onboarding.js";
import { canOpenOnMac, openCopyOnMac, MAC_ICON } from "/app/mac-local.js";
import { TeamScreen, loadTeamFor, teamAgent, teamPath, hasTeam, memberName, firstName } from "/app/team.js";

function dayLabel(ms) {
  const d = new Date(ms), now = new Date();
  const start = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const diff = Math.round((start(now) - start(d)) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
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

// The feature modules (org.js, onboarding.js, model-picker.js, plan.js)
// reach the router and store through `app`.
Object.assign(app, {
  store, navigate: (...a) => navigate(...a), goBack: (...a) => goBack(...a), refreshState: () => refreshState(),
  updateScreen: () => updateScreen(), onSignedOut: () => handleSignedOut(), signOut: () => signOut(),
  openComputerSignIn: (c) => openComputerSignIn(c), openGithubConnect: () => openGithubConnect(),
  githubState: (st) => githubState(st), machineLabel: (hm) => machineLabel(hm), isDesktop: () => isDesktop(),
  // team.js: a teammate's agent reads with the same labels as your own.
  teamStatus: (a) => (a.asking ? { label: ASK_LABEL[a.asking] || "Needs approval", cls: "s-needs" } : statusOf(a.status)),
  backendLabel: (a) => backendLabel(a),
});

// The Mac shell posts a notification when an agent newly needs you (the
// same rows as the Needs you list). The first state is the baseline.
let notifiedNeeds = null;
function notifyNewNeeds(needs) {
  if (PLATFORM !== "mac" || !NATIVE?.notifications) return;
  const keys = new Set(needs.map(({ p, a }) => `${p.workspaceId}|${a.id}`));
  if (notifiedNeeds) {
    for (const { p, a } of needs) {
      const key = `${p.workspaceId}|${a.id}`;
      if (notifiedNeeds.has(key)) continue;
      const path = `/app/p/${encodeURIComponent(p.workspaceId)}/a/${encodeURIComponent(a.id)}`;
      // Already looking at it: no need to be told.
      if (document.hasFocus() && location.pathname === path) continue;
      const body = String(a.lastMessage || "Waiting for you").replace(/\s+/g, " ").slice(0, 160);
      postNative({ type: "notify", title: `${a.title || "An agent"} needs you · ${p.name || p.repo || ""}`.replace(/ · $/, ""), body, path });
    }
  }
  notifiedNeeds = keys;
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
      // An invite or Slack link opened while signed out reopens once signed in.
      if (!store.pendingChecked) {
        store.pendingChecked = true;
        const pending = takePendingLink();
        if (pending && pending !== location.pathname) setTimeout(() => navigate(pending), 0);
      }
      const needs = needsList();
      const count = needs.length;
      if (count !== store.badge) { store.badge = count; postNative({ type: "badge", count }); }
      notifyNewNeeds(needs);
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

// Add project: open one of your GitHub repos, or create a new one under your
// account or any org you belong to. The two are separate tabs because the
// owner filter for browsing and the owner a new repo goes under are separate
// choices.
function openAddProject() {
  const dlg = h(`<dialog class="sheet add-sheet" aria-label="Add project"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Add project</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="seg modes" role="tablist" aria-label="Add project">
      <button type="button" role="tab" data-mode="open" aria-selected="true">${icon("folder")}<span>Open a repo</span></button>
      <button type="button" role="tab" data-mode="create" aria-selected="false">${icon("plus")}<span>New repo</span></button>
    </div>
    <div class="add-pane open-pane">
      <label class="search-wrap">${icon("search")}<input class="input repo-search" type="search" placeholder="Search, or paste owner/name" aria-label="Search repositories"
        autocapitalize="off" autocorrect="off" spellcheck="false"></label>
      <div class="owner-chips" role="tablist" aria-label="Show repos from"></div>
      <div class="repo-list" role="list"><div class="repo-note">Loading your GitHub repos…</div></div>
    </div>
    <form class="add-pane create-pane" hidden novalidate>
      <div class="field-label" id="owner-label">Create under</div>
      <div class="owner-list" role="radiogroup" aria-labelledby="owner-label"><div class="repo-note">Loading your GitHub accounts…</div></div>
      <label class="field-label" for="new-repo-name">Repository name</label>
      <input id="new-repo-name" class="input" name="name" placeholder="my-new-app" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="go" maxlength="100">
      <div class="name-hint" aria-live="polite"></div>
      <label class="field-label" for="new-repo-desc">Description <span class="opt">optional</span></label>
      <input id="new-repo-desc" class="input" name="description" placeholder="What it's for" maxlength="300">
      <div class="field-label" id="vis-label">Visibility</div>
      <div class="seg vis" role="radiogroup" aria-labelledby="vis-label">
        <button type="button" role="radio" data-vis="private" aria-checked="true"><span>Private</span><small>Only people you add</small></button>
        <button type="button" role="radio" data-vis="public" aria-checked="false"><span>Public</span><small>Anyone can see it</small></button>
      </div>
      <button type="submit" class="btn accent block create-btn" disabled>Create repository</button>
      <div class="create-note">GitHub creates it with a README, then it opens on your cloud machine, ready for agents.</div>
    </form>
  </div></dialog>`);
  const search = dlg.querySelector(".repo-search");
  const listEl = dlg.querySelector(".repo-list");
  const chipsEl = dlg.querySelector(".owner-chips");
  const form = dlg.querySelector(".create-pane");
  const ownerList = dlg.querySelector(".owner-list");
  const nameIn = form.querySelector('[name="name"]');
  const descIn = form.querySelector('[name="description"]');
  const hint = form.querySelector(".name-hint");
  const createBtn = form.querySelector(".create-btn");
  let repos = [];
  let owners = []; // [{ login, kind: "user" | "org" }]
  let loaded = false;
  let filter = ls.get("rudder.addFilter") || "";
  let target = ls.get("rudder.newOwner") || "";
  let visibility = "private";
  let mode = "open";
  const ownerOf = (repo) => repo.split("/")[0];
  const avatar = (login, size) => {
    const img = document.createElement("img");
    img.src = `https://github.com/${encodeURIComponent(login)}.png?size=${size}`;
    img.alt = ""; img.loading = "lazy";
    img.addEventListener("error", () => { img.style.visibility = "hidden"; });
    return img;
  };

  function setMode(next) {
    mode = next;
    for (const b of dlg.querySelectorAll(".modes button")) b.setAttribute("aria-selected", String(b.dataset.mode === mode));
    dlg.querySelector(".open-pane").hidden = mode !== "open";
    form.hidden = mode !== "create";
    if (mode === "create" && !coarse) nameIn.focus();
    if (mode === "open" && !coarse) search.focus();
  }

  // Open a repo
  function renderChips() {
    chipsEl.hidden = owners.length < 2;
    chipsEl.replaceChildren(...[{ login: "" }, ...owners].map((o) => {
      const b = h(`<button type="button" role="tab" class="owner-chip"></button>`);
      b.setAttribute("aria-selected", String(o.login === filter));
      if (o.login) b.appendChild(avatar(o.login, 48));
      const label = document.createElement("span");
      label.textContent = o.login || "All";
      b.appendChild(label);
      b.addEventListener("click", () => { filter = o.login; ls.set("rudder.addFilter", filter); renderChips(); render(); });
      return b;
    }));
  }
  function render() {
    const q = search.value.trim().toLowerCase().replace(/^https?:\/\/github\.com\//, "").replace(/\.git$/, "");
    const shown = repos
      .filter((r) => !filter || ownerOf(r.repo).toLowerCase() === filter.toLowerCase())
      .filter((r) => !q || r.repo.toLowerCase().includes(q) || (r.description || "").toLowerCase().includes(q))
      .slice(0, 60);
    listEl.replaceChildren();
    const typed = /^[\w.-]+\/[\w.-]+$/.test(q) && !repos.some((r) => r.repo.toLowerCase() === q);
    if (typed) {
      const b = h(`<button type="button" class="repo-row" role="listitem"><span class="r-av"></span><span class="r-text"><span class="rn"></span><span class="rd">Open this repo</span></span><span class="r-go">Open</span></button>`);
      b.querySelector(".r-av").appendChild(avatar(ownerOf(q), 48));
      b.querySelector(".rn").textContent = q;
      b.addEventListener("click", () => openProjectRepo(q, dlg));
      listEl.appendChild(b);
    }
    for (const r of shown) {
      const b = h(`<button type="button" class="repo-row" role="listitem"><span class="r-av"></span><span class="r-text"><span class="rn"><span class="ro"></span><span class="rr"></span></span><span class="rd"></span></span><span class="r-go"></span></button>`);
      b.querySelector(".r-av").appendChild(avatar(ownerOf(r.repo), 48));
      b.querySelector(".ro").textContent = `${ownerOf(r.repo)}/`;
      b.querySelector(".rr").textContent = r.repo.slice(ownerOf(r.repo).length + 1);
      b.querySelector(".rd").textContent = [r.private ? "Private" : "Public", r.updatedAt ? rel(toMs(r.updatedAt)) : null, r.description].filter(Boolean).join(" · ");
      const go = b.querySelector(".r-go");
      go.textContent = r.open ? "Added" : "Open";
      go.classList.toggle("added", !!r.open);
      b.setAttribute("aria-label", `${r.repo}${r.open ? ", already on your cloud machine" : ""}. Open`);
      b.addEventListener("click", () => openProjectRepo(r.repo, dlg));
      listEl.appendChild(b);
    }
    if (!typed && !shown.length) {
      const note = h(`<div class="repo-note"></div>`);
      note.textContent = !loaded ? "Loading your GitHub repos…" : repos.length ? "No matching repos." : "No repos yet. Create one in New repo.";
      listEl.appendChild(note);
    }
  }

  // New repo
  function renderOwners() {
    if (!owners.length) return;
    if (!owners.some((o) => o.login === target)) target = owners[0].login;
    ownerList.replaceChildren(...owners.map((o) => {
      const b = h(`<button type="button" role="radio" class="owner-opt"><span class="o-av"></span><span class="o-text"><span class="o-name"></span><span class="o-kind"></span></span><span class="o-radio">${icon("check")}</span></button>`);
      b.querySelector(".o-av").appendChild(avatar(o.login, 72));
      b.querySelector(".o-name").textContent = o.login;
      b.querySelector(".o-kind").textContent = o.kind === "user" ? "Your account" : "Organization";
      b.setAttribute("aria-checked", String(o.login === target));
      b.addEventListener("click", () => { target = o.login; ls.set("rudder.newOwner", target); renderOwners(); syncCreate(); });
      return b;
    }));
  }
  const cleanName = (raw) => raw.trim().replace(/[^A-Za-z0-9._-]+/g, "-").replace(/-{2,}/g, "-").replace(/^[-.]+|-+$/g, "");
  function syncCreate() {
    const raw = nameIn.value;
    const name = cleanName(raw);
    const full = target && name ? `${target}/${name}` : "";
    const exists = full && repos.find((r) => r.repo.toLowerCase() === full.toLowerCase());
    hint.className = "name-hint";
    hint.replaceChildren();
    if (!raw.trim()) {
      hint.textContent = "Letters, numbers, dashes, dots and underscores.";
    } else if (!name || name === "." || name === "..") {
      hint.classList.add("err");
      hint.textContent = "That name has nothing GitHub can use. Try letters and numbers.";
    } else if (exists) {
      hint.classList.add("err");
      hint.append(`${exists.repo} already exists. `);
      const open = h(`<button type="button" class="link">Open it</button>`);
      open.addEventListener("click", () => openProjectRepo(exists.repo, dlg));
      hint.appendChild(open);
    } else {
      hint.append(name === raw.trim() ? "Creates " : "Creates it as ");
      const b = document.createElement("b");
      b.textContent = full || name;
      hint.appendChild(b);
    }
    createBtn.disabled = !full || !!exists || !loaded;
    setText(createBtn, full && !exists ? `Create ${full}` : "Create repository");
  }
  for (const b of form.querySelectorAll(".vis button")) {
    b.addEventListener("click", () => {
      visibility = b.dataset.vis;
      for (const x of form.querySelectorAll(".vis button")) x.setAttribute("aria-checked", String(x.dataset.vis === visibility));
    });
  }
  nameIn.addEventListener("input", syncCreate);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = cleanName(nameIn.value);
    if (createBtn.disabled || !target || !name) return;
    const full = `${target}/${name}`;
    createBtn.disabled = true;
    createBtn.classList.add("busy");
    setText(createBtn, `Creating ${full}…`);
    try {
      const res = await api("/api/app/create", { method: "POST", body: { owner: target, name, private: visibility === "private", description: descIn.value.trim() } });
      dlg.close();
      toast(`Created ${full}`);
      await refreshState();
      const ws = res?.workspace?.workspaceId || res?.workspace?.id;
      if (ws) navigate(projectPath(ws));
    } catch (err) {
      if (err.status !== 401) toast(err.message, { error: true });
      createBtn.classList.remove("busy");
      syncCreate();
    }
  });

  for (const b of dlg.querySelectorAll(".modes button")) b.addEventListener("click", () => setMode(b.dataset.mode));
  search.addEventListener("input", render);
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest('[data-act="close"]')) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
  syncCreate();
  api("/api/app/repos").then((data) => {
    repos = data?.repos || [];
    // The first owner is the signed-in account, the rest its orgs.
    owners = (data?.owners || []).map((o, i) => (typeof o === "string" ? { login: o, kind: i === 0 ? "user" : "org" } : o)).filter((o) => o?.login);
    loaded = true;
    if (filter && !owners.some((o) => o.login === filter)) filter = "";
    renderChips(); render(); renderOwners(); syncCreate();
  }).catch((err) => {
    const msg = err.status === 401 ? "Signed out." : err.message;
    for (const box of [listEl, ownerList]) {
      box.replaceChildren(h(`<div class="repo-note err"></div>`));
      box.firstChild.textContent = msg;
    }
  });
}

// Settings is a screen (/app/settings) so the Slack and Linear OAuth
// round trips can land back on it.
function openSettings() { navigate("/app/settings"); }

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
  if (parts[1] === "digest") return { name: "digest", id: parts[2] ? decodeURIComponent(parts[2]) : null, path };
  if (parts[1] === "settings") return { name: "settings", path: "/app/settings" };
  if (parts[1] === "team" && parts[2] && parts[3] && parts[4]) {
    // A teammate's agent, read only. Not `ws`: the sidebar and plan column
    // follow `ws`, and this project is not yours.
    return { name: "team-agent", member: decodeURIComponent(parts[2]), tws: decodeURIComponent(parts[3]), run: decodeURIComponent(parts[4]), path };
  }
  if (parts[1] === "team") return { name: "team", path: `/app/team${location.search}` };
  if (parts[1] === "welcome") return { name: "welcome", path: "/app/welcome" };
  if (parts[1] === "join" && parts[2]) return { name: "join", token: decodeURIComponent(parts[2]), path, needsAccount: true };
  if (parts[1] === "link" && parts[2] === "slack" && parts[3]) return { name: "slack-link", code: decodeURIComponent(parts[3]), path, needsAccount: true };
  if (parts[1] === "p" && parts[2]) {
    const ws = decodeURIComponent(parts[2]);
    if (parts[3] === "a" && parts[4]) return { name: "agent", ws, run: decodeURIComponent(parts[4]), path };
    return { name: "project", ws, path };
  }
  return { name: "home", path: "/app" };
}
const projectPath = (ws) => `/app/p/${encodeURIComponent(ws)}`;
const agentPath = (ws, run) => `${projectPath(ws)}/a/${encodeURIComponent(run)}`;
const digestPath = (id) => (id ? `/app/digest/${encodeURIComponent(id)}` : "/app/digest");

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

// Desktop (a wide browser, the Mac app): a sidebar of projects and agents,
// the screen in the middle, and the project's plan beside it. Phones keep the
// stacked screens.
const DESKTOP_MQ = matchMedia("(min-width: 1024px)");
let desktop = null;
app.onScopeChange = () => desktop?.update();
const isDesktop = () => !!desktop;
// Full-page screens (no sidebar), on desktop too.
const FULL_PAGE = new Set(["join", "slack-link", "welcome", "onboarding"]);

function buildScreen(route) {
  if (route.name === "project") return ProjectScreen(route.ws);
  if (route.name === "agent") return AgentScreen(route.ws, route.run);
  if (route.name === "digest") return DigestScreen(route.id);
  if (route.name === "settings") return SettingsScreen();
  if (route.name === "team") return TeamScreen();
  if (route.name === "team-agent") return AgentScreen(route.tws, route.run, { member: route.member });
  if (route.name === "welcome") return OnboardingScreen({ again: !onboardingDue(store.state) });
  if (route.name === "join") return JoinScreen(route.token);
  if (route.name === "slack-link") return SlackLinkScreen(route.code);
  if (route.name === "onboarding") return OnboardingScreen();
  return HomeScreen();
}

function mountScreen(dir = "forward") {
  if (screen) { screen.destroy?.(); screen = null; }
  let route = null;
  if (!store.signedOut) {
    route = parseRoute(location.pathname);
    // First run: pick how you use Rudder before anything else.
    if (route.name === "home" && onboardingDue(store.state)) route = { name: "onboarding", path: "/app" };
    if (!route.needsAccount && route.name !== "onboarding") ls.set("rudder.lastRoute", { path: route.path, at: Date.now() });
  }
  const wantDesktop = DESKTOP_MQ.matches && !!route && !FULL_PAGE.has(route.name);
  document.body.classList.toggle("desktop", wantDesktop);
  if (wantDesktop && !desktop) {
    viewEl.textContent = "";
    desktop = DesktopShell();
    viewEl.appendChild(desktop.el);
  } else if (!wantDesktop && desktop) {
    desktop.destroy(); desktop = null;
    viewEl.textContent = "";
  }
  const target = desktop ? desktop.main : viewEl;
  target.textContent = "";
  screen = store.signedOut ? SignInScreen() : buildScreen(route);
  if (dir === "back" && !desktop) screen.el.classList.add("back-nav");
  target.appendChild(screen.el);
  screen.update?.();
  desktop?.update(route);
}
function updateScreen() {
  if (store.signedOut && !(screen?.isSignIn)) return mountScreen();
  if (!store.signedOut && screen?.isSignIn) return mountScreen();
  // The state that says onboarding is due (or done) arrives after the first paint.
  if (!store.signedOut && parseRoute(location.pathname).name === "home" && !!screen?.isOnboarding !== onboardingDue(store.state)) return mountScreen();
  screen?.update?.();
  desktop?.update(parseRoute(location.pathname));
}
DESKTOP_MQ.addEventListener?.("change", () => mountScreen());

function DesktopShell() {
  const el = h(`<div class="dsk">
    <nav class="dsk-side" aria-label="Projects and agents">
      <div class="ds-top"><a class="brand ds-brand" href="/app" data-link><img src="/app/icon.svg" alt=""><span>Rudder</span></a></div>
      <div class="ds-scroll">
        <div class="ds-sec ds-needs" hidden><div class="ds-h"><span>Needs you</span><span class="ds-c"></span></div><div class="ds-list ds-needs-list"></div></div>
        <div class="ds-sec"><div class="ds-h"><span>Projects</span><button type="button" class="ds-add" aria-label="Add project" title="Add project">${icon("plus")}</button></div><div class="ds-projects"></div></div>
      </div>
      <div class="ds-foot">
        <a class="ds-link ds-team" href="/app/team" data-link hidden>${icon("users")}<span>Team</span></a>
        <a class="ds-link ds-digest" href="/app/digest" data-link>${icon("sun")}<span>Digest</span></a>
        <a class="ds-link ds-settings" href="/app/settings" data-link><span class="avatar"></span><span class="ds-who"><span class="ds-email"></span><span class="ds-org"></span></span>${icon("settings")}</a>
      </div>
    </nav>
    <div class="dsk-main"></div>
    <aside class="dsk-plan" hidden aria-label="Plan"></aside>
  </div>`);
  const main = el.querySelector(".dsk-main");
  const planCol = el.querySelector(".dsk-plan");
  const projectsEl = el.querySelector(".ds-projects");
  const needsSec = el.querySelector(".ds-needs");
  const needsEl = el.querySelector(".ds-needs-list");
  el.querySelector(".ds-add").addEventListener("click", () => openAddProject());
  const openSet = new Set(ls.get("rudder.sideOpen", []));
  const saveOpen = () => ls.set("rudder.sideOpen", [...openSet]);
  let planView = null;
  let route = null;

  const sideAgent = (x) => {
    const a = h(`<a class="ds-agent" data-link><span class="dot"></span><span class="ds-at"><span class="ds-an"></span><span class="ds-as"></span></span></a>`);
    return a;
  };
  const paintAgent = (a, { p, a: ag, showProject }) => {
    const st = agentStatus(ag);
    a.className = `ds-agent ${st.cls}`;
    a.querySelector(".dot").classList.toggle("live", !!st.live);
    if (ag.pending) a.removeAttribute("href"); else a.href = agentPath(p.workspaceId, ag.id);
    setText(a.querySelector(".ds-an"), ag.title || ag.task || "Untitled agent");
    setText(a.querySelector(".ds-as"), showProject ? `${p.name} · ${st.label}` : st.label);
    const here = route?.name === "agent" && route.ws === p.workspaceId && route.run === ag.id;
    a.classList.toggle("on", here);
    if (here) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
    a.setAttribute("aria-label", `${ag.title || "Agent"}, ${st.label}${showProject ? `, ${p.name}` : ""}`);
  };

  function update(r) {
    if (r) route = r;
    const s = store.state;
    setText(el.querySelector(".avatar"), (s?.email || "?").slice(0, 1));
    setText(el.querySelector(".ds-email"), s?.email || "Settings");
    const orgId = ls.get("rudder.org") || "";
    const org = (s?.orgs || []).find((o) => o.id === orgId);
    setText(el.querySelector(".ds-org"), org ? org.name : "Personal");
    el.querySelector(".ds-settings").classList.toggle("on", route?.name === "settings");
    el.querySelector(".ds-digest").classList.toggle("on", route?.name === "digest");
    el.querySelector(".ds-team").hidden = !hasTeam(s);
    el.querySelector(".ds-team").classList.toggle("on", route?.name === "team" || route?.name === "team-agent");

    const needs = needsList();
    needsSec.hidden = !needs.length;
    setText(needsSec.querySelector(".ds-c"), needs.length ? String(needs.length) : "");
    reconcile(needsEl, needs.slice(0, 8), (x) => `${x.p.workspaceId}|${x.a.id}`, sideAgent, (a, x) => paintAgent(a, { ...x, showProject: true }));

    const projects = (s?.projects || []).slice().sort((a, b) => {
      const pa = pinnedAt(a), pb = pinnedAt(b);
      if (!!pa !== !!pb) return pa ? -1 : 1;
      if (pa) return pa < pb ? -1 : pa > pb ? 1 : 0;
      return (toMs(b.lastActivityAt) || 0) - (toMs(a.lastActivityAt) || 0);
    });
    const here = route?.ws;
    if (here && !openSet.has(here)) { openSet.add(here); saveOpen(); }
    // reconcile keeps unkeyed children, so the "Loading…" placeholder goes by hand.
    if (projects.length) projectsEl.querySelector(":scope > .ds-none")?.remove();
    reconcile(projectsEl, projects, (p) => p.workspaceId,
      () => h(`<div class="ds-proj"><div class="ds-prow"><button type="button" class="ds-tw" aria-expanded="false">${icon("chevron")}</button><a class="ds-pn" data-link><span class="ds-pname"></span><span class="ds-pc"></span></a></div><div class="ds-agents" role="list"></div></div>`),
      (pEl, p) => {
        const ws = p.workspaceId;
        const open = openSet.has(ws);
        const tw = pEl.querySelector(".ds-tw");
        tw.setAttribute("aria-expanded", String(open));
        tw.setAttribute("aria-label", `${open ? "Collapse" : "Expand"} ${p.name}`);
        tw.onclick = () => { if (openSet.has(ws)) openSet.delete(ws); else openSet.add(ws); saveOpen(); update(); };
        const link = pEl.querySelector(".ds-pn");
        link.href = projectPath(ws);
        setText(pEl.querySelector(".ds-pname"), p.name || p.repo);
        const n = (p.agents || []).filter(needsYou).length;
        const live = (p.agents || []).filter((a) => a.status === "working" || a.status === "starting").length;
        const pc = pEl.querySelector(".ds-pc");
        setText(pc, n ? String(n) : live ? String(live) : "");
        pc.className = `ds-pc ${n ? "s-needs" : live ? "s-working" : ""}`;
        pc.title = n ? `${n} need you` : live ? `${live} working` : "";
        const onProject = route?.name === "project" && route.ws === ws;
        pEl.classList.toggle("on", onProject);
        if (onProject) link.setAttribute("aria-current", "page"); else link.removeAttribute("aria-current");
        const list = pEl.querySelector(".ds-agents");
        list.hidden = !open;
        if (!open) { list.replaceChildren(); return; }
        // Live and recent agents; the project screen has the rest.
        const agents = (p.agents || []).filter((a) => groupOf(a) !== "earlier" || (route?.run === a.id))
          .sort((x, y) => (y.updatedAt || 0) - (x.updatedAt || 0)).slice(0, 12);
        if (agents.length) list.querySelector(":scope > .ds-none")?.remove();
        reconcile(list, agents, (a) => a.id, sideAgent, (a, ag) => paintAgent(a, { p, a: ag }));
        const hidden = (p.agents || []).length - agents.length;
        let moreEl = pEl.querySelector(".ds-more");
        if (hidden > 0) {
          if (!moreEl) { moreEl = h(`<a class="ds-more" data-link></a>`); pEl.appendChild(moreEl); }
          moreEl.href = projectPath(ws);
          setText(moreEl, `${hidden} earlier`);
        } else moreEl?.remove();
        if (!agents.length && !hidden) list.replaceChildren(h(`<div class="ds-none">No agents yet</div>`));
      });
    if (!projects.length) projectsEl.replaceChildren(h(`<div class="ds-none">${s ? "No projects yet" : "Loading…"}</div>`));

    // The plan beside the screen, for the project on screen.
    const p = route?.ws ? findProject(route.ws) : null;
    const showPlan = !!planOf(p);
    planCol.hidden = !showPlan;
    el.classList.toggle("with-plan", showPlan);
    if (!showPlan) { if (planView) { planView.el.remove(); planView = null; } return; }
    if (!planView || planView.ws !== p.workspaceId) {
      planView?.el.remove();
      planView = PlanView({ ws: p.workspaceId, hrefFor: (run) => agentPath(p.workspaceId, run), variant: "panel" });
      planView.ws = p.workspaceId;
      planCol.replaceChildren(planView.el);
    }
    planView.update(p);
    for (const n of planCol.querySelectorAll(".pv-node")) n.classList.toggle("on", !!route?.run && n.getAttribute("href") === agentPath(p.workspaceId, route.run));
  }
  return { el, main, update, destroy() { el.remove(); document.body.classList.remove("desktop"); } };
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

// A home machine running an older worker sends a compacted conversation's
// summary as a message from the user. Show it as the marker newer workers send.
const COMPACT_SUMMARY_RE = /^This session is being continued from a previous conversation that ran out of context\.[^\n]*\n+(?:Summary:\s*\n)?/;
function withCompactMarkers(messages) {
  if (!messages.some((m) => m.role === "user" && COMPACT_SUMMARY_RE.test(m.text || ""))) return messages;
  return messages.map((m) => (m.role === "user" && COMPACT_SUMMARY_RE.test(m.text || "")
    ? { id: m.id, role: "system", kind: "compact", ts: m.ts, text: "Context compacted", compact: { summary: m.text.replace(COMPACT_SUMMARY_RE, "").replace(/\n+(?:If you need specific details from before compaction|Continue the conversation from where it left off)[\s\S]*$/, "").trim() } }
    : m));
}

// Photos and files from the phone. Each one is uploaded to the project on the
// cloud machine (<repo>/.rudder/uploads, git-excluded) and the message carries
// the saved paths under "Attached files:", which the agent opens like any file.
const ATTACH_MAX_BYTES = 25 * 1024 * 1024;
const ATTACH_HEADER = "Attached files:";
// The iOS shell before 1.0.1 has no camera permission string, and picking
// "Take Photo" there crashes the app: offer files only until it updates.
const ATTACH_DOCS_ONLY = !!NATIVE && !NATIVE.attachments;
const IOS = NATIVE?.platform === "ios" || /iP(hone|ad|od)/.test(navigator.userAgent)
  || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const attachPreviews = new Map(); // saved path -> object URL of the image picked on this device

// The clip opens a menu: Photos, Camera, Files, each going straight to its
// picker. The iOS shell from 1.0.2 presents its own (RUDDER_NATIVE.picker),
// because WebKit's file input puts a second menu (Photo Library, Take Photo,
// Choose Files) in front of anything that accepts images. Elsewhere a file
// input stands in; on iOS "application/octet-stream" is what makes WebKit skip
// that menu for Files and still let every file type be picked.
const ATTACH_SOURCES = [
  { kind: "photos", icon: "image", label: "Photos", accept: "image/*" },
  { kind: "camera", icon: "camera", label: "Camera", accept: "image/*", capture: "environment" },
  { kind: "files", icon: "folder", label: "Files", accept: IOS ? "application/octet-stream" : "" },
];
const nativePicks = new Map(); // pick id -> resolve
window.rudderPicked = (res) => {
  const done = nativePicks.get(res?.id);
  if (!done) return;
  nativePicks.delete(res.id);
  done(res);
};
function pickNative(kind) {
  const id = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 7)}`;
  return new Promise((resolve) => {
    nativePicks.set(id, resolve);
    postNative({ type: "pick", kind, id });
  });
}
function base64File({ name, type, data }) {
  const bytes = Uint8Array.fromBase64 ? Uint8Array.fromBase64(data) : Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
  return new File([bytes], name || "file", { type: type || "application/octet-stream" });
}

// The menu lives in the composer so it rides up with the keyboard. A tap
// anywhere outside closes it (a scrim would be clipped to the composer, whose
// backdrop filter contains fixed children).
function openAttachMenu(composer, anchor, pick) {
  const menu = h(`<div class="menu attach-menu" role="menu" aria-label="Attach"></div>`);
  const close = () => {
    menu.remove();
    anchor.setAttribute("aria-expanded", "false");
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey, true);
  };
  const outside = (e) => { if (!menu.contains(e.target) && !anchor.contains(e.target)) close(); };
  const onKey = (e) => { if (e.key === "Escape") { close(); anchor.focus(); } };
  for (const src of ATTACH_SOURCES) {
    const locked = ATTACH_DOCS_ONLY && src.kind !== "files";
    const b = h(`<button type="button" role="menuitem">${icon(src.icon)}<span class="m-label"><span></span>${locked ? "<small>Update the app in TestFlight</small>" : ""}</span></button>`);
    b.querySelector(".m-label > span").textContent = src.label;
    b.disabled = locked;
    b.addEventListener("click", () => { close(); pick(src); });
    menu.appendChild(b);
  }
  composer.appendChild(menu);
  anchor.setAttribute("aria-expanded", "true");
  document.addEventListener("pointerdown", outside, true);
  document.addEventListener("keydown", onKey, true);
  if (!coarse) menu.querySelector("button:not([disabled])")?.focus();
  return close;
}

function withAttachments(text, paths) {
  if (!paths.length) return text;
  const list = paths.map((p) => `- ${p}`).join("\n");
  return text ? `${text}\n\n${ATTACH_HEADER}\n${list}` : `${ATTACH_HEADER}\n${list}`;
}

function splitAttachments(text) {
  const m = /(?:^|\n\n)Attached files:\n((?:- [^\n]+(?:\n|$))+)\s*$/.exec(text || "");
  if (!m) return { body: text || "", files: [] };
  return { body: text.slice(0, m.index).trim(), files: m[1].trim().split("\n").map((line) => line.slice(2).trim()).filter(Boolean) };
}

const attachmentName = (p) => String(p).split("/").pop().replace(/^\d{8}-\d{6}-(?:\d+-)?/, "");
const isImageName = (name) => /\.(png|jpe?g|gif|webp|heic|heif|avif|bmp)$/i.test(name);

// Chips under a message: a thumbnail when this device still has the image,
// otherwise the file's name.
function paintAttachments(box, files) {
  const sig = files.map((f) => `${f.path || f.name}|${f.url || ""}`).join(";");
  if (box.dataset.sig === sig) return;
  box.dataset.sig = sig;
  box.hidden = !files.length;
  box.replaceChildren(...files.map((f) => {
    const name = f.name || attachmentName(f.path);
    const url = f.url || (f.path && attachPreviews.get(f.path));
    const chip = h(`<span class="att${url ? " has-img" : ""}" title=""></span>`);
    chip.title = name;
    if (url) {
      const img = document.createElement("img");
      img.src = url; img.alt = name; img.loading = "lazy";
      chip.appendChild(img);
    } else {
      chip.innerHTML = `${icon("file")}<span class="att-name"></span>`;
      chip.querySelector(".att-name").textContent = name;
    }
    return chip;
  }));
}

async function uploadAttachment(ws, file) {
  if (file.size > ATTACH_MAX_BYTES) throw new ApiError(`${file.name || "That file"} is over 25 MB`, 413);
  const res = await api(`/api/app/upload?workspace=${encodeURIComponent(ws)}&name=${encodeURIComponent(file.name || "file")}`, { method: "POST", raw: file });
  if (!res?.path) throw new ApiError("The cloud machine did not save the file", 502);
  if (/^image\//.test(file.type)) attachPreviews.set(res.path, URL.createObjectURL(file));
  return res.path;
}

function createComposer({ placeholder, draftKey, onSubmit, hint, attachments = false }) {
  const el = h(`<div class="composer"><div class="att-tray" hidden></div><form autocomplete="off">
      ${attachments ? `<button type="button" class="attach" aria-label="Attach photos or files" aria-haspopup="menu" aria-expanded="false" title="Attach">${icon("clip")}</button><input type="file" class="file-in" multiple hidden>` : ""}
      <textarea rows="1" enterkeyhint="${coarse ? "enter" : "send"}" aria-label="${escapeHtml(placeholder)}" placeholder="${escapeHtml(placeholder)}"></textarea>
      <button type="submit" class="send" aria-label="Send" disabled>${icon("send")}</button>
    </form>${hint ? `<div class="c-hint">${escapeHtml(hint)}</div>` : ""}</div>`);
  const form = el.querySelector("form");
  form.classList.toggle("with-attach", attachments);
  const ta = el.querySelector("textarea");
  const btn = el.querySelector(".send");
  const tray = el.querySelector(".att-tray");
  let files = []; // [{ file, name, url }]
  ta.value = store.drafts[draftKey] || "";
  const grow = () => {
    ta.style.height = "auto";
    ta.style.height = `${Math.min(ta.scrollHeight, 168)}px`;
    btn.disabled = !ta.value.trim() && !files.length;
  };
  const paintTray = () => {
    tray.hidden = !files.length;
    tray.replaceChildren(...files.map((f, i) => {
      const chip = h(`<span class="att tray${f.url ? " has-img" : ""}"><button type="button" class="att-x" aria-label="Remove">${icon("x")}</button></span>`);
      if (f.url) {
        const img = document.createElement("img");
        img.src = f.url; img.alt = f.name;
        chip.prepend(img);
      } else {
        const label = h(`<span class="att-label">${icon("file")}<span class="att-name"></span></span>`);
        label.querySelector(".att-name").textContent = f.name;
        chip.prepend(label);
      }
      chip.querySelector(".att-x").setAttribute("aria-label", `Remove ${f.name}`);
      chip.querySelector(".att-x").addEventListener("click", () => {
        files.splice(i, 1);
        paintTray(); grow();
      });
      return chip;
    }));
  };
  if (attachments) {
    const input = el.querySelector(".file-in");
    const attachBtn = el.querySelector(".attach");
    const addFiles = (list) => {
      for (const file of list) {
        if (file.size > ATTACH_MAX_BYTES) { toast(`${file.name} is over 25 MB`, { error: true }); continue; }
        const name = file.name || "file";
        files.push({ file, name, url: /^image\//.test(file.type) ? URL.createObjectURL(file) : null });
      }
      paintTray(); grow();
    };
    // Runs inside the tap that chose the source: the browser only opens a
    // file input from a user gesture.
    const pick = async (src) => {
      if (NATIVE?.picker) {
        const res = await pickNative(src.kind);
        for (const why of res.skipped || []) toast(why, { error: true });
        addFiles((res.files || []).map(base64File));
        return;
      }
      input.accept = src.accept;
      if (src.capture) input.setAttribute("capture", src.capture); else input.removeAttribute("capture");
      input.click();
    };
    let closeMenu = null;
    attachBtn.addEventListener("click", () => {
      if (closeMenu && attachBtn.getAttribute("aria-expanded") === "true") { closeMenu(); return; }
      // A desktop file dialog already covers photos and files alike.
      if (!coarse && !NATIVE) { pick(ATTACH_SOURCES.find((s) => s.kind === "files")); return; }
      closeMenu = openAttachMenu(el, attachBtn, pick);
    });
    input.addEventListener("change", () => {
      addFiles([...(input.files || [])]);
      input.value = "";
      ta.focus();
    });
  }
  const saveDraft = () => {
    if (ta.value) store.drafts[draftKey] = ta.value; else delete store.drafts[draftKey];
    ls.set("rudder.drafts", store.drafts);
  };
  let busy = false;
  const submit = async () => {
    const text = ta.value.trim();
    if ((!text && !files.length) || busy) return;
    busy = true; btn.disabled = true;
    const picked = files;
    const ok = await onSubmit(text, picked);
    busy = false;
    if (ok !== false) {
      ta.value = ""; saveDraft();
      if (files === picked) { files = []; paintTray(); }
    }
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
        <div class="a-meta"><span class="status-text"></span><span class="chip c-lock" hidden>${icon("lock")}<span>Private</span></span><span class="chip c-proj"></span><span class="chip c-model"></span><span class="chip mono c-phase"></span></div>
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
  const lock = el.querySelector(".c-lock");
  lock.hidden = a.visibility !== "private" || !hasTeam(store.state);
  lock.title = "Only you can see this agent. Teammates see only its status.";
  const msg = (a.lastMessage || a.summary || a.task || "").replace(/\s+/g, " ").trim();
  const m = el.querySelector(".a-msg");
  setText(m, msg); m.hidden = !msg;
  el.setAttribute("aria-label", `${a.title || "Agent"}, ${st.label}, ${p.name}${lock.hidden ? "" : ", private"}`);
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
/* Agent sign-in: Claude Code and Codex on the cloud machine           */
/* ------------------------------------------------------------------ */

// The CLI's own login runs on the cloud machine (cloud/worker/logins.mjs) and
// the phone opens its page. In the iOS app the page's redirect comes straight
// back to the app (AgentLogin.swift), so there is nothing to copy. Without it,
// Claude shows a code to paste here and Codex a code to type on its page.
const AGENTS = [
  { id: "claude", label: "Claude Code", ic: "spark", account: "Claude" },
  { id: "codex", label: "Codex", ic: "code", account: "ChatGPT" },
];
const agentName = (provider) => AGENTS.find((x) => x.id === provider)?.label || provider;
const PLAN_LABEL = { max: "Max", pro: "Pro", plus: "Plus", team: "Team", business: "Business", enterprise: "Enterprise", edu: "Edu", free: "Free", API: "API key" };
const planLabel = (plan) => (plan ? PLAN_LABEL[plan] || plan[0].toUpperCase() + plan.slice(1) : "");
const NATIVE_LOGIN = !!NATIVE?.login;
const LOGIN_BUSY = new Set(["starting", "waiting", "finishing"]);

const authorizeWaiters = new Map();
window.rudderAuthorized = (res) => {
  const done = authorizeWaiters.get(res?.id);
  if (!done) return;
  authorizeWaiters.delete(res.id);
  done(res);
};
// Opens a sign-in page in the iOS app's system sheet (it shares Safari's
// logins). With a callback it resolves with the redirect's query; without one
// it resolves when the sheet closes.
function authorizeNative(id, url, callback = null) {
  return new Promise((resolve) => {
    authorizeWaiters.set(id, resolve);
    postNative({ type: "authorize", id, url, ...(callback ? { port: callback.port, path: callback.path } : {}) });
  });
}
function openSignInPage(url) {
  if (NATIVE_LOGIN) void authorizeNative(`page-${Date.now().toString(36)}`, url);
  else openExternal(url);
}

// What a row says about one agent's login on the machine.
function agentLoginState(home, provider) {
  const login = home?.logins?.[provider];
  if (login && LOGIN_BUSY.has(login.status)) {
    return { kind: "busy", pill: "Signing in", cls: "s-accent", sub: login.status === "finishing" ? "Finishing on the machine…" : "Waiting for you to approve" };
  }
  const cred = home?.credentials?.[provider];
  if (cred?.ok) {
    const bits = [cred.account, planLabel(cred.plan)].filter(Boolean);
    return { kind: "ok", pill: "Signed in", cls: "s-done", sub: bits.join(" · ") || "Signed in" };
  }
  if (cred && cred.ok === false) {
    const detail = cred.detail || "";
    if (/expired/i.test(detail)) return { kind: "out", sub: "Login expired. Sign in again.", err: true, expired: true };
    if (/not logged in|not signed in/i.test(detail)) return { kind: "out", sub: "Not signed in" };
    return { kind: "out", sub: detail, err: true };
  }
  if (!home?.connected) return { kind: "out", sub: "Not checked yet" };
  return { kind: "checking", sub: "Checking…" };
}

// Agents can't run at all, or a login that worked has expired: worth putting
// above the projects. One agent never signed in is not a problem.
function agentLoginTrouble(home) {
  if (!home?.credentials || home.status !== "running") return false;
  const states = AGENTS.map((x) => agentLoginState(home, x.id));
  return !states.some((st) => st.kind === "ok" || st.kind === "busy") || states.some((st) => st.expired);
}

function AccountsCard() {
  const el = h(`<div class="card accounts" role="list" aria-label="Agent sign-in"></div>`);
  const rows = new Map();
  for (const agent of AGENTS) {
    const row = h(`<div class="acct" role="listitem">
      <div class="m-icon">${icon(agent.ic)}</div>
      <div class="a-body"><div class="a-title"><span></span><span class="pill" hidden><span class="dot"></span><span class="pl"></span></span></div>
      <div class="a-sub"></div></div>
      <button type="button" class="btn small accent a-btn" hidden>Sign in</button>
      <button type="button" class="icon-btn a-more" aria-label="${escapeHtml(agent.label)} account" hidden>${icon("more")}</button>
    </div>`);
    setText(row.querySelector(".a-title span"), agent.label);
    row.querySelector(".a-btn").addEventListener("click", () => openAgentSignIn(agent.id));
    row.querySelector(".a-more").addEventListener("click", async () => {
      const cred = store.state?.home?.credentials?.[agent.id];
      const ok = await confirmDialog({
        title: `Switch ${agent.label} account?`,
        body: `Agents on your cloud machine use ${cred?.account || "this account"} now. They'll use the ${agent.account} account you sign in with next.`,
        confirm: "Sign in",
      });
      if (ok) openAgentSignIn(agent.id);
    });
    el.appendChild(row);
    rows.set(agent.id, row);
  }
  function update(home) {
    for (const agent of AGENTS) {
      const row = rows.get(agent.id);
      const st = agentLoginState(home, agent.id);
      const pill = row.querySelector(".pill");
      pill.hidden = !st.pill;
      if (st.pill) {
        pill.className = `pill ${st.cls}`;
        pill.querySelector(".dot").classList.toggle("live", st.kind === "busy");
        setText(pill.querySelector(".pl"), st.pill);
      }
      const sub = row.querySelector(".a-sub");
      setText(sub, st.sub);
      sub.classList.toggle("err", !!st.err);
      // A sign-in left half done (the app closed mid-way) can be started
      // over from here; the machine drops the old one.
      row.querySelector(".a-btn").hidden = !(st.kind === "out" || st.kind === "busy");
      row.querySelector(".a-more").hidden = st.kind !== "ok";
    }
  }
  return { el, update };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// One sign-in, start to finish, in a sheet.
function openAgentSignIn(provider) {
  const name = agentName(provider);
  const dlg = h(`<dialog class="sheet login-sheet" aria-label="Sign in to ${escapeHtml(name)}"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3></h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="ls-body" aria-live="polite"></div>
  </div></dialog>`);
  setText(dlg.querySelector("h3"), `Sign in to ${name}`);
  const body = dlg.querySelector(".ls-body");
  let session = null;
  let closed = false;
  let ended = false;

  const busy = (title, detail = "") => {
    body.replaceChildren(h(`<div class="ls-wait"><span class="spin" aria-hidden="true"></span><div><div class="ls-title"></div><div class="ls-note"></div></div></div>`));
    setText(body.querySelector(".ls-title"), title);
    setText(body.querySelector(".ls-note"), detail);
  };
  const failed = (message, { retry = true } = {}) => {
    body.replaceChildren(h(`<div class="ls-msg err"></div>`));
    setText(body.firstChild, message);
    const actions = h(`<div class="ls-actions"></div>`);
    if (retry) {
      const again = h(`<button type="button" class="btn block accent">Try again</button>`);
      again.addEventListener("click", () => void begin());
      actions.appendChild(again);
    }
    // In the iOS app the page normally hands its answer straight back; if
    // that did not work, the code routes still do.
    if (NATIVE_LOGIN) {
      const alt = h(`<button type="button" class="btn block ls-alt"></button>`);
      setText(alt, provider === "claude" ? "Use a code instead" : "Use a device code instead");
      alt.addEventListener("click", () => void begin(provider === "claude" ? "code" : "device"));
      actions.appendChild(alt);
    }
    body.appendChild(actions);
  };
  const succeeded = async () => {
    ended = true;
    body.replaceChildren(h(`<div class="ls-done"><div class="ls-check">${icon("check")}</div><div class="ls-title">Signed in</div>
      <div class="ls-note">Agents on your cloud machine now use this ${escapeHtml(AGENTS.find((x) => x.id === provider)?.account || "")} account.</div></div>`));
    toast(`${name} is signed in`);
    await refreshState();
    setTimeout(() => { if (!closed) dlg.close(); }, 1600);
  };
  const settle = (out) => {
    if (closed) return;
    if (out?.status === "done") void succeeded();
    else failed(out?.error || "The sign-in did not finish.");
  };

  // The machine may be asleep, or not exist yet for a new account: starting
  // the sign-in wakes or creates it, which can outlast one request.
  async function startSession(device) {
    const deadline = Date.now() + 5 * 60_000;
    for (;;) {
      const home = store.state?.home;
      busy(
        "Starting the sign-in on your cloud machine",
        !home ? "Creating your cloud machine first. The first start takes a minute or two."
          : !home.connected ? "Waking your cloud machine first. This can take a minute." : "",
      );
      try {
        return await api("/api/app/login/start", { method: "POST", body: { provider, device } });
      } catch (err) {
        if (closed || err.status === 401) throw err;
        if (err.status !== 503 || Date.now() > deadline) throw err;
        await refreshState();
        await sleep(3000);
        if (closed) throw new Error("closed");
      }
    }
  }

  async function finish(payload) {
    busy("Finishing on your cloud machine", `${name} is checking the sign-in.`);
    try {
      settle(await api("/api/app/login/finish", { method: "POST", body: { provider, id: session.id, ...payload } }));
    } catch (err) {
      if (!closed && err.status !== 401) failed(err.message);
    }
  }

  async function begin(mode = "auto") {
    ended = false;
    const device = provider === "codex" && (mode === "device" || !NATIVE_LOGIN);
    // Claude's paste-a-code page belongs to the same run as its callback, so
    // a run still waiting is reused rather than started over.
    const reuse = mode === "code" && session && session.codeUrl && store.state?.home?.logins?.[provider]?.id === session.id
      && LOGIN_BUSY.has(store.state.home.logins[provider].status);
    if (!reuse) {
      try {
        session = await startSession(device);
      } catch (err) {
        if (!closed && err.status !== 401) failed(err.message);
        return;
      }
    }
    if (closed) return;
    if (device) return showDevice();
    if (provider === "claude" && (mode === "code" || !NATIVE_LOGIN || !session.callback)) return showCode();
    busy(`Approve in the ${provider === "claude" ? "Claude" : "OpenAI"} window`, "It closes by itself once you're signed in.");
    const res = await authorizeNative(session.id, session.url, session.callback);
    if (closed) return;
    if (res?.callback) await finish({ callback: res.callback });
    else if (res?.cancelled) failed("The sign-in window closed before it finished.");
    else failed(res?.error || "The sign-in did not finish.");
  }

  function showCode() {
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-step"><span class="ls-n">1</span><div>Open the sign-in page and tap <b>Authorize</b>.</div></div>
      <button type="button" class="btn block accent ls-open">${icon("external")}<span>Open the sign-in page</span></button>
      <div class="ls-step"><span class="ls-n">2</span><div>Copy the code it shows, then paste it here.</div></div>
      <form class="ls-code"><input class="input" name="code" placeholder="Paste the code" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" enterkeyhint="go">
      <button type="submit" class="btn block primary">Connect</button></form>
    </div>`));
    const input = body.querySelector("input");
    body.querySelector(".ls-open").addEventListener("click", () => {
      openSignInPage(session.codeUrl);
      setTimeout(() => input.focus(), 400);
    });
    body.querySelector("form").addEventListener("submit", (e) => {
      e.preventDefault();
      const code = input.value.trim();
      if (!code) { input.focus(); return; }
      void finish({ code });
    });
  }

  function showDevice() {
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-step"><span class="ls-n">1</span><div>Open the sign-in page and sign in to ChatGPT.</div></div>
      <div class="ls-step"><span class="ls-n">2</span><div>Enter this code there. Tap it to copy.</div></div>
      <button type="button" class="ls-usercode" aria-label="Copy the code"></button>
      <button type="button" class="btn block accent ls-open">${icon("external")}<span>Copy code and open the page</span></button>
      <div class="ls-wait small"><span class="spin" aria-hidden="true"></span><div class="ls-note">Waiting for you to enter the code. It expires in 15 minutes.</div></div>
    </div>`));
    const codeBtn = body.querySelector(".ls-usercode");
    setText(codeBtn, session.userCode || "");
    codeBtn.addEventListener("click", async () => { if (await copyText(session.userCode)) toast("Code copied"); });
    body.querySelector(".ls-open").addEventListener("click", async () => {
      await copyText(session.userCode);
      openSignInPage(session.url);
    });
    // The machine finishes on its own; watch it.
    void (async () => {
      const id = session.id;
      while (!closed && session?.id === id) {
        await sleep(2000);
        if (closed || session?.id !== id) return;
        await refreshState();
        const login = store.state?.home?.logins?.[provider];
        if (!login || login.id !== id || LOGIN_BUSY.has(login.status)) continue;
        if (login.status === "done") return void succeeded();
        if (login.status !== "cancelled") return failed(login.error || "The sign-in did not finish.");
        return;
      }
    })();
  }

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest("[data-act=close]")) dlg.close();
  });
  dlg.addEventListener("close", () => {
    closed = true;
    dlg.remove();
    // Closing mid-way ends the run on the machine, so a later one starts clean.
    if (session && !ended) {
      void api("/api/app/login/cancel", { method: "POST", body: { provider, id: session.id } }).catch(() => {});
    }
    void refreshState();
  });
  document.body.appendChild(dlg);
  dlg.showModal();
  void begin();
}

/* ------------------------------------------------------------------ */
/* GitHub, computers, first-run setup                                  */
/* ------------------------------------------------------------------ */

// GitHub as the cloud machine will see it: the machine's own check when it
// has one, else whether the vault holds a token (it may be asleep).
function githubState(s) {
  const cred = s?.home?.status === "running" ? s.home.credentials?.github : null;
  if (cred?.ok) return { kind: "ok", sub: cred.account ? `Connected as ${cred.account}` : "Connected" };
  if (cred && cred.ok === false && s?.github?.connected) return { kind: "out", err: true, sub: `GitHub rejected the token (${cred.detail || "not signed in"}). Connect again.` };
  if (s?.github?.connected) return { kind: "ok", sub: "Connected" };
  return { kind: "out", sub: "So the machine can clone and push your repos" };
}

// GitHub's device flow, run by Rudder: the phone shows the code and opens
// github.com/login/device; the token goes to your cloud machine, not here.
function openGithubConnect() {
  const dlg = h(`<dialog class="sheet login-sheet" aria-label="Connect GitHub"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Connect GitHub</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="ls-body" aria-live="polite"></div>
  </div></dialog>`);
  const body = dlg.querySelector(".ls-body");
  let closed = false;
  let done = false;
  let flow = null;

  const failed = (message) => {
    body.replaceChildren(h(`<div class="ls-msg err"></div>`));
    setText(body.firstChild, message);
    const again = h(`<div class="ls-actions"><button type="button" class="btn block accent">Try again</button></div>`);
    again.querySelector("button").addEventListener("click", () => void begin());
    body.appendChild(again);
  };

  async function begin() {
    body.replaceChildren(h(`<div class="ls-wait"><span class="spin" aria-hidden="true"></span><div><div class="ls-title">Asking GitHub for a code</div></div></div>`));
    try {
      flow = await api("/api/app/github/start", { method: "POST", body: {} });
    } catch (err) {
      if (!closed && err.status !== 401) failed(err.message);
      return;
    }
    if (closed) return;
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-step"><span class="ls-n">1</span><div>Open GitHub and sign in if it asks.</div></div>
      <div class="ls-step"><span class="ls-n">2</span><div>Enter this code there, then tap <b>Authorize</b>. Tap the code to copy it.</div></div>
      <button type="button" class="ls-usercode" aria-label="Copy the code"></button>
      <button type="button" class="btn block accent ls-open">${icon("external")}<span>Copy code and open GitHub</span></button>
      <div class="ls-wait small"><span class="spin" aria-hidden="true"></span><div class="ls-note">Waiting for GitHub. Agents on your cloud machine will use this login to clone and push.</div></div>
    </div>`));
    const codeBtn = body.querySelector(".ls-usercode");
    setText(codeBtn, flow.userCode);
    codeBtn.addEventListener("click", async () => { if (await copyText(flow.userCode)) toast("Code copied"); });
    body.querySelector(".ls-open").addEventListener("click", async () => {
      await copyText(flow.userCode);
      openSignInPage(flow.verificationUri);
    });
    const mine = flow;
    while (!closed && flow === mine) {
      await sleep(2500);
      if (closed || flow !== mine) return;
      let res;
      try {
        res = await api("/api/app/github/poll", { method: "POST", body: {} });
      } catch (err) {
        if (err.status === 401) return;
        continue; // a dropped poll is retried; GitHub keeps the code
      }
      if (closed || flow !== mine) return;
      if (res.status === "done") {
        done = true;
        body.replaceChildren(h(`<div class="ls-done"><div class="ls-check">${icon("check")}</div><div class="ls-title">GitHub is connected</div><div class="ls-note"></div></div>`));
        setText(body.querySelector(".ls-note"), `Your cloud machine now signs in to GitHub as ${res.account}.`);
        toast("GitHub is connected");
        await refreshState();
        setTimeout(() => { if (!closed) dlg.close(); }, 1600);
        return;
      }
      if (res.status === "failed") return failed(res.error || "GitHub did not finish the sign-in.");
    }
  }

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest("[data-act=close]")) dlg.close();
  });
  dlg.addEventListener("close", () => {
    closed = true;
    dlg.remove();
    if (flow && !done) void api("/api/app/github/cancel", { method: "POST", body: {} }).catch(() => {});
  });
  document.body.appendChild(dlg);
  dlg.showModal();
  void begin();
}

const COMPUTER_CODE_RE = /^[A-Z0-9]{3}-?[A-Z0-9]{3}$/i;
const formatComputerCode = (value) => {
  const raw = String(value || "").toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 6);
  return raw.length > 3 ? `${raw.slice(0, 3)}-${raw.slice(3)}` : raw;
};

// `rudder login` on a computer shows a code (and a QR of /app/approve); this
// phone, already signed in, says yes or no to that computer.
function openComputerSignIn(prefill = "") {
  const dlg = h(`<dialog class="sheet login-sheet" aria-label="Sign in a computer"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Sign in a computer</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="ls-body" aria-live="polite"></div>
  </div></dialog>`);
  const body = dlg.querySelector(".ls-body");
  let closed = false;

  function askCode(value = "", error = "") {
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-step"><span class="ls-n">1</span><div>On the computer, run <code>rudder login</code>.</div></div>
      <div class="ls-step"><span class="ls-n">2</span><div>Enter the code it shows.</div></div>
      <form class="ls-code"><input class="input computer-code" name="code" placeholder="K7P-4QX" maxlength="7" autocomplete="off"
        autocapitalize="characters" autocorrect="off" spellcheck="false" enterkeyhint="go" aria-label="Code from rudder login">
      <div class="ls-msg err" hidden></div>
      <button type="submit" class="btn block primary">Continue</button></form>
    </div>`));
    const input = body.querySelector("input");
    const errEl = body.querySelector(".ls-msg");
    input.value = formatComputerCode(value);
    if (error) { setText(errEl, error); errEl.hidden = false; }
    input.addEventListener("input", () => { input.value = formatComputerCode(input.value); });
    body.querySelector("form").addEventListener("submit", (e) => {
      e.preventDefault();
      if (!COMPUTER_CODE_RE.test(input.value)) { setText(errEl, "The code is 6 letters and numbers, like K7P-4QX."); errEl.hidden = false; input.focus(); return; }
      void lookup(input.value);
    });
    setTimeout(() => { if (!closed && !input.value) input.focus(); }, 300);
  }

  async function lookup(code) {
    body.replaceChildren(h(`<div class="ls-wait"><span class="spin" aria-hidden="true"></span><div><div class="ls-title">Checking the code</div></div></div>`));
    let found;
    try {
      found = await api("/api/app/computer/lookup", { method: "POST", body: { code } });
    } catch (err) {
      if (!closed && err.status !== 401) askCode(code, err.message);
      return;
    }
    if (closed) return;
    const name = found.deviceName || "this computer";
    body.replaceChildren(h(`<div class="ls-steps">
      <div class="ls-title computer-ask"></div>
      <div class="ls-note computer-note"></div>
      <div class="ls-actions">
        <button type="button" class="btn block accent" data-v="1">Approve</button>
        <button type="button" class="btn block danger" data-v="0">Deny</button>
      </div>
    </div>`));
    setText(body.querySelector(".computer-ask"), `Sign in ${name}?`);
    setText(body.querySelector(".computer-note"),
      `Code ${found.code}. It gets your Rudder account${store.state?.email ? ` (${store.state.email})` : ""}: your cloud machine, projects and agents. Only approve a computer you are using right now.`);
    body.querySelectorAll("[data-v]").forEach((b) => b.addEventListener("click", () => void answer(found.code, name, b.dataset.v === "1")));
  }

  async function answer(code, name, approve) {
    body.querySelectorAll("button").forEach((b) => { b.disabled = true; });
    try {
      await api("/api/app/computer/approve", { method: "POST", body: { code, approve } });
    } catch (err) {
      if (!closed && err.status !== 401) askCode("", err.message);
      return;
    }
    if (closed) return;
    if (!approve) {
      body.replaceChildren(h(`<div class="ls-done"><div class="ls-title">Denied</div><div class="ls-note">That computer was not signed in, and the code no longer works.</div></div>`));
      setTimeout(() => { if (!closed) dlg.close(); }, 1600);
      return;
    }
    body.replaceChildren(h(`<div class="ls-done"><div class="ls-check">${icon("check")}</div><div class="ls-title"></div>
      <div class="ls-note">Go back to its terminal. <code>rudder cloud</code> in a repo there opens it on your cloud machine.</div></div>`));
    setText(body.querySelector(".ls-title"), `${name} is signed in`);
    toast(`${name} is signed in`);
  }

  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest("[data-act=close]")) dlg.close();
  });
  dlg.addEventListener("close", () => { closed = true; dlg.remove(); });
  document.body.appendChild(dlg);
  dlg.showModal();
  if (prefill && COMPUTER_CODE_RE.test(prefill)) void lookup(prefill);
  else askCode(prefill);
}

// The QR `rudder login` prints lands on /app/approve?code=...; a signed-out
// phone signs in first, so the code waits here (it lives 5 minutes anyway).
const APPROVE_KEY = "rudder.approveCode";
function takePendingApprove() {
  const pending = ls.get(APPROVE_KEY);
  ls.set(APPROVE_KEY, null);
  return pending && Date.now() - pending.at < 5 * 60_000 ? pending.code : null;
}

const INSTALL_LINE = "npm i -g @exla/rudder && rudder login";
function ComputerCard() {
  const el = h(`<div class="card computer">
    <div class="c-head"><div class="m-icon">${icon("terminal")}</div><div><div class="c-title">Use Rudder on your computer</div>
      <div class="c-sub">Same projects and agents, from a terminal. Install it, then sign in:</div></div></div>
    <button type="button" class="c-cmd" aria-label="Copy the install command"><code></code>${icon("copy")}</button>
    <div class="c-foot"><span><code>rudder login</code> shows a code. Approve it here.</span>
      <button type="button" class="btn small accent c-enter">Enter code</button></div>
  </div>`);
  setText(el.querySelector(".c-cmd code"), INSTALL_LINE);
  el.querySelector(".c-cmd").addEventListener("click", async () => {
    if (await copyText(INSTALL_LINE)) { toast("Copied"); track("install_cli_copied", { from: "home" }); }
  });
  el.querySelector(".c-enter").addEventListener("click", () => openComputerSignIn());
  return { el };
}

// A fresh account's three steps. Each row is done or not; the card goes away
// once all three are done (and stays away, so a sleeping machine's unknown
// logins never bring it back).
const SETUP_DONE_KEY = "rudder.setupDone";
function SetupCard() {
  const el = h(`<div class="section setup-sec">
    <div class="section-head"><h2>Get set up</h2><span class="count"></span></div>
    <div class="card accounts setup" role="list" aria-label="Setup steps"></div></div>`);
  const list = el.querySelector(".setup");
  const STEPS = [
    { id: "github", ic: "github", title: "Connect GitHub", action: "Connect", run: () => openGithubConnect() },
    { id: "agent", ic: "spark", title: "Sign in Claude Code or Codex", action: "Sign in", run: () => openAgentChooser() },
    { id: "repo", ic: "folder", title: "Pick or create a repo", action: "Add", run: () => openAddProject() },
  ];
  const rows = new Map();
  for (const step of STEPS) {
    const row = h(`<div class="acct" role="listitem">
      <div class="m-icon">${icon(step.ic)}</div>
      <div class="a-body"><div class="a-title"><span></span></div><div class="a-sub"></div></div>
      <span class="setup-check" hidden>${icon("check")}</span>
      <button type="button" class="btn small accent a-btn"></button>
    </div>`);
    setText(row.querySelector(".a-title span"), step.title);
    setText(row.querySelector(".a-btn"), step.action);
    row.querySelector(".a-btn").addEventListener("click", step.run);
    list.appendChild(row);
    rows.set(step.id, row);
  }
  function states(s) {
    const home = s?.home;
    const agents = AGENTS.map((x) => agentLoginState(home, x.id));
    const signedIn = agents.findIndex((st) => st.kind === "ok");
    let agent;
    if (signedIn >= 0) agent = { kind: "ok", sub: `${AGENTS[signedIn].label} · ${agents[signedIn].sub}` };
    else if (agents.some((st) => st.kind === "busy")) agent = { kind: "busy", sub: "Signing in…" };
    else if (home && !home.credentials) agent = { kind: "unknown", sub: home.connected ? "Checking…" : "Checked when your cloud machine is up" };
    else agent = { kind: "out", sub: "Agents run with your own subscription or API key" };
    const projects = s?.projects || [];
    const repo = projects.length
      ? { kind: "ok", sub: projects.length === 1 ? projects[0].repo || projects[0].name : `${projects.length} projects` }
      : { kind: "out", sub: "It's cloned onto your cloud machine" };
    return { github: githubState(s), agent, repo };
  }
  function update(s) {
    if (ls.get(SETUP_DONE_KEY)) { el.hidden = true; return; }
    const st = states(s);
    const all = Object.values(st);
    if (all.every((x) => x.kind === "ok")) { ls.set(SETUP_DONE_KEY, true); el.hidden = true; return; }
    // Nothing known to be missing (a sleeping machine): nothing to show.
    el.hidden = !all.some((x) => x.kind === "out");
    const doneCount = all.filter((x) => x.kind === "ok").length;
    setText(el.querySelector(".count"), `${doneCount} of ${all.length}`);
    for (const [id, row] of rows) {
      const x = st[id];
      const sub = row.querySelector(".a-sub");
      setText(sub, x.sub);
      sub.classList.toggle("err", !!x.err);
      row.classList.toggle("done", x.kind === "ok");
      row.querySelector(".setup-check").hidden = x.kind !== "ok";
      row.querySelector(".a-btn").hidden = x.kind === "ok" || x.kind === "unknown";
    }
  }
  return { el, update };
}

// Claude Code or Codex: whichever the user pays for.
function openAgentChooser() {
  const dlg = h(`<dialog class="sheet" aria-label="Sign in an agent"><div class="dialog">
    <div class="sheet-grab"></div>
    <div class="add-head"><h3>Which agent do you use?</h3><button type="button" class="icon-btn" data-act="close" aria-label="Close">${icon("x")}</button></div>
    <div class="ls-actions"></div>
    <p class="hint">You can sign in the other one later from the machine card.</p>
  </div></dialog>`);
  const actions = dlg.querySelector(".ls-actions");
  for (const agent of AGENTS) {
    const b = h(`<button type="button" class="btn block">${icon(agent.ic)}<span></span></button>`);
    setText(b.querySelector("span"), `${agent.label} (${agent.account} account)`);
    b.addEventListener("click", () => { dlg.close(); openAgentSignIn(agent.id); });
    actions.appendChild(b);
  }
  dlg.addEventListener("click", (e) => {
    if (e.target === dlg || e.target.closest("[data-act=close]")) dlg.close();
  });
  dlg.addEventListener("close", () => dlg.remove());
  document.body.appendChild(dlg);
  dlg.showModal();
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
  // Arrived from the QR `rudder login` printed: say why signing in comes first.
  const waiting = ls.get(APPROVE_KEY);
  if (waiting?.code && Date.now() - waiting.at < 5 * 60_000) {
    const note = h(`<p class="approve-note"></p>`);
    setText(note, `Sign in to approve your computer (code ${waiting.code}).`);
    el.querySelector(".signin p").after(note);
  }
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
    const g = h(`<button type="button" class="btn primary block">${icon("google")}<span>Continue with Google</span></button>`);
    // An invite or Slack link comes back after sign-in (?next=, plus the
    // localStorage copy for servers that ignore it).
    const next = parseRoute(location.pathname).needsAccount ? location.pathname : "";
    const loginUrl = (q) => { const u = new URLSearchParams(q); if (next) u.set("next", next); const s = u.toString(); return `/app/login${s ? `?${s}` : ""}`; };
    g.addEventListener("click", () => { location.href = loginUrl({}); });
    const gh = h(`<div class="alt"><button type="button">Use GitHub instead</button></div>`);
    gh.querySelector("button").addEventListener("click", () => { location.href = loginUrl({ provider: "github" }); });
    // Signing in needs only Google or GitHub; a code from `rudder cloud app`
    // is the optional shortcut, so it waits behind a link.
    const have = h(`<div class="alt"><button type="button">Have a code from <code>rudder cloud app</code>?</button></div>`);
    have.querySelector("button").addEventListener("click", () => {
      have.replaceWith(pair);
      pair.querySelector("input").focus();
    });
    actions.append(g, gh, have);
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
    approvePendingComputer();
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
      <div class="section digest-slot"></div>
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
      <div class="section computer-slot"></div>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  const acts = el.querySelector(".tb-actions");
  // Talk to Rudder: the iOS shell (RUDDER_NATIVE.voice) runs the voice
  // assistant natively, so the button only exists there.
  if (NATIVE?.voice) {
    const micBtn = h(`<button type="button" class="icon-btn" aria-label="Talk to Rudder"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="9" y="3" width="6" height="11" rx="3"/><path d="M5 11a7 7 0 0 0 14 0"/><path d="M12 18v3"/></svg></button>`);
    micBtn.addEventListener("click", () => postNative({ type: "voice" }));
    acts.appendChild(micBtn);
  }
  acts.appendChild(refreshButton(refreshState));
  // Team: what your teammates' agents are doing (team.js), once you have one.
  const teamBtn = h(`<a class="icon-btn" href="/app/team" data-link aria-label="Team" title="Team" hidden>${icon("users")}</a>`);
  acts.appendChild(teamBtn);
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
  // First run: the three steps lead, above the projects.
  const setup = SetupCard();
  setup.el.hidden = true;
  scroller.insertBefore(setup.el, projectsSec);
  const computerSlot = el.querySelector(".computer-slot");
  computerSlot.appendChild(ComputerCard().el);
  const digestCard = DigestCard();
  el.querySelector(".digest-slot").appendChild(digestCard.el);

  // Machine card (built once)
  const machine = h(`<div class="card machine">
      <div class="m-icon">${icon("server")}</div>
      <div class="m-body"><div class="m-title"><span>Cloud machine</span><span class="pill"><span class="dot"></span><span class="pl"></span></span></div>
      <div class="m-sub"></div></div>
      <button type="button" class="btn small accent wake" hidden>${icon("power")}<span>Wake</span></button>
    </div>`);
  const wakeBtn = machine.querySelector(".wake");
  // Who Claude Code and Codex sign in as on the machine, and Sign in buttons.
  const accounts = AccountsCard();
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
    teamBtn.hidden = !hasTeam(s);
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
    setup.update(s);
    digestCard.update(s);

    // Machine
    const home = s.home;
    if (home) {
      if (machineSlot.firstElementChild !== machine) machineSlot.replaceChildren(machine, accounts.el);
      accounts.update(home);
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
        // Only a tailnet the machine actually joined is worth a word, and the
        // full *.ts.net name is too long for a phone (Settings shows it).
        if (home.tailnet?.online && home.tailnet?.dnsName) bits.push(`on Tailscale as ${home.tailnet.dnsName.split(".")[0]}`);
        bits.push(`${home.activeAgents || 0} active ${home.activeAgents === 1 ? "agent" : "agents"}`);
        // It is not the user's computer: say where it runs and why that matters.
        subText = `Runs in the cloud, so agents keep going with your laptop closed · ${bits.join(" · ")}`;
      }
      // A GitHub login the cloud machine can't use stops agents from cloning or
      // pushing: say so before anyone starts a task. Claude Code and Codex have
      // their own rows below, with a Sign in button.
      const github = home.status === "running" ? home.credentials?.github : null;
      // Never connected at all is a setup step (the Get set up card), not a fault.
      const githubBroken = !!github && github.ok === false && s.github?.connected !== false;
      if (githubBroken) {
        subText = `GitHub can't sign in on this machine (${github.detail || "not logged in"}). Connect it again from Settings.`;
      }
      setText(sub, subText);
      sub.classList.toggle("err", home.status === "failed" || githubBroken);
      const trouble = home.status === "failed" || githubBroken || agentLoginTrouble(home);
      // Projects lead; the machine card only comes up above them while it is
      // the reason nothing works.
      const anchor = trouble ? projectsSec : computerSlot;
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
        : "Your agents run on your own cloud machine, so they keep going with your phone locked and your laptop closed. Create it, then sign in to Claude Code or Codex.";
      const key = home ? "h" : "n";
      if (!emptyEl || emptyEl.dataset.k !== key) {
        emptyEl = emptyState({ ic: home ? "folder" : "server", title: home ? "No projects yet" : "Welcome to Rudder", body, action: home ? null : createMachineButton() });
        emptyEl.dataset.k = key;
      }
      if (emptyEl.parentNode !== projectsBody) projectsBody.appendChild(emptyEl);
    } else if (emptyEl) { emptyEl.remove(); emptyEl = null; }
    renderAdd();
  }

  return { el, update };
}

// A new account has no machine: creating one is the first step, from here.
function createMachineButton() {
  const btn = h(`<button type="button" class="btn accent">${icon("server")}<span>Create my cloud machine</span></button>`);
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    setText(btn.querySelector("span"), "Creating…");
    try {
      const res = await api("/api/app/wake", { method: "POST", body: {} });
      if (res?.home && store.state) store.state.home = res.home;
      toast("Creating your cloud machine. The first start takes a minute or two.");
    } catch (err) {
      if (err.status !== 401) toast(err.message, { error: true });
      btn.disabled = false;
      setText(btn.querySelector("span"), "Create my cloud machine");
    }
    await refreshState();
  });
  return btn;
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
/* Morning digest: what the agents did since the last one, by project  */
/* ------------------------------------------------------------------ */

const clockTime = (ms) => new Date(ms).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
function digestDayLabel(day) {
  // `day` is the account's local YYYY-MM-DD; noon keeps it on the same date here.
  const ms = Date.parse(`${day}T12:00:00`);
  return Number.isFinite(ms) ? dayLabel(ms) : day;
}
function digestSpan(d) {
  if (!d?.since) return "";
  return `${dayLabel(d.since)} ${clockTime(d.since)} to ${dayLabel(d.until) === dayLabel(d.since) ? "" : `${dayLabel(d.until)} `}${clockTime(d.until)}`;
}
const DIGEST_EVENT = {
  started: "Started",
  message: "You asked",
  finished: "Finished",
  merged: "Merged",
  pushed: "Pushed",
  deployed: "Deployed",
  failed: "Failed",
  stopped: "Stopped",
  conflict: "Hit a merge conflict",
  working: "Working",
  updated: "Active",
};
const DIGEST_EVENT_CLS = { finished: "s-done", merged: "s-accent", pushed: "s-accent", deployed: "s-done", failed: "s-failed", conflict: "s-needs", message: "s-idle" };
const DIGEST_STATUS = { ...STATUS, deployed: { label: "Deployed", cls: "s-done" } };

function digestStats(t) {
  const out = [];
  if (t.finished) out.push(["s-done", t.finished, "finished"]);
  if (t.merged) out.push(["s-accent", t.merged, "merged"]);
  if (t.deployed) out.push(["s-done", t.deployed, "deployed"]);
  if (t.prs) out.push(["s-accent", t.prs, t.prs === 1 ? "PR" : "PRs"]);
  if (t.needsYou) out.push(["s-needs", t.needsYou, t.needsYou === 1 ? "needs you" : "need you"]);
  if (t.working) out.push(["s-working", t.working, "working"]);
  if (t.failed) out.push(["s-failed", t.failed, "failed"]);
  return out.map(([cls, n, label]) => `<span class="stat ${cls}"><span class="dot"></span><b>${n}</b> ${escapeHtml(label)}</span>`).join("");
}

// First thing on the home screen: the newest digest, or when the first one comes.
function DigestCard() {
  const el = h(`<a class="card digest-card tap" data-link href="/app/digest">
      <div class="d-top"><span class="d-icon">${icon("sun")}</span><span class="d-kicker"></span><span class="dot d-unread s-accent"></span><span class="d-chev">${icon("chevron")}</span></div>
      <div class="d-head"></div>
      <div class="d-sub"></div>
      <div class="p-stats d-stats"></div>
    </a>`);
  function update(s) {
    const d = s?.digest;
    el.hidden = !s?.home && !d;
    if (!d) {
      setText(el.querySelector(".d-kicker"), "Morning digest");
      setText(el.querySelector(".d-head"), "Your first digest arrives at 9:01 AM.");
      setText(el.querySelector(".d-sub"), "Tap to see what your agents have done so far.");
      el.querySelector(".d-stats").innerHTML = "";
      el.querySelector(".d-unread").hidden = true;
      return;
    }
    setText(el.querySelector(".d-kicker"), `Morning digest · ${digestDayLabel(d.day)}`);
    setText(el.querySelector(".d-head"), d.headline);
    const projects = (d.projects || []).join(" · ");
    setText(el.querySelector(".d-sub"), [projects, ...(d.lines || [])].filter(Boolean).join("  ·  "));
    el.querySelector(".d-stats").innerHTML = digestStats(d.totals || {});
    el.querySelector(".d-unread").hidden = !!d.readAt;
    el.setAttribute("aria-label", `Morning digest, ${digestDayLabel(d.day)}: ${d.headline}`);
  }
  return { el, update };
}

// `id`: a kept digest, null for the newest one, "live" for since the last one.
function DigestScreen(id) {
  const el = h(`<section class="screen digest-screen">
    <header class="topbar">
      <a class="icon-btn back" href="/app" aria-label="Back to projects">${icon("back")}</a>
      <div class="titles"><div class="title">Digest</div><div class="subtitle"><span class="span"></span></div></div>
    </header>
    <div class="scroller digest">
      <div class="ptr">${icon("down")}</div>
      <div class="d-days"></div>
      <div class="d-body"></div>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  const body = el.querySelector(".d-body");
  const daysEl = el.querySelector(".d-days");
  el.querySelector(".back").addEventListener("click", (e) => { e.preventDefault(); goBack("/app"); });
  let destroyed = false;
  let current = null;
  let days = [];
  body.innerHTML = `<div class="sk sk-card" style="height:120px"></div><div class="sk sk-card" style="height:180px"></div><div class="sk sk-card" style="height:180px;opacity:.6"></div>`;

  async function load() {
    try {
      const [list, digest] = await Promise.all([
        api("/api/app/digests").catch(() => ({ digests: [] })),
        id === "live" ? api("/api/app/digest?live=1") : api(id ? `/api/app/digest?id=${encodeURIComponent(id)}` : "/api/app/digest").catch((err) => {
          // No digest kept yet: show what has happened so far instead.
          if (err.status === 404) return api("/api/app/digest?live=1");
          throw err;
        }),
      ]);
      if (destroyed) return;
      days = list?.digests || [];
      current = digest;
      paint();
      if (digest?.id && !digest.readAt) {
        api("/api/app/digest/read", { method: "POST", body: { id: digest.id } }).then(() => {
          if (store.state?.digest?.id === digest.id) { store.state.digest.readAt = new Date().toISOString(); }
        }).catch(() => {});
      }
    } catch (err) {
      if (destroyed || err.status === 401) return;
      if (err.waking) {
        body.replaceChildren(emptyState({ ic: "server", title: "Waking your cloud machine", body: "The digest is read from your agents' records there. This takes about a minute." }));
        setTimeout(() => { if (!destroyed) load(); }, 5000);
        return;
      }
      body.replaceChildren(emptyState({ ic: "alert", title: "Can't load the digest", body: escapeHtml(err.message) }));
    }
  }

  function paintDays() {
    const items = [{ id: "live", label: "So far today" }, ...days.map((d) => ({ id: d.id, label: digestDayLabel(d.day), unread: !d.readAt }))];
    const active = current?.live ? "live" : current?.id;
    reconcile(daysEl, items, (x) => x.id,
      () => h(`<a class="chip d-day tap" data-link></a>`),
      (a, x) => {
        a.href = digestPath(x.id);
        a.classList.toggle("on", x.id === active);
        a.classList.toggle("unread", !!x.unread && x.id !== active);
        setText(a, x.label);
      });
  }

  function paint() {
    const d = current;
    paintDays();
    setText(el.querySelector(".title"), d.live ? "So far today" : `Digest · ${digestDayLabel(d.day)}`);
    setText(el.querySelector(".span"), digestSpan(d));
    const hero = h(`<div class="card d-hero">
        <div class="d-top"><span class="d-icon">${icon("sun")}</span><span class="d-kicker"></span></div>
        <div class="d-head"></div><div class="d-lines"></div><div class="p-stats d-stats"></div></div>`);
    setText(hero.querySelector(".d-kicker"), d.live ? "Since the last digest" : `Delivered ${clockTime(d.generatedAt || d.until)}`);
    setText(hero.querySelector(".d-head"), d.headline);
    hero.querySelector(".d-lines").innerHTML = (d.lines || []).map((l) => `<div>${escapeHtml(l)}</div>`).join("");
    hero.querySelector(".d-stats").innerHTML = digestStats(d.totals || {});
    const parts = [hero];
    const reminder = digestReminderCard();
    if (reminder) parts.push(reminder);
    for (const p of d.projects || []) parts.push(digestProjectSection(p));
    if (!(d.projects || []).length) {
      parts.push(emptyState({ ic: "sun", title: "Nothing ran", body: "No agent did anything in this window. Start a task in a project and it shows up here tomorrow morning." }));
    }
    body.replaceChildren(...parts);
  }

  topbarScrollShadow(scroller, bar);
  pullToRefresh(scroller, load);
  load();
  return { el, update() { if (current) paintDays(); }, destroy() { destroyed = true; } };
}

// The iOS shell can remind at 9:01 (a local notification, DigestReminder.swift).
const REMINDER_KEY = "rudder.digestReminder";
function digestReminderCard() {
  if (!NATIVE?.digest || ls.get(REMINDER_KEY) === "on") return null;
  const denied = ls.get(REMINDER_KEY) === "denied";
  const el = h(`<div class="card machine d-remind">
      <div class="m-icon">${icon("sun")}</div>
      <div class="m-body"><div class="m-title"><span>Get this at 9:01 every morning</span></div>
      <div class="m-sub"></div></div>
      <button type="button" class="btn small accent">Turn on</button></div>`);
  setText(el.querySelector(".m-sub"), denied
    ? "Notifications are off for Rudder. Turn them on in Settings, then tap again."
    : "A notification when your digest is ready.");
  el.querySelector("button").addEventListener("click", () => postNative({ type: "digestReminder", enabled: true }));
  return el;
}
window.rudderDigestReminder = ({ enabled, denied } = {}) => {
  ls.set(REMINDER_KEY, enabled ? "on" : denied ? "denied" : null);
  toast(enabled ? "You'll get your digest at 9:01 every morning" : denied ? "Notifications are off for Rudder in Settings" : "Couldn't schedule the reminder", { error: !enabled });
  document.querySelector(".d-remind")?.remove();
  if (!enabled) { const card = digestReminderCard(); if (card) document.querySelector(".d-hero")?.after(card); }
};

function digestProjectSection(p) {
  const sec = h(`<div class="section d-proj">
      <a class="section-head d-proj-head tap" data-link><h2></h2><span class="count"></span>${icon("chevron")}</a>
      <div class="d-repo"></div>
      <div class="list grouped card d-runs"></div>
      <details class="d-tl"><summary>Timeline</summary><ol class="d-events"></ol></details>
    </div>`);
  const head = sec.querySelector(".d-proj-head");
  head.href = projectPath(p.workspaceId);
  setText(sec.querySelector("h2"), p.name);
  setText(sec.querySelector(".count"), `${p.totals.agents} ${p.totals.agents === 1 ? "agent" : "agents"}`);
  setText(sec.querySelector(".d-repo"), p.repo || "");
  const runsEl = sec.querySelector(".d-runs");
  for (const r of p.runs || []) runsEl.appendChild(digestRunRow(p, r));
  // Over time: every event in this project, oldest first, a day header when the date turns.
  const ol = sec.querySelector(".d-events");
  let lastDay = null;
  for (const e of p.timeline || []) {
    const day = dayLabel(e.at);
    if (day !== lastDay) { ol.appendChild(h(`<li class="d-ev-day">${escapeHtml(day)}</li>`)); lastDay = day; }
    const li = h(`<li class="d-ev ${DIGEST_EVENT_CLS[e.kind] || "s-working"}"><span class="d-ev-time"></span><span class="dot"></span><a class="d-ev-body" data-link><b></b> <span class="d-ev-title"></span><span class="d-ev-text"></span></a></li>`);
    setText(li.querySelector(".d-ev-time"), clockTime(e.at));
    setText(li.querySelector("b"), DIGEST_EVENT[e.kind] || e.kind);
    setText(li.querySelector(".d-ev-title"), e.title);
    setText(li.querySelector(".d-ev-text"), e.text ? (e.kind === "message" ? `"${e.text}"` : e.text) : "");
    li.querySelector(".d-ev-body").href = agentPath(p.workspaceId, e.runId);
    ol.appendChild(li);
  }
  // A short timeline reads at a glance; a long one waits to be opened.
  if ((p.timeline || []).length <= 8) sec.querySelector(".d-tl").open = true;
  return sec;
}

function digestRunRow(p, r) {
  const st = DIGEST_STATUS[r.status] || statusOf(r.status);
  const el = h(`<a class="row agent tap d-run ${st.cls}" data-link>
      <span class="a-dot"><span class="dot"></span></span>
      <div class="a-body">
        <div class="a-top"><span class="a-title"></span><span class="a-time"></span></div>
        <div class="a-meta"><span class="status-text"></span><span class="chip c-model"></span></div>
        <div class="a-msg d-did"></div>
        <div class="d-badges"></div>
      </div></a>`);
  el.href = agentPath(p.workspaceId, r.id);
  el.querySelector(".dot").classList.toggle("live", r.status === "working");
  setText(el.querySelector(".a-title"), r.title || r.task || "Untitled agent");
  setText(el.querySelector(".a-time"), r.firstAt === r.lastAt ? clockTime(r.lastAt) : `${clockTime(r.firstAt)} to ${clockTime(r.lastAt)}`);
  setText(el.querySelector(".status-text"), st.label);
  const model = [BACKEND[r.backend] || r.backend, r.model].filter(Boolean).join(" · ");
  const chip = el.querySelector(".c-model");
  setText(chip, model); chip.hidden = !model;
  const did = el.querySelector(".d-did");
  setText(did, r.summary || r.task || ""); did.hidden = !(r.summary || r.task);
  const badges = [];
  if (r.merged) badges.push(`<span class="chip s-accent">${icon("merge")} ${r.pushed ? "Merged and pushed" : "Merged"}</span>`);
  if (r.pr?.url) badges.push(`<button type="button" class="chip s-accent d-pr" data-url="${escapeHtml(r.pr.url)}">${icon("github")} PR${r.pr.number ? ` #${r.pr.number}` : ""}</button>`);
  if (r.delivery) badges.push(`<span class="chip s-done">${icon("check")} Deployed${r.delivery.target ? ` to ${escapeHtml(r.delivery.target)}` : ""}</span>`);
  const b = el.querySelector(".d-badges");
  b.innerHTML = badges.join("");
  b.hidden = !badges.length;
  b.querySelector(".d-pr")?.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); openExternal(e.currentTarget.dataset.url); });
  return el;
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
  const el = h(`<section class="screen project-screen">
    <header class="topbar">
      <a class="icon-btn back" href="/app" aria-label="Back to projects">${icon("back")}</a>
      <div class="titles"><div class="title"></div><div class="subtitle"><span class="repo"></span></div></div>
    </header>
    <div class="scroller has-composer">
      <div class="ptr">${icon("down")}</div>
      <div class="status-slot"></div>
      <div class="plan-slot"></div>
      <div class="section ports-sec" hidden><div class="section-head"><h2>Previews</h2></div><div class="ports"></div></div>
      <div class="groups"></div>
      <div class="p-empty"></div>
    </div></section>`);
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  el.querySelector(".back").addEventListener("click", (e) => { e.preventDefault(); goBack("/app"); });
  // The model new agents start on, in the header too (the task box has its chip).
  const headModel = modelChip(ws, { compact: true });
  headModel.el.classList.add("head-model");
  bar.appendChild(headModel.el);
  bar.appendChild(refreshButton(refreshState));
  topbarScrollShadow(scroller, bar);
  pullToRefresh(scroller, refreshState);

  const composer = createComposer({
    placeholder: "Start a new task",
    draftKey: `task:${ws}`,
    hint: coarse ? "" : "Enter to start · Shift+Enter for a new line",
    attachments: true,
    onSubmit: async (text, files = []) => {
      if (/^\/model\s*$/i.test(text) && !files.length) {
        openModelPicker(ws, { onPicked: syncModel });
        return true;
      }
      if (!text) { toast("Say what the agent should do with the files"); return false; }
      try {
        const paths = [];
        for (const [i, f] of files.entries()) {
          if (files.length) toast(files.length > 1 ? `Uploading ${i + 1} of ${files.length}` : "Uploading");
          paths.push(await uploadAttachment(ws, f.file));
        }
        const res = await api("/api/app/task", { method: "POST", body: { workspace: ws, text: withAttachments(text, paths) } });
        toast(text.startsWith("/") ? "Sent to the dashboard" : "Task started");
        if (!text.startsWith("/")) track("task_started", { source: "app", files: files.length });
        else if (/^\/model\s/i.test(text)) syncModel();
        await refreshState();
        const run = res?.run || res?.runId;
        if (run) navigate(agentPath(ws, run));
        return true;
      } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); return false; }
    },
  });
  el.appendChild(composer.el);
  // Above the task box: the model new agents start on, and Linear issues
  // when Linear is connected.
  const tools = h(`<div class="c-tools"></div>`);
  const taskModel = modelChip(ws);
  const syncModel = () => { taskModel.refresh(); headModel.refresh(); };
  tools.appendChild(taskModel.el);
  const linearBtn = h(`<button type="button" class="tool-chip" hidden><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="8.5"/><path d="M5 14.5l4.5 4.5M4 10.5L13.5 20M6 7.5L16.5 18M8.5 5L19 15.5"/></svg><span>From Linear</span></button>`);
  linearBtn.addEventListener("click", () => openLinearIssues(ws, { onStarted: () => refreshState() }));
  tools.appendChild(linearBtn);
  linearAvailable().then((ok) => { linearBtn.hidden = !ok; });
  // "New agents here are private" (team.ts): only once you have a team.
  const privBtn = h(`<button type="button" class="tool-chip priv-chip" hidden aria-pressed="false"></button>`);
  let privOverride = null; // what this device just set, until the state agrees
  const paintPriv = (p) => {
    privBtn.hidden = !p || !hasTeam(store.state);
    if (privBtn.hidden) return;
    if (privOverride !== null && !!p.newAgentsPrivate === privOverride) privOverride = null;
    const on = privOverride ?? !!p.newAgentsPrivate;
    const sig = String(on);
    if (privBtn.dataset.sig === sig) return;
    privBtn.dataset.sig = sig;
    privBtn.innerHTML = `${icon(on ? "lock" : "users")}<span>${on ? "New agents private" : "Team sees new agents"}</span>`;
    privBtn.classList.toggle("on", on);
    privBtn.setAttribute("aria-pressed", String(on));
    privBtn.title = on ? "New agents in this project are private. Click to let your team see new ones" : "Your team sees new agents in this project. Click to make new ones private";
  };
  privBtn.addEventListener("click", async () => {
    const p = findProject(ws);
    if (!p || privBtn.disabled) return;
    const next = !(privOverride ?? !!p.newAgentsPrivate);
    privBtn.disabled = true;
    try {
      await api("/api/app/visibility", { method: "POST", body: { workspace: ws, newAgentsPrivate: next } });
      privOverride = next;
      paintPriv(p);
      toast(next ? "New agents in this project start private" : "Your team sees new agents in this project");
      refreshState();
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { privBtn.disabled = false; }
  });
  tools.appendChild(privBtn);
  composer.el.querySelector("form").before(tools);
  // On a phone the plan is a section of this screen; on desktop it has a column.
  const planSlot = el.querySelector(".plan-slot");
  const planCard = PlanView({ ws, hrefFor: (run) => agentPath(ws, run), variant: "card" });
  planSlot.appendChild(planCard.el);

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
    paintPriv(p);
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

    planSlot.hidden = isDesktop();
    if (!isDesktop()) planCard.update(p);

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

function AgentScreen(ws, run, { member = null } = {}) {
  // `member`: a teammate's agent (team.js), read only. No composer, prompts,
  // terminal or actions, and its transcript comes through the team route.
  const ro = !!member;
  const key = ro ? `team|${member}|${ws}|${run}` : `${ws}|${run}`;
  const agentNow = () => (ro ? teamAgent(member, ws, run)?.agent : findAgent(ws, run)) || null;
  track("agent_panel_opened", { layout: isDesktop() ? "desktop" : "phone" });
  const el = h(`<section class="screen">
    <header class="topbar">
      <a class="icon-btn back" aria-label="Back to project">${icon("back")}</a>
      <div class="titles"><div class="title"></div>
        <div class="subtitle"><span class="dot"></span><span class="st"></span><span class="sep">·</span><span class="bk"></span></div></div>
      ${canOpenOnMac() ? `<button type="button" class="icon-btn mac-copy" aria-label="Open a copy on this Mac" title="Open a copy on this Mac">${MAC_ICON}</button>` : ""}
      <button type="button" class="icon-btn lock-toggle" hidden aria-pressed="false"></button>
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
  const backTo = ro ? teamPath() : projectPath(ws);
  back.href = backTo;
  back.setAttribute("aria-label", ro ? "Back to team" : "Back to project");
  back.addEventListener("click", (e) => { e.preventDefault(); goBack(backTo); });
  if (ro) for (const n of el.querySelectorAll(".mac-copy, .term-toggle, .more")) n.hidden = true;
  topbarScrollShadow(scroller, bar);

  const composer = createComposer({
    placeholder: "Message the agent",
    draftKey: `msg:${key}`,
    hint: coarse ? "" : "Enter to send · Shift+Enter for a new line",
    attachments: true,
    onSubmit: (text, files) => { send(text, files); return true; },
  });
  el.querySelector(".chat-wrap").after(composer.el);
  // Read only: whose agent this is, where the composer would be.
  const roBar = ro ? h(`<div class="composer ro-bar" role="note"><div class="rb-in"><span class="rb-ic">${icon("eye")}</span><span class="rb-text"><span class="rb-t"></span><span class="rb-s"></span></span></div></div>`) : null;
  if (ro) {
    composer.el.hidden = true;
    composer.el.after(roBar);
  }
  const paintRoBar = () => {
    if (!ro) return;
    const m = teamAgent(member, ws, run)?.member;
    setText(roBar.querySelector(".rb-t"), `${m ? memberName(m) : "A teammate"}'s agent · Read only`);
    setText(roBar.querySelector(".rb-s"), `You can read along. Only ${m ? firstName(m) : "they"} can message it, answer it or change it.`);
  };

  // Your own agent: private hides it from teammates (team.ts). Shown once
  // you are in an org, since before that nobody else could see it anyway.
  const lockBtn = el.querySelector(".lock-toggle");
  const paintLock = () => {
    const a = agentNow();
    const show = !ro && hasTeam(store.state) && !!a && !a.pending;
    lockBtn.hidden = !show;
    if (!show) return;
    const priv = a.visibility === "private";
    const sig = String(priv);
    if (lockBtn.dataset.sig === sig) return;
    lockBtn.dataset.sig = sig;
    lockBtn.innerHTML = icon(priv ? "lock" : "unlock");
    lockBtn.classList.toggle("on", priv);
    lockBtn.setAttribute("aria-pressed", String(priv));
    lockBtn.setAttribute("aria-label", priv ? "Private: only you can see this agent. Share it with your team" : "Your team can see this agent. Make it private");
    lockBtn.title = priv ? "Private. Only you can see it" : "Visible to your team. Click to make it private";
  };
  lockBtn.addEventListener("click", async () => {
    const a = agentNow();
    if (!a || lockBtn.disabled) return;
    const next = a.visibility === "private" ? "team" : "private";
    lockBtn.disabled = true;
    try {
      await api("/api/app/visibility", { method: "POST", body: { workspace: ws, run, visibility: next } });
      a.visibility = next;
      postNative({ type: "haptic" });
      toast(next === "private" ? "Private. Teammates see only its status" : "Your team can see this agent again");
      paintLock();
      refreshState();
    } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
    finally { lockBtn.disabled = false; }
  });

  // Pinned above the composer: the oldest prompt the agent is blocked on.
  const askSlot = h(`<div class="ask-slot"></div>`);
  composer.el.before(askSlot);
  let askCard = null;
  function paintAsk() {
    if (ro) return;
    const asks = pendingAsks(agentNow());
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
  if (!ro) fastStatePoll = () => {
    const a = agentNow();
    return !!a && (isRunning(a.status) || pendingAsks(a).length > 0);
  };
  if (!ro) pollState();

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
    const backend = agentNow()?.backend === "codex" ? "codex" : "claude";
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
  const resizeObs = new ResizeObserver(() => { if (atBottom) toBottom(false); });
  resizeObs.observe(scroller);

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
    const compactRow = e.target.closest(".compact-row");
    if (compactRow) {
      const node = compactRow.closest(".compact");
      const k = `${key}|${node.dataset.key}`;
      if (store.expanded.has(k)) store.expanded.delete(k); else store.expanded.add(k);
      paintCompact(node, node._msg);
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
      if (item) { store.pending.set(key, list.filter((x) => x !== item)); send(item.text, [], item); }
    }
  });

  // Mac app: pull a copy of this agent into the local clone.
  const copyHere = () => {
    const p = findProject(ws);
    const a = agentNow();
    openCopyOnMac({ runId: run, repo: p?.repo || null, title: a?.title || store.transcripts.get(key)?.title || null });
  };
  el.querySelector(".mac-copy")?.addEventListener("click", copyHere);

  // Overflow menu
  const moreBtn = el.querySelector(".more");
  let menuEl = null;
  function closeMenu() { menuEl?.remove(); menuEl = null; moreBtn.setAttribute("aria-expanded", "false"); }
  moreBtn.addEventListener("click", () => {
    if (menuEl) return closeMenu();
    const a = agentNow();
    const status = a?.status || store.transcripts.get(key)?.status;
    const canStop = !["stopped", "merged", "failed", "orphaned"].includes(status);
    const canMerge = status === "done" || status === "review";
    menuEl = h(`<div><div class="scrim"></div><div class="menu" role="menu"></div></div>`);
    const menu = menuEl.querySelector(".menu");
    const add = (html, fn) => { const b = h(html); b.addEventListener("click", () => { closeMenu(); fn(); }); menu.appendChild(b); };
    if (canMerge) add(`<button type="button" role="menuitem">${icon("merge")}<span>Merge into main</span></button>`, () => act("merge"));
    if (a?.remoteUrl) add(`<button type="button" role="menuitem">${icon("external")}<span>Open in Claude</span></button>`, () => openExternal(a.remoteUrl));
    if (canOpenOnMac() && findProject(ws)?.repo) add(`<button type="button" role="menuitem">${MAC_ICON}<span>Open a copy on this Mac</span></button>`, copyHere);
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
    const a = agentNow();
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
  async function send(text, files = [], retryOf = null) {
    const item = retryOf || { id: `pending-${Date.now()}-${seq++}`, text, ts: Date.now(), files: files.map((f) => ({ ...f })) };
    item.state = item.files?.some((f) => !f.path) ? "uploading" : "sending";
    store.pending.set(key, [...(store.pending.get(key) || []), item]);
    atBottom = true;
    paint();
    toBottom(true);
    try {
      // Files first, each once: a retry after a failed send reuses the paths.
      for (const f of item.files || []) {
        if (!f.path) { f.path = await uploadAttachment(ws, f.file); paint(); }
      }
      item.sentText = withAttachments(item.text, (item.files || []).map((f) => f.path));
      item.state = "sending";
      paint();
      await api("/api/app/send", { method: "POST", body: { workspace: ws, run, text: item.sentText } });
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
      const t = ro
        ? await api(`/api/app/team/transcript?member=${encodeURIComponent(member)}&workspace=${encodeURIComponent(ws)}&run=${encodeURIComponent(run)}&limit=300`)
        : await api(`/api/app/transcript?workspace=${encodeURIComponent(ws)}&run=${encodeURIComponent(run)}&limit=300`);
      store.transcripts.set(key, t);
      lastErr = null;
      waking = false;
      reconcilePending(t);
    } catch (err) {
      // A sleeping home is being woken by the server: say so in place, no toast.
      // A teammate's sleeping machine is never woken from here; a private (or
      // deleted) agent is a 404. Both say so in place.
      if (ro && (err.status === 503 || err.status === 404)) {
        waking = err.status === 503;
        const prev = store.transcripts.get(key);
        const text = err.status === 404 ? "This agent is private now, or it was deleted." : err.message;
        store.transcripts.set(key, { ...(prev || {}), messages: err.status === 404 ? [] : prev?.messages || [], error: text, roNotice: text });
        return;
      }
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
      const sent = (p.sentText || p.text).trim();
      return !users.some((m) => (m.text || "").trim() === sent && (m.ts == null || m.ts >= p.ts - 120000));
    });
    store.pending.set(key, kept);
  }

  function header() {
    paintRoBar();
    paintLock();
    const a = agentNow();
    const t = store.transcripts.get(key);
    const title = a?.title || t?.title || "Agent";
    setText(el.querySelector(".title"), title);
    document.title = ro ? `${title} · ${memberName(teamAgent(member, ws, run)?.member)} · Rudder` : `${title} · Rudder`;
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
    interruptBtn.hidden = ro || !isRunning(status);
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
    for (const m of withCompactMarkers(t.messages || [])) {
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
      items.push({ kind: "error", id: "error", text: t.error, ...(ro ? { ic: t.roNotice?.includes("private") ? "lock" : "power", title: t.roNotice?.includes("private") ? "Not visible to you" : "Their cloud machine is asleep" } : {}) });
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
    if (x.kind === "error") return emptyState({ ic: x.ic || "alert", title: x.title || "Couldn't load the conversation", body: escapeHtml(x.text) });
    if (x.kind === "pending") return h(`<div class="msg user"><div class="atts" hidden></div><div class="bubble md"></div><div class="m-foot"></div></div>`);
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
    if (role === "user") return h(`<div class="msg user"><div class="atts" hidden></div><div class="bubble md"></div></div>`);
    if (role === "system" && x.m.kind === "compact") {
      return h(`<div class="compact"><button type="button" class="compact-row" aria-expanded="false"><span class="c-line"></span><span class="c-label"><span class="c-text"></span>${icon("chevron").replace("<svg", '<svg class="t-caret"')}</span><span class="c-line"></span></button><div class="compact-sum md" hidden></div></div>`);
    }
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
      md.hidden = !p.text;
      paintAttachments(node.querySelector(".atts"), p.files || []);
      const uploading = (p.files || []).filter((f) => !f.path).length;
      const label = p.state === "sent" ? "Delivered" : p.state === "uploading" && uploading ? `Uploading ${p.files.length - uploading + 1} of ${p.files.length}` : "Sending";
      const foot = node.querySelector(".m-foot");
      const html = p.state === "failed" ? `<span>Not sent</span><button type="button" class="retry">Retry</button>` : `<span>${label}</span>`;
      if (foot.dataset.h !== html) { foot.innerHTML = html; foot.dataset.h = html; }
      return;
    }
    if (x.kind === "steps") return paintSteps(node, x);
    const m = x.m;
    if (m.role === "tool") { node._msg = m; return paintTool(node, m); }
    if (m.kind === "compact") { node._msg = m; return paintCompact(node, m); }
    const md = node.querySelector(".md");
    let text = m.text || "";
    if (m.role === "user") {
      const split = splitAttachments(text);
      text = split.body;
      md.hidden = !text;
      paintAttachments(node.querySelector(".atts"), split.files.map((path) => ({ path })));
    }
    const sig = `${text}|${markdownReady()}`;
    if (md.dataset.sig !== sig) { renderMarkdown(md, text); md.dataset.sig = sig; }
  }

  function paintCompact(node, m) {
    const c = m.compact || {};
    const open = store.expanded.has(`${key}|${node.dataset.key}`) && !!c.summary;
    const row = node.querySelector(".compact-row");
    node.classList.toggle("open", open);
    node.classList.toggle("has-sum", !!c.summary);
    row.disabled = !c.summary;
    row.setAttribute("aria-expanded", String(open));
    const tokens = c.preTokens ? ` · ${c.preTokens >= 1000 ? `${Math.round(c.preTokens / 1000)}k` : c.preTokens} tokens summarized` : "";
    setText(row.querySelector(".c-text"), `Context compacted${tokens}`);
    row.setAttribute("aria-label", `Context compacted${tokens}.${c.summary ? ` ${open ? "Hide" : "Show"} the summary the agent continued from.` : ""}`);
    const sum = node.querySelector(".compact-sum");
    sum.hidden = !open;
    if (!open) return;
    const sig = `${c.summary}|${markdownReady()}`;
    if (sum.dataset.sig !== sig) { renderMarkdown(sum, c.summary); sum.dataset.sig = sig; }
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
    if (!document.hidden) {
      // A teammate's agent: its title and status come from the team view.
      if (ro) loadTeamFor(member).then(() => { if (timer !== -1) paint(); });
      await loadTranscript();
    }
    if (timer !== -1) timer = setTimeout(tick, ro ? (waking ? 10000 : 3000) : waking ? 5000 : 2000);
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
      if (!ro) fastStatePoll = null;
      document.removeEventListener("visibilitychange", onVis);
      resizeObs.disconnect();
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

// The iOS shell's 9:01 notification, tapped: open the newest digest.
window.rudderOpenDigest = () => { if (!store.signedOut) navigate(digestPath(null)); };

window.rudderRefresh = () => {
  nativeSignInRequested = false;
  if (NATIVE && window.RUDDER_NATIVE?.token) NATIVE.token = window.RUDDER_NATIVE.token;
  return refreshState().then(approvePendingComputer);
};
// A computer's approve code that waited for this phone to sign in.
function approvePendingComputer() {
  if (store.signedOut || !store.state) return;
  const code = takePendingApprove();
  if (code) openComputerSignIn(code);
}

onMarkdownReady(() => { /* agent screen repaints itself */ });
loadMarkdown();

function restoreLastRoute() {
  const here = parseRoute(location.pathname);
  if (here.name !== "home" || history.state?.depth) return;
  const last = ls.get("rudder.lastRoute");
  if (!last || Date.now() - last.at > 24 * 3600 * 1000) return;
  const r = parseRoute(last.path || "");
  if (r.name !== "project" && r.name !== "agent") return;
  history.replaceState({ depth: 0 }, "", "/app");
  depth = 1;
  history.pushState({ depth }, "", projectPath(r.ws));
  if (r.name === "agent") { depth = 2; history.pushState({ depth }, "", agentPath(r.ws, r.run)); }
}

if (!history.state) history.replaceState({ depth: 0 }, "", location.pathname + location.search);
// /app/join/<token> and /app/link/slack/<code> need an account: if signing in
// comes first, the link is waiting afterwards.
if (parseRoute(location.pathname).needsAccount) rememberPendingLink(location.pathname);
// /app/pair?code=ABCD-EFGH (the link `rudder cloud app` prints) signs in here.
const pairFromLink = location.pathname === "/app/pair" ? new URLSearchParams(location.search).get("code") : null;
// /app/approve?code=K7P-4QX (the QR `rudder login` prints) approves a computer
// once this phone is signed in.
const approveFromLink = location.pathname === "/app/approve" ? new URLSearchParams(location.search).get("code") : null;
if (approveFromLink) {
  ls.set(APPROVE_KEY, { code: formatComputerCode(approveFromLink), at: Date.now() });
  history.replaceState({ depth: 0 }, "", "/app");
}
if (pairFromLink && !NATIVE) {
  history.replaceState({ depth: 0 }, "", "/app");
  redeemPairCode(pairFromLink, null);
} else if (!approveFromLink) {
  restoreLastRoute();
}
mountScreen();
refreshState().then(() => {
  pollState();
  approvePendingComputer();
});

if (!NATIVE && "serviceWorker" in navigator && (location.protocol === "https:" || location.hostname === "localhost")) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/app/sw.js", { scope: "/app" })
      .catch(() => navigator.serviceWorker.register("/app/sw.js", { scope: "/app/" }))
      .catch((err) => console.warn("service worker not registered", err));
  });
}
