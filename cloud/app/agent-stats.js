// Live numbers beside the work: an agent's output speed in its page header (from the transcript's `stats`, cloud/worker/transcripts.mjs),
// and the plan usage of the cloud machine's Claude and Codex sign-ins in the
// sidebar footer (GET /api/app/usage, src/usage.ts fetchQuota on the home).
// Each shows a popover with the detail: on hover with a mouse, on tap.
import { h, setText } from "/app/render.js";
import { api, coarse, ls, rel, toMs } from "/app/core.js";
import { UpdateNow } from "/app/home-update.js";

const SVG = (body) => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
const GAUGE = SVG('<path d="M4.5 17.5a8.5 8.5 0 1 1 15 0"/><path d="M12 13.5l3.5-4"/><circle cx="12" cy="14" r="1.2" fill="currentColor"/>');
const REFRESH = SVG('<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 4v7h-7"/>');

/* Formatting ------------------------------------------------------------ */

export function formatTokens(n) {
  if (n >= 999_500) return `${Math.round(n / 100_000) / 10}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}K`;
  return String(n);
}
const formatRate = (r) => (r >= 10 ? String(Math.round(r)) : r.toFixed(1));
const formatSeconds = (ms) => (ms < 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 1000)} s`);
const level = (pct) => (pct >= 90 ? "hot" : pct >= 70 ? "warm" : "ok");
// "just now", "5m ago", "Yesterday", "Nov 3".
function ago(t) {
  const r = rel(t);
  return r === "now" ? "just now" : /^\d+[mhd]$/.test(r) ? `${r} ago` : r;
}

function resetText(iso) {
  const at = toMs(iso);
  if (!at) return "";
  const mins = Math.round((at - Date.now()) / 60_000);
  if (mins <= 0) return "Resets now";
  if (mins < 60) return `Resets in ${mins}m`;
  if (mins < 24 * 60) return `Resets in ${Math.floor(mins / 60)}h ${mins % 60}m`;
  return `Resets ${new Date(at).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" })}`;
}

/* Popover ---------------------------------------------------------------- */

// One open at a time. Anchored to its trigger, below it or above when the
// trigger is near the bottom (the sidebar footer). Hover opens it with a
// mouse; a click or tap keeps it open until a click outside or Escape.
let openPop = null;

export function popover(anchor, { label, render, place = "below" }) {
  let el = null, pinned = false, leaveTimer = 0, enterTimer = 0;
  const close = () => {
    clearTimeout(leaveTimer); clearTimeout(enterTimer);
    if (!el) return;
    el.remove(); el = null; pinned = false;
    anchor.setAttribute("aria-expanded", "false");
    if (openPop?.close === close) openPop = null;
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey, true);
  };
  const outside = (e) => { if (el && !el.contains(e.target) && !anchor.contains(e.target)) close(); };
  const onKey = (e) => { if (e.key === "Escape") { close(); anchor.focus(); } };
  const position = () => {
    if (!el) return;
    const a = anchor.getBoundingClientRect();
    const w = el.offsetWidth, hgt = el.offsetHeight;
    const vw = document.documentElement.clientWidth, vh = window.innerHeight;
    const left = Math.max(8, Math.min(vw - w - 8, place === "above" ? a.left : a.right - w));
    const below = a.bottom + 6, above = a.top - hgt - 6;
    const top = place === "above" ? (above >= 8 ? above : below) : (below + hgt <= vh - 8 ? below : Math.max(8, above));
    el.style.left = `${Math.round(left)}px`;
    el.style.top = `${Math.round(top)}px`;
  };
  const paint = () => {
    if (!el) return;
    el.replaceChildren(render());
    position();
  };
  const open = () => {
    if (el) return;
    openPop?.close();
    el = h(`<div class="st-pop" role="dialog"></div>`);
    el.setAttribute("aria-label", label);
    el.addEventListener("pointerenter", () => clearTimeout(leaveTimer));
    el.addEventListener("pointerleave", (e) => { if (e.pointerType === "mouse" && !pinned) leaveTimer = setTimeout(close, 220); });
    document.body.appendChild(el);
    anchor.setAttribute("aria-expanded", "true");
    openPop = { close };
    paint();
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", onKey, true);
  };
  anchor.setAttribute("aria-haspopup", "dialog");
  anchor.setAttribute("aria-expanded", "false");
  anchor.addEventListener("click", () => {
    if (el && pinned) return close();
    open(); pinned = true;
  });
  if (!coarse) {
    anchor.addEventListener("pointerenter", (e) => {
      if (e.pointerType !== "mouse") return;
      clearTimeout(leaveTimer);
      enterTimer = setTimeout(open, 140);
    });
    anchor.addEventListener("pointerleave", (e) => {
      if (e.pointerType !== "mouse") return;
      clearTimeout(enterTimer);
      if (!pinned) leaveTimer = setTimeout(close, 220);
    });
  }
  window.addEventListener("resize", position);
  return {
    paint, close,
    get open() { return !!el; },
    destroy() { close(); window.removeEventListener("resize", position); },
  };
}

/* Agent stats: header line + popover ------------------------------------- */

/**
 * The agent page's output speed, at the end of the header's subtitle
 * `line` ("Working"). Call `update(transcript.stats)` after
 * each paint of the line. Hidden while the transcript has no numbers (a new
 * agent, a backend without session logs, or a teammate's agent). When the
 * home could not work them out (`stats.error`), it says so and the popover
 * gives the reason.
 */
export function AgentStats(line) {
  // Context left has its own readout above the composer (context-meter.js).
  const el = h(`<button type="button" class="as-chip" hidden>
    <span class="as-sep" aria-hidden="true">·</span><span class="as-tps"></span>
  </button>`);
  line.appendChild(el);
  const tpsEl = el.querySelector(".as-tps");
  let stats = null;
  let problem = null;

  const pop = popover(el, { label: "Agent speed", render: () => renderAgentPop(stats, problem) });

  // When the line has no room, the speed gives way whole instead of being
  // cut mid-word. The status stays, and so does "Speed unavailable". The step
  // is a class on the chip (agent-stats.css): the page sets the line's own
  // classes on each paint.
  function fit() {
    el.classList.remove("fit-tps");
    if (el.hidden || problem || !line.clientWidth) return;
    el.classList.add("as-measure");
    const over = line.scrollWidth > line.clientWidth;
    el.classList.remove("as-measure");
    el.classList.toggle("fit-tps", over);
  }
  const resize = new ResizeObserver(fit);
  resize.observe(line);

  function update(next) {
    paint(next);
    fit();
  }

  function paint(next) {
    problem = next?.error || null;
    stats = next && !problem ? next : null;
    const speed = stats?.speed;
    el.hidden = !speed && !problem;
    if (el.hidden) { pop.close(); return; }
    if (problem) {
      setText(tpsEl, "Speed unavailable");
      el.setAttribute("aria-label", "Speed unavailable. Show why");
      if (pop.open) pop.paint();
      return;
    }
    setText(tpsEl, `${formatRate(speed.tokensPerSecond)} tok/s`);
    el.setAttribute("aria-label", `${formatRate(speed.tokensPerSecond)} tokens per second over the last ${speed.responses.length} responses`);
    if (pop.open) pop.paint();
  }

  return { el, update, destroy: () => { resize.disconnect(); pop.destroy(); } };
}

function renderAgentPop(stats, problem) {
  const box = h(`<div class="st-body"></div>`);
  if (problem) {
    const sec = h(`<section class="st-sec"><div class="st-h"><span>Output speed</span></div><p class="st-note"></p></section>`);
    setText(sec.querySelector(".st-note"), `The cloud machine could not work it out: ${problem}`);
    box.appendChild(sec);
    return box;
  }
  if (!stats) return box;
  const speed = stats.speed;
  if (speed) {
    const max = Math.max(...speed.responses.map((r) => r.tokens / r.ms));
    const total = speed.responses.reduce((n, r) => n + r.tokens, 0);
    const ms = speed.responses.reduce((n, r) => n + r.ms, 0);
    const sec = h(`<section class="st-sec">
      <div class="st-h"><span>Output speed</span><b class="mono"></b></div>
      <div class="st-bars" role="img"></div>
      <div class="st-line"></div>
      <p class="st-note">Timed from each request to its last token, so it includes the wait for the first token. Thinking counts as output.</p>
    </section>`);
    setText(sec.querySelector("b"), `${formatRate(speed.tokensPerSecond)} tok/s`);
    const bars = sec.querySelector(".st-bars");
    bars.setAttribute("aria-label", speed.responses.map((r) => `${formatRate((r.tokens / r.ms) * 1000)} tokens per second`).join(", "));
    for (const r of speed.responses) {
      const b = h(`<span class="st-bar"><i></i><em class="mono"></em></span>`);
      b.querySelector("i").style.height = `${Math.max(8, Math.round(((r.tokens / r.ms) / max) * 100))}%`;
      setText(b.querySelector("em"), formatRate((r.tokens / r.ms) * 1000));
      b.title = `${r.tokens.toLocaleString()} tokens in ${formatSeconds(r.ms)}`;
      bars.appendChild(b);
    }
    setText(sec.querySelector(".st-line"), `Last ${speed.responses.length} ${speed.responses.length === 1 ? "response" : "responses"}: ${total.toLocaleString()} tokens in ${formatSeconds(ms)}`);
    box.appendChild(sec);
  }
  if (stats.model) {
    const m = h(`<div class="st-foot mono"></div>`);
    setText(m, stats.model);
    box.appendChild(m);
  }
  return box;
}

/* Plan usage: sidebar meter + popover ------------------------------------ */

// Antigravity's meters are its Gemini plan's, and the sidebar has room for a short name.
const PROVIDERS = [{ id: "claude", label: "Claude" }, { id: "codex", label: "Codex" }, { id: "antigravity", label: "Gemini" }];
const POLL_MS = 60_000;

// The meter can be hidden on this device: "Hide" in its popover, and
// Settings > Appearance shows it again. Every mounted meter follows the choice.
const HIDE_KEY = "rudder.hidePlanUsage";
let usageHidden = ls.get(HIDE_KEY, false) === true;
const usageMeters = new Set();
export const planUsageShown = () => !usageHidden;
export function setPlanUsageShown(shown) {
  usageHidden = !shown;
  ls.set(HIDE_KEY, usageHidden);
  for (const onToggle of usageMeters) onToggle();
}

/**
 * Plan usage for the cloud machine's signed-in Claude, Codex and Antigravity
 * accounts (a harness can hold several, src/accounts.ts): one row each in the sidebar
 * footer (the window closest to its limit), and every
 * window with its reset time in the popover. Usage belongs to the machine's
 * sign-ins, not to a project. When it cannot load, the meter says so and the
 * popover gives the reason. Mount `el` in the footer and call
 * `update(store.state)` when the state changes; it fetches its own numbers.
 */
export function PlanUsage() {
  const el = h(`<button type="button" class="pu" hidden><span class="pu-ic">${GAUGE}</span><span class="pu-rows"></span></button>`);
  const rows = el.querySelector(".pu-rows");
  let report = null;       // the last QuotaReport from the home
  let problem = null;      // why the last fetch failed, shown in the popover
  let updater = null;      // Update now, while the machine is too old to report usage
  let signedIn = [];       // "<provider>:<account>" signed in on the machine
  let timer = 0, inflight = false, stopped = false;

  const pop = popover(el, { label: "Plan usage", place: "above", render: () => renderUsagePop({
    report, problem, updater, signedIn, refresh: () => load(true), hide: () => setPlanUsageShown(false),
  }) });

  const onToggle = () => {
    paint();
    if (usageHidden) clearTimeout(timer);
    else { void load(); schedule(); }
  };
  usageMeters.add(onToggle);

  async function load(force = false) {
    if (inflight || stopped || usageHidden) return;
    inflight = true;
    try {
      report = await api(`/api/app/usage${force ? "?refresh=1" : ""}`);
      problem = null;
      updater?.destroy();
      updater = null;
    } catch (err) {
      if (err.status === 401) return;
      problem = err.status === 503 ? "The cloud machine is asleep. Usage shows once it wakes." : err.message;
      if (err.needsUpdate) updater ??= UpdateNow({ onDone: () => load(true) });
    } finally {
      inflight = false;
      paint();
    }
  }
  function schedule() {
    clearTimeout(timer);
    if (stopped || usageHidden) return;
    timer = setTimeout(() => { if (!document.hidden) void load(); schedule(); }, POLL_MS);
  }
  const onVisible = () => { if (!document.hidden) void load(); };
  document.addEventListener("visibilitychange", onVisible);

  function paint() {
    el.hidden = usageHidden || !signedIn.length || (!report && !problem);
    if (el.hidden) { pop.close(); return; }
    el.classList.toggle("stale", !!(report && problem));
    // Nothing loaded: one plain line, no bars. The popover says why.
    if (!report) {
      const msg = h(`<span class="pu-msg">Plan usage unavailable</span>`);
      rows.replaceChildren(msg);
      el.setAttribute("aria-label", `Plan usage unavailable: ${problem}`);
      if (pop.open) pop.paint();
      return;
    }
    const accounts = reportedAccounts(report, signedIn);
    rows.replaceChildren(...accounts.map(({ label, account }) => {
      const top = bindingWindow(account);
      const row = h(`<span class="pu-row"><span class="pu-name"></span><span class="pu-bar"><i></i></span><span class="pu-pct mono"></span></span>`);
      setText(row.querySelector(".pu-name"), label);
      const bar = row.querySelector(".pu-bar");
      if (top) {
        bar.className = `pu-bar lv-${account.blocked ? "hot" : level(top.percent)}`;
        bar.querySelector("i").style.width = `${top.percent}%`;
        setText(row.querySelector(".pu-pct"), account.blocked ? "Limit" : `${top.percent}%`);
      } else {
        row.classList.add("pu-none");
        setText(row.querySelector(".pu-pct"), "n/a");
      }
      return row;
    }));
    el.setAttribute("aria-label", `Plan usage: ${accounts.map(({ label, account }) => {
      const top = bindingWindow(account);
      return top ? `${label} ${top.percent}% of ${top.label}` : `${label} not available`;
    }).join(", ")}`);
    if (pop.open) pop.paint();
  }

  function update(state) {
    updater?.update(state);
    const creds = state?.home?.credentials;
    // An asleep machine sends no credentials: keep what was last shown.
    if (creds) {
      const next = PROVIDERS.flatMap((p) => (creds[p.id] ?? []).filter((a) => a.ok).map((a) => `${p.id}:${a.id}`));
      if (next.join() !== signedIn.join()) { signedIn = next; paint(); }
    }
  }

  void load();
  schedule();
  return {
    el, update,
    destroy() {
      stopped = true;
      clearTimeout(timer);
      updater?.destroy();
      usageMeters.delete(onToggle);
      document.removeEventListener("visibilitychange", onVisible);
      pop.destroy();
    },
  };
}

const sentence = (text) => text.charAt(0).toUpperCase() + text.slice(1);

// The signed-in accounts the report has, in display order. A harness with
// more than one names each by its email.
function reportedAccounts(report, signedIn) {
  return PROVIDERS.flatMap((p) => {
    const mine = (report.accounts ?? []).filter((a) => a.provider === p.id && signedIn.includes(`${p.id}:${a.account}`));
    return mine.map((account) => ({
      label: mine.length > 1 && account.email ? `${p.label} · ${account.email.split("@")[0]}` : p.label,
      account,
    }));
  });
}

// The window closest to its limit decides when the account is blocked.
// Extra usage is paid overage, not a limit, so it never decides.
function bindingWindow(account) {
  const windows = (account?.windows || []).filter((w) => w.label !== "Extra usage");
  return windows.reduce((top, w) => (!top || w.percent > top.percent ? w : top), null);
}

function renderUsagePop({ report, problem, updater, signedIn, refresh, hide }) {
  const box = h(`<div class="st-body pu-pop"></div>`);
  if (!report) {
    const e = h(`<div class="pu-fail"></div>`);
    setText(e, sentence(problem));
    box.appendChild(e);
    if (updater) box.appendChild(updater.el);
  }
  for (const { label, account } of report ? reportedAccounts(report, signedIn) : []) {
    const sec = h(`<section class="st-sec">
      <div class="st-h"><span class="pu-acct"></span><b class="pu-plan"></b></div>
      <div class="pu-email"></div>
      <div class="pu-wins"></div>
    </section>`);
    setText(sec.querySelector(".pu-acct"), label);
    setText(sec.querySelector(".pu-plan"), account.plan || "");
    setText(sec.querySelector(".pu-email"), account.email || "");
    const wins = sec.querySelector(".pu-wins");
    if (account.blocked) {
      const b = h(`<div class="pu-blocked"></div>`);
      setText(b, sentence(account.blocked));
      wins.appendChild(b);
    }
    for (const w of account.windows || []) {
      const extra = w.label === "Extra usage";
      const row = h(`<div class="pu-win"><div class="pu-wh"><span></span><b class="mono"></b></div><div class="st-meter"><i></i></div><div class="pu-reset"></div></div>`);
      setText(row.querySelector(".pu-wh span"), w.label);
      setText(row.querySelector(".pu-wh b"), `${w.percent}% used`);
      row.querySelector(".st-meter").className = `st-meter lv-${extra ? "ok" : level(w.percent)}`;
      row.querySelector(".st-meter i").style.width = `${Math.min(100, w.percent)}%`;
      setText(row.querySelector(".pu-reset"), resetText(w.resetsAt));
      wins.appendChild(row);
    }
    if (account.error) {
      const e = h(`<div class="pu-err"></div>`);
      setText(e, `Usage unavailable: ${account.error}`);
      wins.appendChild(e);
    }
    box.appendChild(sec);
  }
  const foot = h(`<div class="st-foot pu-foot"><span class="pu-when"></span><button type="button" class="pu-hide" title="Show it again in Settings">Hide</button><button type="button" class="pu-refresh" aria-label="Refresh plan usage" title="Refresh">${REFRESH}</button></div>`);
  // Last data with a failed refresh: the bars are from the last good check,
  // and the foot says why the refresh failed.
  const when = report?.fetchedAt ? `Checked with the providers ${ago(report.fetchedAt)}` : "";
  setText(foot.querySelector(".pu-when"), report && problem ? sentence(problem) : when);
  foot.querySelector(".pu-hide").addEventListener("click", (e) => { e.stopPropagation(); hide(); });
  foot.querySelector(".pu-refresh").addEventListener("click", (e) => { e.stopPropagation(); refresh(); });
  box.appendChild(foot);
  return box;
}
