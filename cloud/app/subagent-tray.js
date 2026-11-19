// The chat's subagent tray: every subagent and teammate the agent started,
// running and finished, in one strip above the composer, so they stay in reach
// without scrolling up to their cards (cloud/worker/subagents.mjs, GET
// /api/app/subagents). Each entry says what the chat's card says, live: status,
// title, kind and name, how long, tool calls and its latest step
// (subagents.js subagentFacts). An entry opens that subagent's activity in the
// tray, painted as the card's is (paintSubagentActivity). Teammates are listed
// too, whether or not the chat has a card for them. The tray hides to one line,
// remembered per device. A cloud machine too old to list subagents gets one
// row with Update now in the tray's place (home-update.js).

import { h, icon, reconcile, setText } from "/app/render.js";
import { api, ls } from "/app/core.js";
import { UpdateNow } from "/app/home-update.js";
import { paintSubagentActivity, subagentFacts } from "/app/subagents.js";

const SHOWN_KEY = "rudder.subagentTray.shown";
// Something runs or one is open: its steps show as they happen. Otherwise the
// tray only waits for a new subagent.
const POLL_LIVE_MS = 3000;
const POLL_IDLE_MS = 12000;

export function mountSubagentTray({ workspace, run }) {
  const el = h(`<section class="sa-tray" hidden aria-label="Subagents">
    <button type="button" class="sat-toggle">
      <span class="sat-ic">${icon("agents")}</span>
      <span class="sat-title">Subagents</span>
      <span class="sat-count"></span>
      <span class="sat-badge" hidden></span>
      <span class="sat-act"></span>${icon("chevron").replace("<svg", '<svg class="sat-caret"')}
    </button>
    <div class="sat-panel">
      <div class="sat-note" hidden></div>
      <div class="sat-update" hidden><p class="sat-update-text">Update your cloud machine to see subagents here.</p></div>
      <div class="sat-strip" role="group" aria-label="Subagents"></div>
      <div class="sat-open" hidden>
        <div class="sat-open-head">
          <button type="button" class="icon-btn sat-back" aria-label="Back to all subagents" title="All subagents">${icon("back")}</button>
          <div class="sat-open-titles">
            <div class="sat-open-title"></div>
            <div class="sat-open-meta"><span class="sa-state"><span class="dot"></span><span class="sa-label"></span></span><span class="sat-open-facts"></span></div>
          </div>
        </div>
        <div class="sat-open-body"><div class="sat-loading">Loading its activity</div></div>
      </div>
    </div>
  </section>`);
  const toggle = el.querySelector(".sat-toggle");
  const countEl = el.querySelector(".sat-count");
  const badgeEl = el.querySelector(".sat-badge");
  const panel = el.querySelector(".sat-panel");
  const note = el.querySelector(".sat-note");
  const updateRow = el.querySelector(".sat-update");
  const strip = el.querySelector(".sat-strip");
  const openEl = el.querySelector(".sat-open");
  const openBody = el.querySelector(".sat-open-body");
  const loading = el.querySelector(".sat-loading");

  let shown = ls.get(SHOWN_KEY, true) !== false;
  let list = [];
  let error = null;
  let outdated = false; // the cloud machine's Rudder has no subagent list yet
  let updater = null;   // its Update now, while it does not
  let open = null;      // the id of the subagent open in the tray
  let opened = null;    // its activity, once loaded
  let stickBottom = true;
  let timer = null;
  let inflight = false;
  let again = false;
  let destroyed = false;

  toggle.addEventListener("click", () => {
    shown = !shown;
    ls.set(SHOWN_KEY, shown);
    paint();
  });
  strip.addEventListener("click", (e) => {
    const card = e.target.closest(".sat-card");
    if (!card) return;
    open = card.dataset.key;
    opened = null;
    stickBottom = true;
    openBody.querySelector(":scope > .sa-body")?.remove();
    paint();
    poll();
    el.querySelector(".sat-back").focus();
  });
  el.querySelector(".sat-back").addEventListener("click", closeOpen);
  el.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && open && !e.defaultPrevented) { e.preventDefault(); closeOpen(); }
  });
  // A mouse wheel scrolls the strip sideways.
  strip.addEventListener("wheel", (e) => {
    if (Math.abs(e.deltaY) <= Math.abs(e.deltaX) || strip.scrollWidth <= strip.clientWidth) return;
    strip.scrollLeft += e.deltaY;
    e.preventDefault();
  }, { passive: false });
  // The open activity follows new steps while it is scrolled to its end.
  openBody.addEventListener("scroll", () => {
    stickBottom = openBody.scrollTop + openBody.clientHeight >= openBody.scrollHeight - 24;
  }, { passive: true });

  function closeOpen() {
    const id = open;
    open = null;
    opened = null;
    paint();
    strip.querySelector(`.sat-card[data-key="${CSS.escape(id)}"]`)?.focus();
  }

  function paint() {
    const running = list.filter((s) => s.status === "running").length;
    el.hidden = !list.length && !error && !outdated;
    if (el.hidden) return;
    el.classList.toggle("collapsed", !shown);
    el.classList.toggle("live", running > 0);
    el.classList.toggle("outdated", outdated);
    toggle.setAttribute("aria-expanded", String(shown));
    const what = outdated ? "your cloud machine needs an update" : `${list.length}${running ? `, ${running} running` : ""}`;
    toggle.setAttribute("aria-label", `Subagents: ${what}. ${shown ? "Hide" : "Show"}`);
    setText(countEl, outdated || !list.length ? "" : String(list.length));
    const badge = outdated ? "Update needed" : running ? `${running} running` : "";
    badgeEl.hidden = !badge;
    setText(badgeEl, badge);
    setText(el.querySelector(".sat-act"), shown ? "Hide" : "Show");
    panel.hidden = !shown;
    note.hidden = !error;
    setText(note, error || "");
    updateRow.hidden = !outdated;
    if (!shown) return;
    const now = Date.now();
    strip.hidden = outdated || !!open;
    openEl.hidden = outdated || !open;
    if (outdated) return;
    if (open) return paintOpen(now);
    reconcile(strip, list, (s) => s.id, createCard, (node, s) => paintCard(node, s, now));
  }

  function paintOpen(now) {
    const s = opened || list.find((x) => x.id === open);
    if (!s) return;
    const f = trayFacts(s, now);
    openEl.classList.remove("s-working", "s-done", "s-failed", "s-idle");
    openEl.classList.add(f.st.cls);
    setText(openEl.querySelector(".sat-open-title"), f.title);
    openEl.querySelector(".dot").classList.toggle("live", !!f.st.live);
    setText(openEl.querySelector(".sa-label"), f.st.label);
    setText(openEl.querySelector(".sat-open-facts"), [f.who, f.time, f.calls].filter(Boolean).join(" · "));
    loading.hidden = !!opened;
    if (!opened) return;
    paintSubagentActivity(openBody, opened);
    if (stickBottom) openBody.scrollTop = openBody.scrollHeight;
  }

  function createCard() {
    return h(`<button type="button" class="sat-card">
      <span class="sat-c-top"><span class="dot"></span><span class="sat-c-name"></span></span>
      <span class="sat-c-who"></span>
      <span class="sat-c-meta"><span class="sat-c-state"></span><span class="sat-c-facts"></span></span>
      <span class="sat-c-step"></span>
    </button>`);
  }

  function paintCard(node, s, now) {
    const f = trayFacts(s, now);
    const facts = [f.time, f.calls].filter(Boolean).join(" · ");
    // What a finished one came back with reads without its markdown marks.
    const step = !s.step ? (s.status === "running" ? "Starting" : "") : s.status === "running" ? s.step : s.step.replace(/\*\*|`|^#+\s*/g, "");
    const sig = [s.status, f.title, f.who, facts, step].join("|");
    if (node.dataset.sig === sig) return;
    node.dataset.sig = sig;
    node.classList.remove("s-working", "s-done", "s-failed", "s-idle");
    node.classList.add(f.st.cls);
    node.querySelector(".dot").classList.toggle("live", !!f.st.live);
    setText(node.querySelector(".sat-c-name"), f.title);
    const who = node.querySelector(".sat-c-who");
    setText(who, f.who);
    who.hidden = !f.who;
    setText(node.querySelector(".sat-c-state"), f.st.label);
    setText(node.querySelector(".sat-c-facts"), facts ? ` · ${facts}` : "");
    setText(node.querySelector(".sat-c-step"), step);
    node.setAttribute("aria-label", [f.title, f.who, [f.st.label, facts].filter(Boolean).join(", "), step, "Open its activity"].filter(Boolean).join(". "));
  }

  async function poll() {
    if (inflight) { again = true; return; }
    clearTimeout(timer);
    timer = null;
    if (destroyed) return;
    inflight = true;
    const asked = open;
    try {
      const query = `workspace=${encodeURIComponent(workspace)}&run=${encodeURIComponent(run)}${asked ? `&open=${encodeURIComponent(asked)}` : ""}`;
      const r = await api(`/api/app/subagents?${query}`);
      if (destroyed) return;
      list = r.subagents;
      error = null;
      outdated = false;
      updater?.destroy();
      updater?.el.remove();
      updater = null;
      if (asked && asked === open) {
        opened = r.open;
        if (!opened) open = null; // gone from the agent's records
      }
    } catch (err) {
      if (destroyed) return;
      if (err.needsUpdate) {
        outdated = true;
        list = [];
        open = null;
        opened = null;
        error = null;
        if (!updater) {
          updater = UpdateNow({ onDone: () => poll() });
          updateRow.append(updater.el);
        }
      } else {
        // A waking machine: the chat already says so. A sign-out: the app
        // handles it. A machine restarting for its update: Update now says so.
        error = outdated || err.waking || err.status === 401 ? null : err.message;
      }
    } finally {
      inflight = false;
    }
    if (destroyed) return;
    paint();
    if (again) { again = false; poll(); return; }
    const live = open || list.some((s) => s.status === "running");
    timer = setTimeout(tick, live ? POLL_LIVE_MS : POLL_IDLE_MS);
  }
  const tick = () => { if (document.hidden) timer = setTimeout(tick, POLL_IDLE_MS); else poll(); };
  // Running time counts up between polls.
  const clock = setInterval(() => {
    if (!document.hidden && shown && list.some((s) => s.status === "running")) paint();
  }, 1000);
  const onVisible = () => { if (!document.hidden) poll(); };
  document.addEventListener("visibilitychange", onVisible);
  poll();

  return {
    el,
    destroy() {
      destroyed = true;
      clearTimeout(timer);
      clearInterval(clock);
      updater?.destroy();
      document.removeEventListener("visibilitychange", onVisible);
    },
  };
}

// A teammate has no Agent call of its own here to name its kind: it says it
// is one.
function trayFacts(s, now) {
  const f = subagentFacts(s, now);
  return s.type === "teammate" ? { ...f, who: ["Teammate", f.who].filter(Boolean).join(" · ") } : f;
}
