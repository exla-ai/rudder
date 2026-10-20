// Settings, organizations and integrations.
// - Settings is a screen (/app/settings): account facts, the org switcher
//   (personal or an org), that org's members and invite links, and the Slack
//   and Linear connections for whichever is selected.
// - /app/join/<token> previews and accepts an org invite.
// - /app/link/slack/<code> binds a Slack user (from `/rudder link`) to this account.
// - "From Linear" in a project's task box starts an agent on a Linear issue.
import { h, icon, setText, copyText } from "/app/render.js";
import { app, api, ls, NATIVE, PLATFORM, rel, confirmDialog, openSheet, openExternal, showError, toast, track } from "/app/core.js";
import { DOWNLOAD_MAC_URL } from "/app/onboarding.js";

const ROLE_LABEL = { admin: "Admin", member: "Member" };
const SCOPE_KEY = "rudder.org";
const PENDING_KEY = "rudder.pendingLink";

const SLACK_SVG = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><path d="M9 3.5v7M15 13.5v7M3.5 15h7M13.5 9h7"/><path d="M9 14.5v6M15 3.5v6M3.5 9h6M14.5 15h6"/></svg>';
const LINEAR_SVG = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="8.5"/><path d="M5 14.5l4.5 4.5M4 10.5L13.5 20M6 7.5L16.5 18M8.5 5L19 15.5"/></svg>';

/** The selected owner scope: an org id, or "" for personal. */
export function currentScope() {
  const id = ls.get(SCOPE_KEY) || "";
  const orgs = app.store?.state?.orgs;
  if (id && Array.isArray(orgs) && !orgs.some((o) => o.id === id)) return "";
  return id;
}
function setScope(id) { ls.set(SCOPE_KEY, id || ""); app.onScopeChange?.(); }
const scopeQuery = (org) => (org ? `?org=${encodeURIComponent(org)}` : "");
const scopeBody = (org) => (org ? { org } : {});

/* Links that need a signed-in account first -------------------------- */

/** Remember a join / Slack link opened while signed out; it reopens after sign-in. */
export function rememberPendingLink(path) { ls.set(PENDING_KEY, { path, at: Date.now() }); }
export function takePendingLink() {
  const p = ls.get(PENDING_KEY);
  ls.set(PENDING_KEY, null);
  return p && Date.now() - p.at < 60 * 60_000 ? p.path : null;
}

/* Integrations cache (also used by the task box) --------------------- */

const integrationsCache = new Map(); // scope -> { at, data }
async function loadIntegrations(org, force = false) {
  const hit = integrationsCache.get(org);
  if (!force && hit && Date.now() - hit.at < 60_000) return hit.data;
  const data = await api(`/api/app/integrations${scopeQuery(org)}`);
  integrationsCache.set(org, { at: Date.now(), data });
  return data;
}
/** Resolves true when Linear is connected for the selected scope. Never throws. */
export async function linearAvailable() {
  try { return !!(await loadIntegrations(currentScope()))?.linear?.connected; } catch { return false; }
}

/* Shared bits -------------------------------------------------------- */

export function screenShell(title, { back = "/app" } = {}) {
  const el = h(`<section class="screen settings-screen">
    <header class="topbar">
      <a class="icon-btn back" aria-label="Back">${icon("back")}</a>
      <div class="titles"><div class="title"></div></div>
    </header>
    <div class="scroller"><div class="set-body"></div></div></section>`);
  setText(el.querySelector(".title"), title);
  const backEl = el.querySelector(".back");
  backEl.href = back;
  backEl.addEventListener("click", (e) => { e.preventDefault(); app.goBack?.(back); });
  const bar = el.querySelector(".topbar");
  const scroller = el.querySelector(".scroller");
  scroller.addEventListener("scroll", () => bar.classList.toggle("scrolled", scroller.scrollTop > 2), { passive: true });
  return { el, body: el.querySelector(".set-body") };
}

export function section(title, { count } = {}) {
  const sec = h(`<div class="section"><div class="section-head"><h2></h2><span class="count"></span></div><div class="sec-body"></div></div>`);
  setText(sec.querySelector("h2"), title);
  if (count != null) setText(sec.querySelector(".count"), String(count));
  return { el: sec, body: sec.querySelector(".sec-body"), setCount: (n) => setText(sec.querySelector(".count"), n == null ? "" : String(n)) };
}

export function initials(s) {
  const t = String(s || "?").trim();
  const parts = t.split(/[\s@._-]+/).filter(Boolean);
  return ((parts[0]?.[0] || "?") + (parts.length > 1 ? parts[1][0] : "")).toUpperCase();
}

export function note(text, cls = "") {
  const n = h(`<div class="repo-note ${cls}"></div>`);
  n.textContent = text;
  return n;
}

/* Settings ----------------------------------------------------------- */

const PROVIDER_NAME = { slack: "Slack", linear: "Linear" };
// What the OAuth callbacks put in ?error= (with &provider=).
function oauthErrorText(code, provider) {
  const p = PROVIDER_NAME[provider] || "The integration";
  switch (code) {
    case "cancelled": return `${p} was not connected: you cancelled.`;
    case "denied": return `${p} was not connected: access was declined.`;
    case "bad_state": return `That ${p} sign-in expired. Try Connect again.`;
    case "no_code": return `${p} did not finish signing in. Try Connect again.`;
    case "forbidden": return `Only an admin of this organization can connect ${p}.`;
    case "exchange_failed": return `${p} could not be reached to finish connecting. Try again in a minute.`;
    case "not_configured": return `${p} is not set up on this server yet.`;
    default: return `${p} was not connected (${code}).`;
  }
}

export function SettingsScreen() {
  const { el, body } = screenShell("Settings");
  // Back from an OAuth round trip: say whether it worked, once.
  const params = new URLSearchParams(location.search);
  const connected = params.get("connected");
  const connectError = params.get("error");
  if (connected || connectError) {
    history.replaceState(history.state, "", "/app/settings");
    // The callback names the org it connected for: show that org's cards.
    const forOrg = params.get("org");
    if (forOrg) setScope(forOrg);
    integrationsCache.clear();
    const name = PROVIDER_NAME[connected] || "";
    if (name) { toast(`${name} connected`); track("integration_connected", { provider: connected }); }
    else if (connectError) toast(oauthErrorText(connectError, params.get("provider")), { error: true });
  }

  const account = section("Account");
  const scopeSec = section("Workspace");
  const orgSec = section("Organization");
  const intSec = section("Integrations");
  const more = h(`<div class="section set-foot"></div>`);
  body.append(account.el, scopeSec.el, orgSec.el, intSec.el, more);

  // Account: who, where the agents run, sign-in helpers.
  const kvs = h(`<div class="card set-kvs"></div>`);
  const acts = h(`<div class="set-acts"></div>`);
  account.body.append(kvs, acts);
  const addAct = (ic, label, fn, cls = "") => {
    const b = h(`<button type="button" class="btn ${cls}">${ic.startsWith("<svg") ? ic : icon(ic)}<span></span></button>`);
    setText(b.querySelector("span"), label);
    b.addEventListener("click", fn);
    acts.appendChild(b);
    return b;
  };
  addAct("terminal", "Sign in a computer", () => app.openComputerSignIn?.());
  const ghBtn = addAct("github", "Connect GitHub", () => app.openGithubConnect?.());
  addAct("spark", "How you use Rudder", () => app.navigate?.("/app/welcome"));
  if (PLATFORM !== "mac") {
    addAct("down", "Get the Mac app", () => { track("download_mac_clicked", { from: "settings" }); openExternal(DOWNLOAD_MAC_URL); });
  }

  const signOutBtn = h(`<button type="button" class="btn block danger">Sign out</button>`);
  signOutBtn.addEventListener("click", () => app.signOut?.());
  more.appendChild(signOutBtn);

  function paintAccount() {
    const s = app.store?.state || {};
    const home = s.home;
    const rows = [["Account", s.email || "Signed in"], ["Cloud machine", home ? app.machineLabel?.(home)?.label || home.status : "None"]];
    if (home?.tailnet?.dnsName) rows.push(["Tailnet", home.tailnet.dnsName]);
    const gh = app.githubState?.(s);
    if (gh) rows.push(["GitHub", gh.kind === "ok" ? gh.sub.replace(/^Connected as /, "") : "Not connected"]);
    if (PLATFORM) rows.push(["App", PLATFORM === "mac" ? "Mac" : "iOS"]);
    const sig = JSON.stringify(rows);
    if (kvs.dataset.sig !== sig) {
      kvs.dataset.sig = sig;
      kvs.replaceChildren(...rows.map(([k, v]) => {
        const r = h(`<div class="kv"><span></span><span></span></div>`);
        r.children[0].textContent = k; r.children[1].textContent = v;
        return r;
      }));
    }
    setText(ghBtn.querySelector("span"), gh?.kind === "ok" ? "Reconnect GitHub" : "Connect GitHub");
  }

  // Workspace: personal, or one of your orgs. Integrations follow it.
  let orgs = Array.isArray(app.store?.state?.orgs) ? app.store.state.orgs : null;
  let scope = currentScope();
  const scopeList = h(`<div class="owner-list scope-list" role="radiogroup" aria-label="Workspace"></div>`);
  const newOrgBtn = h(`<button type="button" class="ghost-add">${icon("plus")}<span>Create organization</span></button>`);
  newOrgBtn.addEventListener("click", () => openCreateOrg((org) => { scope = org.id; setScope(scope); loadOrgs(); }));
  scopeSec.body.append(scopeList, newOrgBtn);

  function paintScopes() {
    const list = [{ id: "", name: "Personal", sub: app.store?.state?.email || "Just you" }, ...(orgs || []).map((o) => ({
      id: o.id, name: o.name, sub: `${ROLE_LABEL[o.role] || o.role} · ${o.memberCount || 1} ${o.memberCount === 1 ? "member" : "members"}`,
    }))];
    if (!list.some((x) => x.id === scope)) scope = "";
    scopeList.replaceChildren(...list.map((o) => {
      const b = h(`<button type="button" role="radio" class="owner-opt"><span class="o-av"><span class="org-av"></span></span><span class="o-text"><span class="o-name"></span><span class="o-kind"></span></span><span class="o-radio">${icon("check")}</span></button>`);
      const av = b.querySelector(".org-av");
      if (o.id) setText(av, initials(o.name)); else av.innerHTML = icon("server");
      setText(b.querySelector(".o-name"), o.name);
      setText(b.querySelector(".o-kind"), o.sub);
      b.setAttribute("aria-checked", String(o.id === scope));
      b.addEventListener("click", () => {
        if (scope === o.id) return;
        scope = o.id; setScope(scope);
        paintScopes(); paintOrg(); paintIntegrations();
      });
      return b;
    }));
  }

  async function loadOrgs() {
    try {
      const res = await api("/api/app/orgs");
      orgs = res?.orgs || [];
      if (app.store?.state) app.store.state.orgs = orgs;
    } catch (err) {
      if (err.status === 404) orgs = [];
      else showError(err);
    }
    paintScopes(); paintOrg(); paintIntegrations();
  }

  // Organization: members, roles, invites. Admins manage; members see.
  let detail = null; // { org, role, members, invites }
  let detailFor = null;
  async function loadDetail(id) {
    detailFor = id;
    try {
      const res = await api(`/api/app/orgs/${encodeURIComponent(id)}`);
      if (detailFor !== id) return;
      detail = res;
    } catch (err) {
      if (detailFor !== id) return;
      detail = { error: err.message };
      if (err.status === 404) { scope = ""; setScope(""); loadOrgs(); return; }
    }
    paintOrg();
  }

  function paintOrg() {
    orgSec.el.hidden = !scope;
    if (!scope) return;
    if (detailFor !== scope) { detail = null; orgSec.body.replaceChildren(h(`<div class="sk sk-card" style="height:120px"></div>`)); loadDetail(scope); return; }
    if (!detail) return;
    if (detail.error) { orgSec.body.replaceChildren(note(detail.error, "err")); return; }
    const admin = detail.role === "admin";
    const org = detail.org || {};
    const members = detail.members || [];
    orgSec.setCount(members.length);
    const frag = [];

    // Name (rename for admins)
    const nameRow = h(`<div class="card org-name"><div class="org-av big"></div><div class="on-body"><div class="on-name"></div><div class="on-sub"></div></div></div>`);
    setText(nameRow.querySelector(".org-av"), initials(org.name));
    setText(nameRow.querySelector(".on-name"), org.name || "Organization");
    setText(nameRow.querySelector(".on-sub"), `You're ${admin ? "an admin" : "a member"}${org.createdAt ? ` · created ${rel(org.createdAt)}` : ""}`);
    if (admin) {
      const rename = h(`<button type="button" class="btn small">Rename</button>`);
      rename.addEventListener("click", () => openRenameOrg(org, () => { loadDetail(scope); loadOrgs(); }));
      nameRow.appendChild(rename);
    }
    frag.push(nameRow);

    // Team: what everyone's agents are doing (team.js).
    const teamLink = h(`<a class="card team-link tap" data-link><span class="tl-ic">${icon("users")}</span><span class="tl-text"><span class="tl-name">See your team's agents</span><span class="tl-sub">What each person is running right now. Read only.</span></span>${icon("chevron")}</a>`);
    teamLink.href = `/app/team?org=${encodeURIComponent(scope)}`;
    frag.push(teamLink);

    // Members
    const list = h(`<div class="card members" role="list" aria-label="Members"></div>`);
    for (const m of members) {
      const row = h(`<div class="member" role="listitem"><span class="m-av"></span><span class="m-text"><span class="m-name"></span><span class="m-email"></span></span><span class="m-role"></span></div>`);
      setText(row.querySelector(".m-av"), initials(m.name || m.email));
      setText(row.querySelector(".m-name"), m.name || m.email || m.accountId);
      if (m.you) row.querySelector(".m-name").appendChild(h(`<span class="chip you">You</span>`));
      setText(row.querySelector(".m-email"), m.name && m.email ? m.email : m.joinedAt ? `Joined ${rel(m.joinedAt)}` : "");
      const roleBox = row.querySelector(".m-role");
      if (admin) {
        const sel = h(`<select class="role-select" aria-label="Role"><option value="admin">Admin</option><option value="member">Member</option></select>`);
        sel.value = m.role;
        sel.setAttribute("aria-label", `Role for ${m.name || m.email || "member"}`);
        sel.addEventListener("change", async () => {
          const role = sel.value;
          sel.disabled = true;
          try {
            await api(`/api/app/orgs/${encodeURIComponent(scope)}/members/${encodeURIComponent(m.accountId)}`, { method: "POST", body: { role } });
            toast(`${m.name || m.email || "Member"} is now ${ROLE_LABEL[role].toLowerCase() === "admin" ? "an admin" : "a member"}`);
            loadDetail(scope); loadOrgs();
          } catch (err) { sel.value = m.role; showError(err); }
          finally { sel.disabled = false; }
        });
        roleBox.appendChild(sel);
        if (!m.you) {
          const rm = h(`<button type="button" class="icon-btn m-rm">${icon("x")}</button>`);
          rm.setAttribute("aria-label", `Remove ${m.name || m.email || "member"}`);
          rm.title = "Remove from organization";
          rm.addEventListener("click", () => removeMember(m));
          roleBox.appendChild(rm);
        }
      } else {
        roleBox.appendChild(h(`<span class="chip"></span>`));
        setText(roleBox.firstChild, ROLE_LABEL[m.role] || m.role);
      }
      list.appendChild(row);
    }
    frag.push(list);

    // Invites (admins)
    if (admin) {
      const inv = h(`<div class="card invite-card">
        <div class="ic-title">Invite people</div>
        <div class="ic-sub">Make a link and send it however you like. Each link works once and lasts 7 days.</div>
        <form class="invite-form" novalidate>
          <input class="input" type="email" name="email" placeholder="Email (optional)" aria-label="Email (optional, locks the link to this address)" autocapitalize="off" autocorrect="off" spellcheck="false">
          <select class="role-select" name="role" aria-label="Role"><option value="member">Member</option><option value="admin">Admin</option></select>
          <button type="submit" class="btn accent">Create link</button>
        </form>
        <div class="invite-out" hidden><code class="io-url"></code><button type="button" class="btn small io-copy">${icon("copy")}<span>Copy</span></button></div>
      </div>`);
      const form = inv.querySelector("form");
      const out = inv.querySelector(".invite-out");
      form.addEventListener("submit", async (e) => {
        e.preventDefault();
        const email = form.email.value.trim();
        if (email && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) { toast("That email doesn't look right", { error: true }); return; }
        const btn = form.querySelector("button");
        btn.disabled = true;
        try {
          const res = await api(`/api/app/orgs/${encodeURIComponent(scope)}/invites`, { method: "POST", body: email ? { email, role: form.role.value } : { role: form.role.value } });
          const url = res?.invite?.url || "";
          out.hidden = !url;
          setText(out.querySelector(".io-url"), url);
          out.querySelector(".io-copy").onclick = async () => { if (await copyText(url)) toast("Invite link copied"); };
          if (url && await copyText(url)) toast("Invite link created and copied");
          form.email.value = "";
          const keepOut = out;
          await loadDetail(scope);
          // Repaint keeps the fresh link on screen.
          orgSec.body.querySelector(".invite-card")?.replaceWith(inv);
          keepOut.hidden = !url;
        } catch (err) { showError(err); }
        finally { btn.disabled = false; }
      });
      frag.push(inv);

      const invites = detail.invites || [];
      if (invites.length) {
        const pend = h(`<div class="card members invites" role="list" aria-label="Pending invites"><div class="inv-h">Pending invites</div></div>`);
        for (const i of invites) {
          const row = h(`<div class="member" role="listitem"><span class="m-av">${icon("external")}</span><span class="m-text"><span class="m-name"></span><span class="m-email"></span></span><span class="m-role"><button type="button" class="btn small">Revoke</button></span></div>`);
          setText(row.querySelector(".m-name"), i.email || "Anyone with the link");
          setText(row.querySelector(".m-email"), `${ROLE_LABEL[i.role] || i.role}${i.expiresAt ? ` · expires ${new Date(i.expiresAt).toLocaleDateString(undefined, { month: "short", day: "numeric" })}` : ""}`);
          row.querySelector("button").setAttribute("aria-label", `Revoke invite for ${i.email || "anyone with the link"}`);
          row.querySelector("button").addEventListener("click", async () => {
            try {
              await api(`/api/app/orgs/${encodeURIComponent(scope)}/invites/${encodeURIComponent(i.id)}`, { method: "DELETE" });
              toast("Invite revoked");
              loadDetail(scope);
            } catch (err) { showError(err); }
          });
          pend.appendChild(row);
        }
        frag.push(pend);
      }
    }

    // Leave / delete
    const foot = h(`<div class="org-foot"></div>`);
    const me = members.find((m) => m.you);
    if (me) {
      const leave = h(`<button type="button" class="btn danger">Leave organization</button>`);
      leave.addEventListener("click", () => removeMember(me, { leave: true }));
      foot.appendChild(leave);
    }
    if (admin) {
      const del = h(`<button type="button" class="btn danger">Delete organization</button>`);
      del.addEventListener("click", async () => {
        const ok = await confirmDialog({ title: `Delete ${org.name}?`, body: "Everyone loses access to it, and its Slack and Linear connections are removed. This cannot be undone.", confirm: "Delete", danger: true });
        if (!ok) return;
        try {
          await api(`/api/app/orgs/${encodeURIComponent(scope)}`, { method: "DELETE" });
          toast("Organization deleted");
          scope = ""; setScope(""); loadOrgs();
        } catch (err) { showError(err); }
      });
      foot.appendChild(del);
    }
    frag.push(foot);
    orgSec.body.replaceChildren(...frag);
  }

  async function removeMember(m, { leave = false } = {}) {
    const orgName = detail?.org?.name || "this organization";
    const ok = await confirmDialog(leave
      ? { title: `Leave ${orgName}?`, body: "You lose access to its integrations. An admin can invite you back.", confirm: "Leave", danger: true }
      : { title: `Remove ${m.name || m.email || "this member"}?`, body: `They lose access to ${orgName} and its integrations.`, confirm: "Remove", danger: true });
    if (!ok) return;
    try {
      await api(`/api/app/orgs/${encodeURIComponent(scope)}/members/${encodeURIComponent(m.accountId)}`, { method: "DELETE" });
      toast(leave ? `You left ${orgName}` : "Member removed");
      if (leave) { scope = ""; setScope(""); }
      loadOrgs();
      if (!leave) loadDetail(scope);
    } catch (err) { showError(err); }
  }

  // Integrations: Slack and Linear for the selected scope.
  let intFor = null;
  async function paintIntegrations(force = false, quiet = false) {
    const org = scope;
    intSec.el.querySelector("h2").textContent = org ? `Integrations · ${orgs?.find((o) => o.id === org)?.name || "Organization"}` : "Integrations · Personal";
    if (intFor !== org || (force && !quiet)) {
      intFor = org;
      intSec.body.replaceChildren(h(`<div class="sk sk-card" style="height:76px"></div>`), h(`<div class="sk sk-card" style="height:76px"></div>`));
    }
    let data;
    try { data = await loadIntegrations(org, force); }
    catch (err) {
      if (intFor !== org) return;
      intSec.body.replaceChildren(note(err.status === 404 ? "Integrations are not available on this server yet." : err.message, err.status === 404 ? "" : "err"));
      return;
    }
    if (intFor !== org) return;
    const key = JSON.stringify(data);
    if (quiet && key === intShown) return; // nothing moved: keep the cards (and any open select)
    intShown = key;
    intSec.body.replaceChildren(slackCard(org, data), linearCard(org, data));
  }
  let intShown = "";

  function integrationCard({ svg, name, blurb }) {
    const card = h(`<div class="card integ"><div class="in-head"><span class="in-ic"></span><span class="in-text"><span class="in-name"></span><span class="in-sub"></span></span><span class="in-act"></span></div><div class="in-extra" hidden></div></div>`);
    card.querySelector(".in-ic").innerHTML = svg;
    setText(card.querySelector(".in-name"), name);
    setText(card.querySelector(".in-sub"), blurb);
    return card;
  }
  const connectBtn = (provider, org) => {
    const b = h(`<button type="button" class="btn small accent">Connect</button>`);
    b.addEventListener("click", async () => {
      b.disabled = true;
      track("integration_connect_clicked", { provider, scope: org ? "org" : "personal" });
      try {
        const res = await api(`/api/app/integrations/${provider}/start`, { method: "POST", body: scopeBody(org) });
        if (!res?.url) throw new Error("The server returned no sign-in link");
        location.href = res.url;
      } catch (err) { showError(err); b.disabled = false; }
    });
    return b;
  };
  const disconnectBtn = (provider, label, org) => {
    const b = h(`<button type="button" class="btn small">Disconnect</button>`);
    b.addEventListener("click", async () => {
      const ok = await confirmDialog({ title: `Disconnect ${label}?`, body: provider === "slack" ? "Rudder stops posting agent updates to Slack, and /rudder stops working there." : "Starting agents from Linear issues stops working.", confirm: "Disconnect", danger: true });
      if (!ok) return;
      try {
        await api(`/api/app/integrations/${provider}${scopeQuery(org)}`, { method: "DELETE" });
        track("integration_disconnected", { provider });
        toast(`${label} disconnected`);
        paintIntegrations(true);
      } catch (err) { showError(err); }
    });
    return b;
  };
  const noAdmin = (label) => { const s = h(`<span class="in-note"></span>`); setText(s, "An admin connects it"); s.title = `Ask an admin of this organization to connect ${label}`; return s; };

  function slackCard(org, data) {
    const s = data?.slack || {};
    const card = integrationCard({ svg: SLACK_SVG, name: "Slack", blurb: "Agent updates in a channel, and /rudder to start tasks." });
    const act = card.querySelector(".in-act");
    const sub = card.querySelector(".in-sub");
    if (!s.configured) { setText(sub, "Not set up on this server yet"); card.classList.add("off"); return card; }
    if (!s.connected) { act.appendChild(data.canManage ? connectBtn("slack", org) : noAdmin("Slack")); return card; }
    setText(sub, `Connected to ${s.team?.name || "your workspace"}`);
    card.classList.add("on");
    if (data.canManage) act.appendChild(disconnectBtn("slack", "Slack", org));
    const extra = card.querySelector(".in-extra");
    extra.hidden = false;
    const label = h(`<label class="field-label">Post updates to</label>`);
    const sel = h(`<select class="role-select channel-select" aria-label="Slack channel for agent updates"></select>`);
    const id = `ch-${Math.random().toString(36).slice(2, 7)}`;
    sel.id = id; label.htmlFor = id;
    const cur = s.channel;
    sel.appendChild(new Option(cur ? `#${cur.name}` : "Choose a channel", cur?.id || ""));
    sel.disabled = !data.canManage;
    let loaded = false;
    const loadChannels = async () => {
      if (loaded) return;
      loaded = true;
      try {
        const res = await api(`/api/app/integrations/slack/channels${scopeQuery(org)}`);
        const chans = res?.channels || [];
        sel.replaceChildren(...(cur ? [] : [new Option("Choose a channel", "")]), ...chans.map((c) => new Option(`#${c.name}`, c.id)));
        if (cur) sel.value = cur.id;
        if (!chans.length) sel.appendChild(new Option("No channels the Rudder app can see", "", false, false));
      } catch (err) { loaded = false; showError(err); }
    };
    if (data.canManage) { sel.addEventListener("focus", loadChannels); sel.addEventListener("pointerdown", loadChannels); loadChannels(); }
    sel.addEventListener("change", async () => {
      if (!sel.value) return;
      try {
        const res = await api("/api/app/integrations/slack/channel", { method: "POST", body: { ...scopeBody(org), channel: sel.value } });
        toast(`Updates go to #${res?.channel?.name || sel.selectedOptions[0]?.textContent?.replace(/^#/, "")}`);
        integrationsCache.delete(org);
      } catch (err) { showError(err); }
    });
    const hint = h(`<div class="hint">In Slack, type <code>/rudder link</code> once to connect your Slack account.</div>`);
    extra.append(label, sel, hint);
    return card;
  }

  function linearCard(org, data) {
    const l = data?.linear || {};
    const card = integrationCard({ svg: LINEAR_SVG, name: "Linear", blurb: "Start an agent on a Linear issue from a project's task box." });
    const act = card.querySelector(".in-act");
    const sub = card.querySelector(".in-sub");
    if (!l.configured) { setText(sub, "Not set up on this server yet"); card.classList.add("off"); return card; }
    if (!l.connected) { act.appendChild(data.canManage ? connectBtn("linear", org) : noAdmin("Linear")); return card; }
    setText(sub, `Connected to ${l.workspace?.name || "your workspace"}`);
    card.classList.add("on");
    if (data.canManage) act.appendChild(disconnectBtn("linear", "Linear", org));
    return card;
  }

  function update() {
    paintAccount();
    const fresh = app.store?.state?.orgs;
    if (Array.isArray(fresh) && JSON.stringify(fresh) !== JSON.stringify(orgs)) { orgs = fresh; paintScopes(); }
  }
  paintAccount(); paintScopes(); paintOrg(); paintIntegrations();
  loadOrgs();
  // A Connect can finish in another window (the Mac app hands OAuth to the
  // default browser), so look again when this one comes back.
  let lastFocus = 0;
  const onFocus = () => {
    if (document.visibilityState === "hidden" || Date.now() - lastFocus < 2000) return;
    lastFocus = Date.now();
    paintIntegrations(true, true);
  };
  window.addEventListener("focus", onFocus);
  document.addEventListener("visibilitychange", onFocus);
  return { el, update, destroy() { window.removeEventListener("focus", onFocus); document.removeEventListener("visibilitychange", onFocus); } };
}

function openCreateOrg(onCreated) {
  const { body, close } = openSheet({ title: "Create organization", className: "org-sheet" });
  body.append(h(`<form class="org-form" novalidate>
    <label class="field-label" for="org-name">Name</label>
    <input id="org-name" class="input" name="name" placeholder="Acme" maxlength="80" autocomplete="organization" enterkeyhint="go">
    <p class="hint">You'll be its admin. Invite people with a link next; they share its Slack and Linear connections.</p>
    <button type="submit" class="btn accent block" disabled>Create</button>
  </form>`));
  const form = body.querySelector("form");
  const btn = form.querySelector("button");
  form.name.addEventListener("input", () => { btn.disabled = !form.name.value.trim(); });
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = form.name.value.trim();
    if (!name) return;
    btn.disabled = true;
    try {
      const res = await api("/api/app/orgs", { method: "POST", body: { name } });
      close();
      toast(`Created ${res?.org?.name || name}`);
      if (res?.org) onCreated?.(res.org);
    } catch (err) { showError(err); btn.disabled = false; }
  });
  setTimeout(() => form.name.focus(), 50);
}

function openRenameOrg(org, onDone) {
  const { body, close } = openSheet({ title: "Rename organization", className: "org-sheet" });
  body.append(h(`<form class="org-form" novalidate>
    <label class="field-label" for="org-rename">Name</label>
    <input id="org-rename" class="input" name="name" maxlength="80" enterkeyhint="go">
    <button type="submit" class="btn accent block">Save</button>
  </form>`));
  const form = body.querySelector("form");
  form.name.value = org.name || "";
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = form.name.value.trim();
    if (!name) return;
    try {
      await api(`/api/app/orgs/${encodeURIComponent(org.id)}`, { method: "PATCH", body: { name } });
      close(); toast("Renamed"); onDone?.();
    } catch (err) { showError(err); }
  });
  setTimeout(() => form.name.select(), 50);
}

/* Join an org -------------------------------------------------------- */

function centerScreen() {
  const el = h(`<section class="screen"><div class="signin center-card"></div></section>`);
  return { el, box: el.querySelector(".center-card") };
}

export function JoinScreen(token) {
  const { el, box } = centerScreen();
  box.replaceChildren(h(`<div class="ls-wait"><span class="spin"></span><div class="ls-title">Opening your invite…</div></div>`));
  const fail = (title, text) => {
    box.replaceChildren(h(`<div class="cc-icon">${icon("alert")}</div>`), h(`<h1></h1>`), h(`<p></p>`), h(`<div class="actions"><a class="btn primary block" href="/app" data-link>Go to Rudder</a></div>`));
    setText(box.querySelector("h1"), title);
    setText(box.querySelector("p"), text);
  };
  api(`/api/app/invites/${encodeURIComponent(token)}`).then((inv) => {
    const org = inv?.org || {};
    if (inv?.alreadyMember) {
      box.replaceChildren(h(`<div class="cc-icon ok">${icon("check")}</div>`), h(`<h1></h1>`), h(`<p>You're already a member.</p>`), h(`<div class="actions"><a class="btn primary block" href="/app/settings" data-link>Open settings</a></div>`));
      setText(box.querySelector("h1"), org.name || "Organization");
      setScope(org.id);
      return;
    }
    box.replaceChildren(h(`<div class="org-av big">${""}</div>`), h(`<h1></h1>`), h(`<p class="ji-text"></p>`), h(`<p class="ji-lock" hidden></p>`),
      h(`<div class="actions"><button type="button" class="btn accent block ji-accept">Join</button><div class="alt"><a href="/app" data-link>Not now</a></div></div>`));
    setText(box.querySelector(".org-av"), initials(org.name));
    setText(box.querySelector("h1"), `Join ${org.name || "this organization"}`);
    setText(box.querySelector(".ji-text"), `You're invited as ${inv.role === "admin" ? "an admin" : "a member"}${inv.expiresAt ? `. The invite expires ${new Date(inv.expiresAt).toLocaleDateString(undefined, { month: "short", day: "numeric" })}` : ""}.`);
    const me = app.store?.state?.email;
    if (inv.email) {
      const lock = box.querySelector(".ji-lock");
      lock.hidden = false;
      setText(lock, me && me.toLowerCase() !== inv.email.toLowerCase()
        ? `This invite is for ${inv.email}, and you're signed in as ${me}. Sign in with that account to accept it.`
        : `For ${inv.email}.`);
    }
    const accept = box.querySelector(".ji-accept");
    accept.addEventListener("click", async () => {
      accept.disabled = true;
      try {
        const res = await api(`/api/app/invites/${encodeURIComponent(token)}/accept`, { method: "POST", body: {} });
        if (res?.org?.id) setScope(res.org.id);
        toast(`You joined ${res?.org?.name || org.name}`);
        await app.refreshState?.();
        app.navigate?.("/app/settings", { replace: true });
      } catch (err) {
        accept.disabled = false;
        if (err.status === 403) toast(err.message || "This invite is for a different email.", { error: true });
        else if (err.status === 410) fail("This invite has expired", "It was used already or it is more than 7 days old. Ask for a new link.");
        else showError(err);
      }
    });
  }).catch((err) => {
    if (err.status === 401) return;
    if (err.status === 410) fail("This invite has expired", "It was used already or it is more than 7 days old. Ask for a new link.");
    else if (err.status === 404) fail("Invite not found", "Check that you opened the whole link.");
    else fail("Couldn't open the invite", err.message);
  });
  return { el };
}

/* Link Slack --------------------------------------------------------- */

export function SlackLinkScreen(code) {
  const { el, box } = centerScreen();
  const me = app.store?.state?.email;
  const expired = () => {
    box.replaceChildren(h(`<div class="cc-icon">${icon("alert")}</div>`), h(`<h1>This link has expired</h1>`),
      h(`<p>Type <code>/rudder link</code> in Slack for a new one.</p>`),
      h(`<div class="actions"><a class="btn primary block" href="/app" data-link>Go to Rudder</a></div>`));
  };
  box.replaceChildren(h(`<div class="ls-wait"><span class="spin"></span><div class="ls-title">Opening the link…</div></div>`));
  api(`/api/app/link/slack/${encodeURIComponent(code)}`).then((info) => {
    const team = info?.team?.name;
    const who = info?.slackUser;
    box.replaceChildren(h(`<div class="cc-icon">${SLACK_SVG}</div>`), h(`<h1>Link Slack to Rudder</h1>`), h(`<p class="sl-who"></p>`), h(`<p class="sl-text"></p>`),
      h(`<div class="actions"><button type="button" class="btn accent block sl-go">Link my Slack account</button><div class="alt"><a href="/app" data-link>Cancel</a></div></div>`));
    const whoEl = box.querySelector(".sl-who");
    setText(whoEl, [who ? `Slack user ${who}` : null, team ? `in ${team}` : null].filter(Boolean).join(" "));
    whoEl.hidden = !who && !team;
    setText(box.querySelector(".sl-text"), `/rudder in Slack will start tasks and show agents for ${me || "this account"}.`);
    const go = box.querySelector(".sl-go");
    go.addEventListener("click", async () => {
      go.disabled = true;
      try {
        await api(`/api/app/link/slack/${encodeURIComponent(code)}`, { method: "POST", body: {} });
        track("integration_connected", { provider: "slack-user" });
        box.replaceChildren(h(`<div class="cc-icon ok">${icon("check")}</div>`), h(`<h1>Slack is linked</h1>`),
          h(`<p>Try <code>/rudder status</code> in Slack, or <code>/rudder &lt;project&gt; &lt;task&gt;</code> to start an agent.</p>`),
          h(`<div class="actions"><a class="btn primary block" href="/app" data-link>Go to Rudder</a></div>`));
      } catch (err) {
        go.disabled = false;
        if (err.status === 401) return;
        if (err.status === 404 || err.status === 410) return expired();
        const p = box.querySelector(".sl-text");
        p.classList.add("err");
        setText(p, err.message);
      }
    });
  }).catch((err) => {
    if (err.status === 401) return;
    if (err.status === 404 || err.status === 410) return expired();
    box.replaceChildren(h(`<div class="cc-icon">${icon("alert")}</div>`), h(`<h1>Couldn't open the link</h1>`), h(`<p></p>`),
      h(`<div class="actions"><a class="btn primary block" href="/app" data-link>Go to Rudder</a></div>`));
    setText(box.querySelector("p"), err.message);
  });
  return { el };
}

/* Linear issues in the task box -------------------------------------- */

export function openLinearIssues(ws, { onStarted } = {}) {
  const { dlg, body, close } = openSheet({ title: "Start from Linear", className: "linear-sheet" });
  body.append(h(`<label class="search-wrap">${icon("search")}<input class="input repo-search" type="search" placeholder="Search issues" aria-label="Search Linear issues" autocapitalize="off" spellcheck="false"></label>`),
    h(`<div class="repo-list" role="list"><div class="repo-note">Loading your Linear issues…</div></div>`));
  const input = body.querySelector("input");
  const list = body.querySelector(".repo-list");
  const org = currentScope();
  let seq = 0;
  let timer = null;
  async function load(q = "") {
    const mine = ++seq;
    try {
      const qs = new URLSearchParams();
      if (org) qs.set("org", org);
      if (q) qs.set("q", q);
      const res = await api(`/api/app/linear/issues${qs.toString() ? `?${qs}` : ""}`);
      if (mine !== seq || !dlg.open) return;
      const issues = res?.issues || [];
      list.replaceChildren(...issues.map((i) => {
        const b = h(`<button type="button" class="repo-row lin-row" role="listitem"><span class="lin-id"></span><span class="r-text"><span class="rn"></span><span class="rd"></span></span><span class="r-go">Start</span></button>`);
        setText(b.querySelector(".lin-id"), i.identifier || "");
        setText(b.querySelector(".rn"), i.title || "Untitled issue");
        setText(b.querySelector(".rd"), [i.state, i.team, i.priority ? `P${i.priority}` : null].filter(Boolean).join(" · "));
        b.setAttribute("aria-label", `${i.identifier} ${i.title}. Start an agent on it`);
        b.addEventListener("click", async () => {
          b.disabled = true;
          setText(b.querySelector(".r-go"), "Starting…");
          try {
            await api("/api/app/linear/start", { method: "POST", body: { ...scopeBody(org), issueId: i.id, workspace: ws } });
            track("linear_issue_started", { scope: org ? "org" : "personal" });
            track("task_started", { source: "linear" });
            close();
            toast(`Started an agent on ${i.identifier || "the issue"}`);
            onStarted?.();
          } catch (err) { showError(err); b.disabled = false; setText(b.querySelector(".r-go"), "Start"); }
        });
        return b;
      }));
      if (!issues.length) list.appendChild(note(q ? "No matching issues." : "No open issues assigned to you."));
    } catch (err) {
      if (mine !== seq) return;
      list.replaceChildren(note(err.status === 401 ? "Signed out." : err.message, "err"));
    }
  }
  input.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(() => load(input.value.trim()), 250); });
  load();
  if (!NATIVE) setTimeout(() => input.focus(), 50);
}
