// Team: what your teammates' agents are doing (cloud/src/team.ts).
// - /app/team lists every member of your orgs (or ?org=<id>), grouped by
//   person then project, from the state their machines last reported.
// - A teammate's agent opens READ ONLY at /app/team/<member>/<ws>/<run>
//   (app.js AgentScreen with a member): no composer, no prompts, no actions.
// - Private agents show only as a muted "Private agent" row with a status.
import { h, icon, setText, reconcile } from "/app/render.js";
import { app, api, rel } from "/app/core.js";
import { screenShell, initials, note } from "/app/org.js";

const enc = encodeURIComponent;
export const teamPath = (org) => (org ? `/app/team?org=${enc(org)}` : "/app/team");
export const teamAgentPath = (member, ws, run) => `/app/team/${enc(member)}/${enc(ws)}/${enc(run)}`;
export const hasTeam = (state) => Array.isArray(state?.orgs) && state.orgs.length > 0;

/* Cache ---------------------------------------------------------------- */

const cache = { data: null, org: null, at: 0, inflight: null, error: null };

/** Load the team view (one org, or all of yours). Never throws. */
export async function loadTeam(org = cache.org) {
  if (cache.inflight && cache.inflight.org === org) return cache.inflight.p;
  const p = (async () => {
    try {
      const data = await api(`/api/app/team${org ? `?org=${enc(org)}` : ""}`);
      cache.data = data; cache.org = org; cache.at = Date.now(); cache.error = null;
    } catch (err) {
      cache.error = err;
      if (err.status === 404 && org) { cache.org = null; cache.data = null; }
    } finally {
      cache.inflight = null;
    }
    return cache.data;
  })();
  cache.inflight = { org, p };
  return p;
}

/** The team view that includes this member (an org-filtered one may not). */
export async function loadTeamFor(member) {
  await loadTeam();
  if (cache.org && !cache.data?.members?.some((m) => m.accountId === member)) await loadTeam(null);
}

/** A teammate's agent from the last team view, shaped like one of your own
 * (asks carry only the kind, so the status reads "Needs approval" etc.). */
export function teamAgent(member, ws, run) {
  const m = cache.data?.members?.find((x) => x.accountId === member);
  const p = m?.projects?.find((x) => x.workspaceId === ws);
  const a = p?.agents?.find((x) => x.id === run);
  if (!m) return null;
  return {
    member: m,
    project: p || null,
    agent: a ? { ...a, asks: a.asking ? [{ id: `team-${a.id}`, kind: a.asking }] : [] } : null,
  };
}
export const memberName = (m) => m?.name || m?.email || "Your teammate";
export const firstName = (m) => (m?.name ? m.name.split(/\s+/)[0] : m?.email ? m.email.split("@")[0] : "Your teammate");

/* Screen --------------------------------------------------------------- */

const LIVE = new Set(["working", "starting"]);

function memberSummary(m) {
  let live = 0, needs = 0, total = 0;
  for (const p of m.projects || []) {
    for (const a of p.agents || []) {
      total += 1;
      // needsYou is the control plane's one rule; review and failures wait on
      // a person too, but "waiting on <name>" means blocked mid-turn.
      const blocked = typeof a.needsYou === "boolean" ? a.needsYou && a.reason !== "review" && a.reason !== "failed" : a.status === "waiting";
      if (a.asking || blocked) needs += 1;
      else if (LIVE.has(a.status)) live += 1;
    }
  }
  const bits = [];
  if (live) bits.push(`${live} working`);
  if (needs) bits.push(`${needs} waiting on ${firstName(m)}`);
  if (!bits.length && total) bits.push(`${total} ${total === 1 ? "agent" : "agents"}, none working`);
  return bits.join(" · ");
}

function createRow(x) {
  if (x.a.private) {
    return h(`<div class="row agent tm-private" role="listitem">
      <span class="a-dot"><span class="dot"></span></span>
      <div class="a-body"><div class="a-top"><span class="a-title"><span class="tm-lock">${icon("lock")}</span><span>Private agent</span></span><span class="a-time"></span></div>
      <div class="a-meta"><span class="status-text"></span></div></div></div>`);
  }
  return h(`<a class="row agent tap" data-link role="listitem">
    <span class="a-dot"><span class="dot"></span></span>
    <div class="a-body"><div class="a-top"><span class="a-title"></span><span class="a-time"></span></div>
    <div class="a-meta"><span class="status-text"></span><span class="chip c-model"></span><span class="chip mono c-phase"></span></div></div></a>`);
}

function updateRow(el, { m, p, a }) {
  const st = app.teamStatus(a);
  el.className = `row agent ${a.private ? "tm-private" : "tap"} ${st.cls}`;
  el.querySelector(".dot").classList.toggle("live", !!st.live);
  setText(el.querySelector(".a-time"), rel(a.updatedAt));
  setText(el.querySelector(".status-text"), st.label);
  if (a.private) {
    el.setAttribute("aria-label", `Private agent, ${st.label}, ${p.name}. Only ${firstName(m)} can see it.`);
    el.title = `Only ${firstName(m)} can see this agent`;
    return;
  }
  el.href = teamAgentPath(m.accountId, p.workspaceId, a.id);
  setText(el.querySelector(".a-title"), a.title || "Untitled agent");
  const model = el.querySelector(".c-model");
  const bk = app.backendLabel(a);
  setText(model, bk); model.hidden = !bk;
  const phase = el.querySelector(".c-phase");
  const phaseText = a.phase && a.phase !== a.status ? a.phase : "";
  setText(phase, phaseText); phase.hidden = !phaseText;
  el.setAttribute("aria-label", `${a.title || "Agent"}, ${st.label}, ${p.name}, ${memberName(m)}'s agent, read only`);
}

const MACHINE = {
  awake: null,
  asleep: { label: "Asleep", cls: "s-idle" },
  none: { label: "No cloud machine", cls: "s-idle" },
};

export function TeamScreen() {
  const { el, body } = screenShell("Team", { back: "/app/settings" });
  el.classList.add("team-screen");
  const refresh = h(`<button type="button" class="icon-btn" aria-label="Refresh">${icon("refresh")}</button>`);
  refresh.addEventListener("click", async () => {
    refresh.classList.add("spinning");
    const t0 = Date.now();
    try { await load(); } finally { setTimeout(() => refresh.classList.remove("spinning"), Math.max(0, 500 - (Date.now() - t0))); }
  });
  el.querySelector(".topbar").appendChild(refresh);

  const params = new URLSearchParams(location.search);
  let org = params.get("org") || null;

  const orgBar = h(`<div class="c-tools tm-orgs" role="radiogroup" aria-label="Organization"></div>`);
  const intro = h(`<p class="tm-intro">What your teammates' agents are doing, as their cloud machines last reported. You can read a teammate's agent but not message or change it. Private agents show only their status.</p>`);
  const list = h(`<div class="tm-list"></div>`);
  body.append(orgBar, intro, list);
  list.appendChild(h(`<div class="section"><div class="sk sk-card" style="height:64px"></div><div class="sk sk-card" style="height:150px;margin-top:10px"></div></div>`));

  function paintOrgs(orgs) {
    orgBar.hidden = orgs.length < 2;
    if (orgs.length < 2) return;
    const opts = [{ id: null, name: "Everyone" }, ...orgs];
    reconcile(orgBar, opts, (o) => o.id || "all", () => h(`<button type="button" role="radio" class="tool-chip"></button>`), (b, o) => {
      setText(b, o.name);
      b.setAttribute("aria-checked", String((o.id || null) === org));
      b.classList.toggle("on", (o.id || null) === org);
      b.onclick = () => {
        if ((o.id || null) === org) return;
        org = o.id || null;
        history.replaceState(history.state, "", `/app/team${org ? `?org=${enc(org)}` : ""}`);
        load();
      };
    });
  }

  function paint() {
    const data = cache.org === org ? cache.data : null;
    const err = cache.error;
    if (!data) {
      if (err && !cache.inflight) {
        list.replaceChildren(err.status === 404
          ? emptyBox("users", "Not in that organization", "You may have left it, or it was deleted.", h(`<a class="btn" href="/app/team" data-link>All your teammates</a>`))
          : note(err.message, "err"));
      }
      return;
    }
    paintOrgs(data.orgs || []);
    if (!(data.orgs || []).length) {
      list.replaceChildren(emptyBox("users", "No team yet", "Create an organization in Settings and invite people. Then you can see what each person's agents are doing here.",
        h(`<a class="btn accent" href="/app/settings" data-link>Open Settings</a>`)));
      return;
    }
    const members = data.members || [];
    if (!members.length) {
      list.replaceChildren(emptyBox("users", "Just you so far", "Invite people from Settings. Once they join, their agents show up here.",
        h(`<a class="btn" href="/app/settings" data-link>Invite people</a>`)));
      return;
    }
    for (const stray of Array.from(list.children)) if (!stray.classList.contains("tm")) stray.remove();
    reconcile(list, members, (m) => m.accountId, () => h(`<div class="section tm">
        <div class="tm-head"><span class="m-av"></span><span class="tm-who"><span class="tm-name"></span><span class="tm-sub"></span></span><span class="pill tm-machine"><span class="dot"></span><span class="pl"></span></span></div>
        <div class="tm-projects"></div></div>`),
    (sec, m) => {
      setText(sec.querySelector(".m-av"), initials(m.name || m.email || "?"));
      setText(sec.querySelector(".tm-name"), memberName(m));
      const sub = [m.name && m.email ? m.email : "", memberSummary(m)].filter(Boolean).join(" · ");
      setText(sec.querySelector(".tm-sub"), sub);
      const pill = sec.querySelector(".tm-machine");
      const mach = MACHINE[m.machine];
      pill.hidden = !mach;
      if (mach) { pill.className = `pill tm-machine ${mach.cls}`; setText(pill.querySelector(".pl"), mach.label); pill.title = m.machine === "asleep" ? "Showing what it last reported. Looking here never wakes it." : ""; }
      const projectsEl = sec.querySelector(".tm-projects");
      const projects = m.projects || [];
      if (!projects.length) {
        projectsEl.replaceChildren(h(`<div class="tm-none">No agents right now</div>`));
        return;
      }
      projectsEl.querySelector(".tm-none")?.remove();
      reconcile(projectsEl, projects, (p) => p.workspaceId, () => h(`<div class="tm-proj">
          <div class="tm-pname">${icon("folder")}<span class="tm-pn"></span><span class="tm-repo"></span><span class="count"></span></div>
          <div class="list grouped card" role="list"></div></div>`),
      (pe, p) => {
        setText(pe.querySelector(".tm-pn"), p.name);
        const repo = pe.querySelector(".tm-repo");
        setText(repo, p.repo && p.repo !== p.name ? p.repo : ""); repo.hidden = !p.repo || p.repo === p.name;
        setText(pe.querySelector(".count"), String(p.agents.length));
        pe.querySelector(".list").setAttribute("aria-label", `${memberName(m)}'s agents in ${p.name}`);
        reconcile(pe.querySelector(".list"), p.agents.map((a) => ({ m, p, a })), (x) => `${x.a.private ? "p" : "t"}:${x.a.id}`, createRow, updateRow);
      });
    });
  }

  async function load() {
    await loadTeam(org);
    paint();
  }

  // The same cadence as the app's state poll; a hidden tab does not poll.
  let timer = null;
  const tick = async () => {
    timer = null;
    if (!document.hidden) await load();
    if (timer !== -1) timer = setTimeout(tick, 4000);
  };
  const onVis = () => { if (!document.hidden && timer !== -1) { clearTimeout(timer); tick(); } };
  document.addEventListener("visibilitychange", onVis);
  if (cache.data && cache.org === org) paint();
  tick();
  return {
    el,
    update: () => paint(),
    destroy() { clearTimeout(timer); timer = -1; document.removeEventListener("visibilitychange", onVis); },
  };
}

function emptyBox(ic, title, text, action) {
  const el = h(`<div class="empty"><div class="e-icon">${icon(ic)}</div><h3></h3><p></p></div>`);
  setText(el.querySelector("h3"), title);
  setText(el.querySelector("p"), text);
  if (action) el.appendChild(action);
  return el;
}

