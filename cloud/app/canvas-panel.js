// The Canvas panel: the project's vibeprint canvas beside the conversation.
// It takes the right-hand slot it shares with Files, the terminal and Notes
// (side-panel.js): a desktop column, or narrower a full sheet.
//
// The canvas runs on the home beside the agents, never inside them
// (cloud/worker/vibeprint-host.mjs): no agent has its rule, tools or files.
// One canvas per project; the panel follows the selected agent, and "Send to
// agent" is the only way the canvas reaches one: its brief, as a message the
// user chooses to send. The other way round, an agent with tracking on (the eye;
// sending it the canvas turns it on) has its changes checked against the canvas
// after its turns (cloud/worker/vibeprint-track.mjs). The frame is vibeprint's
// own page through its relay, signed in with the token in the link's fragment
// (frames get no cookies).
import { api, confirmDialog, toast, ls } from "/app/core.js";
import { h, icon, copyText, escapeHtml } from "/app/render.js";
import { openSide, closeSide, placeSide, dragColumn } from "/app/side-panel.js";

const STARTING_POLL_MS = 1500;
const RUNNING_POLL_MS = 30000;
const sentKey = (ws, run) => `rudder.canvasSent.${ws}.${run}`;
const CANVAS_ICON = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="7" height="6" rx="1.5"/><rect x="14" y="14" width="7" height="6" rx="1.5"/><path d="M6.5 10v3.5a2 2 0 0 0 2 2H14"/><path d="M17 4.5c1.6 1.2 2.6 2.9 2.5 5"/></svg>`;

const state = {
  open: false,
  target: null,  // { ws, run, findAgent }
  view: null,    // the home's answer: { state, url?, token?, error?, note? }
  frameKey: "",  // the canvas the frame shows (ws + url)
  timer: 0,
  busy: false,
};

const el = h(`<section class="cv" aria-label="Canvas">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="cv-head">
    <span class="cv-title">${CANVAS_ICON}<span>Canvas</span></span>
    <span class="cv-agent" hidden></span>
    <span class="cv-actions">
      <button type="button" class="btn cv-send" hidden>Send to agent</button>
      <button type="button" class="icon-btn cv-track" aria-pressed="false" hidden>${icon("eye")}</button>
      <button type="button" class="icon-btn cv-link" aria-label="Copy the iPad link" title="Copy the iPad link" hidden>${icon("copy")}</button>
      <button type="button" class="icon-btn cv-off" aria-label="Turn the canvas off" title="Turn the canvas off" hidden>${icon("power")}</button>
      <button type="button" class="icon-btn cv-close" aria-label="Close canvas" title="Close canvas">${icon("x")}</button>
    </span>
  </header>
  <div class="cv-body"></div>
</section>`);
const $ = (s) => el.querySelector(s);
const bodyEl = $(".cv-body"), agentEl = $(".cv-agent"), sendBtn = $(".cv-send"), trackBtn = $(".cv-track"), linkBtn = $(".cv-link"), offBtn = $(".cv-off");

$(".cv-close").addEventListener("click", () => closePanel());
sendBtn.addEventListener("click", () => { void sendToAgent(); });
trackBtn.addEventListener("click", () => { void toggleTracking(); });
linkBtn.addEventListener("click", () => {
  const v = state.view;
  if (v?.state !== "running") return;
  // ?t= signs the iPad in once (a cookie); then Share > Add to Home Screen.
  void copyText(`${v.url}?t=${encodeURIComponent(v.token)}`).then((ok) => {
    if (ok) toast("Copied the canvas link. Open it on the iPad.");
    else toast("Couldn't copy the canvas link", { error: true });
  });
});
offBtn.addEventListener("click", () => { void turnOff(); });
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible" && state.open) void refresh(); });

// ---------- The right-hand slot (side-panel.js) ----------

// Another panel taking the slot closes the canvas.
const panel = { el, close: async () => { stopPanel(); return true; } };
$(".side-grip").addEventListener("pointerdown", dragColumn);

// ---------- Public surface ----------

/** The topbar Canvas button of a project or agent screen (`run` null for the project). */
export function canvasButton({ ws, run = null, findAgent }) {
  const b = h(`<button type="button" class="icon-btn cv-toggle">${CANVAS_ICON}</button>`);
  b.setAttribute("aria-label", "Canvas");
  b.title = "Canvas";
  b.addEventListener("click", () => { void togglePanel({ ws, run, findAgent }); });
  return b;
}

/** The route changed, or the other pane took focus: an open canvas follows that
 * project and agent. Another panel taking the slot closes it. */
export function followCanvas(route, findAgent) {
  if (!state.open) return;
  placeSide(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  if (!route?.ws || !["project", "agent"].includes(route.name)) { closePanel(); return; }
  retarget({ ws: route.ws, run: route.name === "agent" ? route.run : null, findAgent });
}

async function togglePanel(target) {
  if (state.open && state.target?.ws === target.ws && state.target?.run === target.run) { closePanel(); return; }
  if (!(await openSide(panel))) return;
  state.open = true;
  retarget(target);
}

function closePanel() {
  stopPanel();
  closeSide(panel);
}

function stopPanel() {
  state.open = false;
  clearTimeout(state.timer);
}

function retarget(target) {
  const project = state.target?.ws !== target.ws;
  state.target = target;
  paintHead();
  if (project) {
    state.view = null;
    state.frameKey = "";
    bodyEl.replaceChildren(h(`<div class="cv-empty">Looking for this project's canvas</div>`));
  }
  void refresh();
}

// ---------- The home's canvas ----------

async function refresh() {
  clearTimeout(state.timer);
  const t = state.target;
  if (!state.open || !t?.ws) return;
  let view;
  try {
    view = await api(`/api/app/vibeprint?${new URLSearchParams({ workspace: t.ws, ...(t.run ? { run: t.run } : {}) })}`);
  } catch (e) {
    view = { state: "failed", error: e?.message || "The canvas is unavailable" };
  }
  // A newer target (another agent of the same project too) has its own answer coming.
  if (!state.open || state.target !== t) return;
  show(view);
  const wait = view.state === "starting" ? STARTING_POLL_MS : view.state === "running" ? RUNNING_POLL_MS : 0;
  // A hidden page skips the poll; visibilitychange refreshes when it shows again.
  if (wait) state.timer = setTimeout(() => { if (document.visibilityState === "visible") void refresh(); }, wait);
}

async function request(op, extra = {}) {
  const t = state.target;
  if (!t?.ws || state.busy) return null;
  state.busy = true;
  try {
    return await api("/api/app/vibeprint", { method: "POST", body: { workspace: t.ws, op, ...(t.run ? { run: t.run } : {}), ...extra } });
  } finally { state.busy = false; }
}

/** The eye: this agent's turns update the canvas, or stop. Turning it on checks its changes now. */
async function toggleTracking() {
  const t = state.target;
  if (!t?.run) return;
  const on = !state.view?.tracking?.on;
  const name = agentName();
  let view;
  try { view = await request("tracking", { on, check: on }); } catch (e) { toast(e?.message || "Could not change that", { error: true }); return; }
  if (!view || state.target?.ws !== t.ws || state.target?.run !== t.run) return;
  show(view);
  toast(on ? `Checking ${name}'s changes against the canvas, and again after its turns` : `${name}'s turns no longer update the canvas`);
}

async function turnOn() {
  try {
    const view = await request("start");
    if (view) show(view);
  } catch (e) { show({ state: "failed", error: e?.message || "The canvas could not start" }); return; }
  void refresh();
}

async function turnOff() {
  const ok = await confirmDialog({ title: "Turn the canvas off?", body: "The canvas stops on the home and its iPad link stops working until you turn it on again. Your drawing is kept.", confirm: "Turn off" });
  if (!ok) return;
  let view;
  try { view = await request("stop"); } catch (e) { toast(e?.message || "Could not turn the canvas off", { error: true }); return; }
  // null: another canvas request was still running, so nothing was sent.
  if (view) show(view);
}

function show(view) {
  state.view = view;
  paintHead();
  if (view.state === "running") {
    const key = `${state.target.ws} ${view.url}`;
    if (state.frameKey === key && bodyEl.querySelector("iframe")) return;
    state.frameKey = key;
    const frame = document.createElement("iframe");
    frame.className = "cv-frame";
    frame.title = "Vibeprint canvas";
    frame.allow = "clipboard-read; clipboard-write; fullscreen";
    frame.referrerPolicy = "no-referrer";
    // pad: the canvas without vibeprint's own chat column; the token stays in the fragment.
    frame.src = `${view.url}?view=pad#t=${encodeURIComponent(view.token)}`;
    bodyEl.replaceChildren(frame);
    return;
  }
  state.frameKey = "";
  if (view.state === "starting") {
    bodyEl.replaceChildren(h(`<div class="cv-empty"><span class="cv-spin" aria-hidden="true"></span><span>Starting the canvas${view.note ? `. ${escapeHtml(view.note)}` : ""}</span></div>`));
    return;
  }
  if (view.state === "failed") {
    const card = h(`<div class="cv-card"><p class="cv-error"></p><button type="button" class="btn primary">Try again</button></div>`);
    card.querySelector(".cv-error").textContent = view.error;
    card.querySelector("button").addEventListener("click", () => { void turnOn(); });
    bodyEl.replaceChildren(card);
    return;
  }
  const card = h(`<div class="cv-card">
    <h3>Draw this project's architecture</h3>
    <p>A vibeprint canvas runs on the home beside your agents. They don't see it; send it to an agent when you want it to build against it.</p>
    <button type="button" class="btn primary">Start the canvas</button>
  </div>`);
  card.querySelector("button").addEventListener("click", () => { void turnOn(); });
  bodyEl.replaceChildren(card);
}

function agentName() {
  const t = state.target;
  return t?.run ? t.findAgent?.(t.ws, t.run)?.title || "this agent" : null;
}

function paintHead() {
  const name = agentName();
  const running = state.view?.state === "running";
  agentEl.hidden = !name;
  agentEl.textContent = name ? `with ${name}` : "";
  sendBtn.hidden = !running || !name;
  sendBtn.title = name ? `Send the canvas to ${name} as a message` : "";
  const tracking = state.view?.tracking;
  trackBtn.hidden = !running || !name;
  trackBtn.setAttribute("aria-pressed", tracking?.on ? "true" : "false");
  trackBtn.classList.toggle("cv-track-error", !!tracking?.error);
  const last = tracking?.lastAt ? ` Last checked at ${new Date(tracking.lastAt).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.` : "";
  const label = !name ? "" : tracking?.on
    ? `${name}'s changes update the canvas after its turns.${tracking.pending ? " A check is due." : last}${tracking.error ? ` The last check failed: ${tracking.error}` : ""} Click to stop.`
    : `Update the canvas from ${name}'s changes after each turn`;
  trackBtn.title = label;
  trackBtn.setAttribute("aria-label", label);
  linkBtn.hidden = !running;
  offBtn.hidden = !running;
}

// ---------- Send to agent ----------

async function sendToAgent() {
  const t = state.target;
  if (!t?.run) return;
  const name = agentName();
  const since = ls.get(sentKey(t.ws, t.run));
  let brief;
  try {
    brief = await api("/api/app/vibeprint", { method: "POST", body: { workspace: t.ws, op: "brief", ...(since ? { since } : {}) } });
  } catch (e) { toast(e?.message || "Could not read the canvas", { error: true }); return; }
  if (!brief.components) { toast("Nothing is on the canvas yet. Draw and Submit it first."); return; }
  const ok = await previewDialog(`Send the canvas to ${name}?`, brief.text);
  if (!ok || state.target !== t) return;
  try {
    await api("/api/app/send", { method: "POST", body: { workspace: t.ws, run: t.run, text: brief.text } });
  } catch (e) { toast(e?.message || "Could not send the canvas", { error: true }); return; }
  ls.set(sentKey(t.ws, t.run), brief.at);
  // It builds against the canvas now, so its turns update it (from its next change on).
  const tracked = !!state.view?.tracking?.on;
  if (!tracked) {
    try {
      const view = await api("/api/app/vibeprint", { method: "POST", body: { workspace: t.ws, run: t.run, op: "tracking", on: true, check: false } });
      if (state.target?.ws === t.ws && state.target?.run === t.run) show(view);
    } catch (e) { toast(`Sent the canvas to ${name}, but its turns won't update it: ${e?.message || "tracking failed"}`, { error: true }); return; }
  }
  toast(tracked ? `Sent the canvas to ${name}` : `Sent the canvas to ${name}. Its turns now update the canvas.`);
}

/** The message exactly as the agent will get it, with Send and Cancel. */
function previewDialog(title, text) {
  return new Promise((resolve) => {
    const dlg = h(`<dialog class="cv-dlg" aria-labelledby="cv-dlg-t"><div class="dialog">
      <h3 id="cv-dlg-t"></h3><div class="cv-brief"></div>
      <div class="d-actions"><button type="button" class="btn" data-v="0">Cancel</button>
      <button type="button" class="btn primary" data-v="1">Send</button></div></div></dialog>`);
    dlg.querySelector("h3").textContent = title;
    dlg.querySelector(".cv-brief").textContent = text;
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
