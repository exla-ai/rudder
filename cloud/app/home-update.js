// "Update now" for the cloud machine, wherever the app says it runs a Rudder
// too old for something: the All agents chat, plan usage, and any request the
// server marks `needsUpdate` (server.ts homeNeedsUpdate). It starts the same
// update as Alt+r in a dashboard on the machine (POST /api/app/home/update,
// server.ts updateHomeNow) and follows it in /api/app/state until the machine
// is back on the new version; then `onDone` brings the screen back.
//
// The dashboard's rule holds here too: agents mid-turn or holding a question
// for you restart with the machine, so the button says so before it starts
// and the server refuses to interrupt them without that (`interrupt: true`).
import { h, setText } from "/app/render.js";
import { api, openSheet } from "/app/core.js";

const POLL_MS = 2000;

/** Agents an update would interrupt. The same rule as server.ts homeBusyAgents. */
export function busyAgents(state) {
  const projects = [...(state?.projects || []), ...(state?.coordinator ? [state.coordinator] : [])];
  return projects.reduce((n, p) => n + (p.agents || []).filter((a) => a && (a.status === "working" || a.status === "starting" || a.asks?.length
    || (a.status === "waiting" && (a.attention === "input" || a.attention === "permission")))).length, 0);
}

const agents = (n) => (n === 1 ? "1 agent" : `${n} agents`);
const rudder = (version) => (version ? `Rudder ${version}` : "the newest Rudder");
const clock = (ms) => { const s = Math.floor(ms / 1000); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`; };

/**
 * The button and what follows it. `state` is the app's latest /api/app/state
 * (fetched here when not given); `update(state)` keeps the busy count current.
 * `onDone(state)` runs once the machine is connected on the new version.
 */
export function UpdateNow({ state = null, onDone = () => {} } = {}) {
  const el = h(`<div class="hu">
    <p class="hu-fail" role="alert" hidden></p>
    <div class="hu-act"><button type="button" class="btn accent small hu-go"></button></div>
    <p class="hu-line" role="status"><span class="spin" hidden></span><span class="hu-text"></span></p>
  </div>`);
  const failEl = el.querySelector(".hu-fail");
  const go = el.querySelector(".hu-go");
  const spin = el.querySelector(".spin");
  const text = el.querySelector(".hu-text");
  let latest = state;
  let phase = "offer"; // offer | starting | updating | done | failed
  let from = null;     // the version it ran when the update started
  let reason = "";
  let started = false; // whether the failed update got as far as starting
  let startedAt = 0;
  let timer = 0;
  let gone = false;

  const home = () => latest?.home || null;

  function paint() {
    const busy = busyAgents(latest);
    const to = rudder(home()?.updateTo);
    const asking = phase === "offer" || phase === "failed";
    go.parentElement.hidden = !asking;
    go.disabled = phase === "starting";
    setText(go, phase === "failed" ? (busy ? `Try again and restart ${agents(busy)}` : "Try again") : busy ? `Update and restart ${agents(busy)}` : "Update now");
    spin.hidden = phase !== "updating" && phase !== "starting";
    failEl.hidden = phase !== "failed";
    setText(failEl, phase === "failed" ? `The update did not ${started ? "finish" : "start"}: ${reason.replace(/\.$/, "")}.` : "");
    let line = "";
    if (phase === "offer" || phase === "failed") {
      line = busy
        ? `${agents(busy)} ${busy === 1 ? "is" : "are"} working or waiting on you. Updating stops ${busy === 1 ? "it" : "them"} now; ${busy === 1 ? "it restarts" : "they restart"} on ${to} and ${busy === 1 ? "continues" : "continue"}.`
        : `Takes a minute or two. Agents restart on ${to} and pick up where they left off.`;
    } else if (phase === "starting") {
      line = "Starting the update…";
    } else if (phase === "updating") {
      const restarting = home() && !home().connected;
      line = `${restarting ? "Restarting on" : "Updating to"} ${to}. Your cloud machine comes back by itself. ${clock(Date.now() - startedAt)}`;
    } else if (phase === "done") {
      line = `Your cloud machine now runs ${rudder(home()?.version)}.`;
    }
    setText(text, line);
  }

  function fail(message, didStart) {
    phase = "failed";
    started = didStart;
    reason = message || "no reason given";
    clearTimeout(timer);
    paint();
  }

  // Follows the update: done once the machine is connected and reporting
  // live on another version, failed when its host says why it could not.
  async function follow() {
    clearTimeout(timer);
    if (gone) return;
    try {
      latest = await api("/api/app/state");
    } catch (err) {
      if (err.status === 401) return;
      // A missed poll is not a failed update: look again.
    }
    if (gone || phase !== "updating") return;
    const m = home();
    if (m?.updateError) return fail(m.updateError, true);
    if (m?.connected && m.stateLive && !m.updating) {
      if (m.version && m.version !== from) {
        phase = "done";
        paint();
        onDone(latest);
        return;
      }
      return fail(`your cloud machine came back on ${rudder(m.version)}`, true);
    }
    paint();
    timer = setTimeout(follow, POLL_MS);
  }

  function track() {
    phase = "updating";
    from ??= home()?.version || null;
    startedAt ||= Date.now();
    paint();
    timer = setTimeout(follow, POLL_MS);
  }

  go.addEventListener("click", async () => {
    if (phase !== "offer" && phase !== "failed") return;
    const busy = busyAgents(latest);
    from = home()?.version || null;
    startedAt = Date.now();
    phase = "starting";
    paint();
    try {
      const res = await api("/api/app/home/update", { method: "POST", body: { interrupt: busy > 0 } });
      if (res?.home) latest = { ...latest, home: res.home };
      track();
    } catch (err) {
      if (err.status === 401) { phase = "offer"; paint(); return; }
      // One already on its way (another device, or the dashboard): follow it.
      if (/already on its way/.test(err.message)) { track(); return; }
      // The state may have changed under the button (an agent started): show it as it is now.
      latest = await api("/api/app/state").catch(() => latest);
      fail(err.message, false);
    }
  });

  if (latest?.home?.updating) track();
  else paint();
  if (!latest) {
    api("/api/app/state").then((s) => {
      latest = s;
      if (gone) return;
      if (s?.home?.updating && phase === "offer") track();
      else paint();
    }).catch(() => {});
  }

  return {
    el,
    update(next) {
      if (!next || phase === "updating" || phase === "done") return;
      latest = next;
      if (next.home?.updating && phase === "offer") track();
      else paint();
    },
    destroy() { gone = true; clearTimeout(timer); },
  };
}

/**
 * A request the cloud machine is too old for: the server's message and Update
 * now in a sheet. Opened from the error toast's Update now button (core.js).
 */
export function openUpdateSheet(message) {
  const sheet = openSheet({ title: "Update your cloud machine", className: "hu-sheet" });
  const p = h(`<p class="hu-why"></p>`);
  setText(p, message);
  const update = UpdateNow({ onDone: () => setTimeout(() => sheet.close(), 1600) });
  sheet.body.append(p, update.el);
  sheet.dlg.addEventListener("close", () => update.destroy());
}
