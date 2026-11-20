// The tour: Rudder's one first-run flow. app.js opens it once per device, the
// first time the home screen shows after signing in, and Settings > Show the
// tour opens it again. Each step pairs a few words with a scene: a small copy
// of the app's own screens, drawn with the app's tokens (tutorial.css), that
// plays what the step says once and then rests on its last frame. With
// reduced motion a scene shows that last frame straight away.
// Arrow keys, Enter and a swipe move through it; Escape or Skip closes it.
// Product events carry the step number and step id, nothing typed.
import { h, icon, setText } from "/app/render.js";
import { ls, PLATFORM, coarse, track } from "/app/core.js";
import { harnessLogo } from "/app/harness-logos.js";
import { MAC_ICON } from "/app/mac-local.js";

// ?from=app: the site counts downloads that came from the app.
export const DOWNLOAD_MAC_URL = "https://rudder.exla.ai/download/mac?from=app";
export const PHONE_APP_URL = "https://rudder.exla.ai/download/ios";

// Finished or skipped on this device. Signing out clears it with the rest of
// rudder.* (app.js forgetAccount), so the next account here sees the tour too.
const SEEN_KEY = "rudder.tourSeen";
const ALT = /Mac/.test(navigator.platform) ? "Option" : "Alt";
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
// Every scene is drawn on a board this size and scaled to the stage. A
// narrow stage (a phone) gets a narrower board: the window scenes drop the
// sidebar, as the phone app has none, and the rest close up.
const BOARD_W = 560;
const BOARD_W_NARROW = 410;
const BOARD_H = 420;
const NARROW_STAGE = 520;

const PHONE_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="7" y="2.5" width="10" height="19" rx="2.5"/><path d="M11 18.5h2"/></svg>';
const TS_DOTS = '<svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><circle cx="5" cy="5" r="2.3" opacity=".35"/><circle cx="12" cy="5" r="2.3" opacity=".35"/><circle cx="19" cy="5" r="2.3" opacity=".35"/><circle cx="5" cy="12" r="2.3"/><circle cx="12" cy="12" r="2.3"/><circle cx="19" cy="12" r="2.3"/><circle cx="5" cy="19" r="2.3" opacity=".35"/><circle cx="12" cy="19" r="2.3"/><circle cx="19" cy="19" r="2.3" opacity=".35"/></svg>';

/** The tour has not been finished or skipped on this device. */
export const tourDue = () => !ls.get(SEEN_KEY);

/* Scenes ------------------------------------------------------------- */

// The desktop app's window: the sidebar of projects and agents, then `main`.
// `agents` are the rudder project's rows: [title, status class, extra class].
function windowScene(main, agents, { cls = "" } = {}) {
  const rows = agents.map(([t, st, extra = ""]) =>
    `<div class="tw-ag ${st} ${extra}"><span class="dot"></span><span class="tw-agt">${t}</span></div>`).join("");
  return `<div class="tw ${cls}">
    <div class="tw-side">
      <div class="tw-lights"><i></i><i></i><i></i></div>
      <div class="tw-brand"><img src="/app/icon.svg" alt=""><b>Rudder</b></div>
      <div class="tw-label">Projects</div>
      <div class="tw-proj on">${icon("chevron")}<span>rudder</span></div>
      <div class="tw-ags">${rows}</div>
      <div class="tw-proj">${icon("chevron")}<span>wagerpals</span></div>
      <div class="tw-proj">${icon("chevron")}<span>rudder-site</span></div>
    </div>
    <div class="tw-main">${main}</div>
  </div>`;
}

function welcomeScene({ narrow }) {
  const here = PLATFORM === "mac" ? "mac" : PLATFORM === "ios" ? "phone" : "web";
  // The devices in a column left of the machine, or in a row above it.
  const at = narrow ? [[0, 22], [144, 22], [288, 22]] : [[20, 88], [20, 186], [20, 284]];
  const dev = (id, svg, label, i) =>
    `<div class="trw-dev${id === here ? " here" : ""}" style="left:${at[i][0]}px;top:${at[i][1]}px">${svg}<span>${label}</span></div>`;
  const lines = narrow
    ? ["M61 66 C 61 100, 205 96, 205 128", "M205 66 L 205 128", "M349 66 C 349 100, 205 96, 205 128"]
    : ["M150 112 C 196 112, 196 210, 236 210", "M150 210 L 236 210", "M150 308 C 196 308, 196 210, 236 210"];
  const agent = (logo, title, st, label, i) =>
    `<div class="trw-row ${st}" style="--i:${i}"><span class="dot live"></span><span class="trw-t">${title}</span><span class="trw-st">${label}</span>${harnessLogo(logo)}</div>`;
  return `<div class="tr-art tr-welcome">
    <div class="trw-glow"></div>
    <svg class="trw-lines" viewBox="0 0 ${narrow ? BOARD_W_NARROW : BOARD_W} ${BOARD_H}" aria-hidden="true">${lines.map((d) => `<path d="${d}"/>`).join("")}</svg>
    ${lines.map((d, i) => `<span class="trw-packet" style="offset-path:path('${d}');--i:${i}"></span>`).join("")}
    ${dev("mac", MAC_ICON, "Mac app", 0)}
    ${dev("phone", PHONE_ICON, "iPhone", 1)}
    ${dev("web", icon("globe"), "Browser", 2)}
    <div class="trw-machine">
      <div class="trw-head"><span class="trw-ic">${icon("server")}</span><b>Cloud machine</b><span class="pill s-working"><span class="dot live"></span>Running</span></div>
      <div class="trw-rows">
        ${agent("claude", "Phone app for agent control", "s-working", "Working", 0)}
        ${agent("codex", "Port scanner for the home", "s-working", "Working", 1)}
        ${agent("opencode", "Resume old sessions", "s-needs", "Needs input", 2)}
        ${agent("antigravity", "Landing page hero", "s-working", "Working", 3)}
      </div>
    </div>
  </div>`;
}

const START_TASK = "Add a dark mode toggle to Settings";

function startScene() {
  const card = (title, st, label, model, line, extra = "") => `<div class="trs-card ${st} ${extra}">
    <div class="trs-ct"><span class="dot${st === "s-working" ? " live" : ""}"></span><b>${title}</b></div>
    <div class="trs-cm"><span class="trs-st">${label}</span><span class="trs-tag">${model}</span></div>
    <div class="trs-cl">${line}</div></div>`;
  const main = `<div class="trs-head"><div class="trs-ht"><b>rudder</b><span>exla-ai/rudder @ main</span></div>
      <span class="tr-chip">${harnessLogo("claude")}<span>Opus 5.5</span></span></div>
    <div class="trs-sec">Working</div>
    <div class="trs-list">
      ${card(START_TASK, "s-working", "Working", "Claude · Opus 5.5", "Reading cloud/app/org.js", "trs-new")}
      ${card("Port scanner for the home", "s-working", "Working", "Codex · Sol 6.1", "Reading /proc/net/tcp6 for owner pids.")}
      ${card("Fix flaky Ctrl+W chord test", "s-needs", "Needs input", "Codex · Sol 6.1", "Can I run cargo test with network access?")}
    </div>
    <div class="trs-comp">
      <div class="trs-box"><span class="trs-clip">${icon("clip")}</span>
        <span class="trs-text"><span class="trs-typed"></span><span class="trs-caret"></span><span class="trs-ph">Start a new task</span></span>
        <span class="trs-send">${icon("up")}</span></div>
      <div class="trs-hint">Enter to start</div>
    </div>`;
  return `<div class="tr-art tr-start">${windowScene(main, [
    [START_TASK, "s-working", "trs-side-new"],
    ["Port scanner for the home", "s-working"],
    ["Fix flaky Ctrl+W chord test", "s-needs"],
  ])}</div>`;
}

// Types the task into the composer, sends it, and empties the box; the new
// agent's card (CSS, .trs-new) arrives as it empties.
function playStart(art, later) {
  const box = art.querySelector(".trs-box");
  const typed = art.querySelector(".trs-typed");
  const begin = 520;
  const per = 36;
  later(() => box.classList.add("typing"), begin - 120);
  for (let i = 1; i <= START_TASK.length; i += 1) later(() => setText(typed, START_TASK.slice(0, i)), begin + i * per);
  const sent = begin + START_TASK.length * per + 260;
  later(() => box.classList.add("sent"), sent);
  later(() => { setText(typed, ""); box.classList.remove("typing", "sent"); }, sent + 170);
}

function modelScene() {
  const tabs = [["claude", "Claude"], ["codex", "Codex"], ["opencode", "OpenCode"], ["antigravity", "Antigravity"]]
    .map(([id, label], i) => `<span class="trm-tab t${i}">${harnessLogo(id)}<span>${label}</span></span>`).join("");
  const list = (cls, names) => `<div class="trm-list ${cls}">${names.map((n, i) =>
    `<div class="trm-model${i === 0 ? " on" : ""}"><span>${n}</span><span class="trm-radio">${i === 0 ? icon("check") : ""}</span></div>`).join("")}</div>`;
  const main = `<div class="trs-head"><div class="trs-ht"><b>rudder</b><span>exla-ai/rudder @ main</span></div>
      <span class="tr-chip on">${harnessLogo("claude")}<span>Opus 5.5</span></span></div>
    <div class="trs-sec">Working</div>
    <div class="trs-list"><div class="trs-card s-working"><div class="trs-ct"><span class="dot"></span><b>${START_TASK}</b></div></div>
      <div class="trs-card s-working"><div class="trs-ct"><span class="dot"></span><b>Port scanner for the home</b></div></div></div>`;
  return `<div class="tr-art tr-model">${windowScene(main, [
    [START_TASK, "s-working"], ["Port scanner for the home", "s-working"], ["Fix flaky Ctrl+W chord test", "s-needs"],
  ], { cls: "dim" })}
    <div class="trm-sheet">
      <div class="trm-head"><b>Model for new agents</b>${icon("x")}</div>
      <div class="trm-tabs"><span class="trm-ind"></span>${tabs}</div>
      <div class="trm-accts"><span class="trm-acct">you@example.com <i class="p0">Max</i><i class="p1">Pro</i></span>
        <span class="trm-add">${icon("plus")}<span>Add account</span></span></div>
      <div class="trm-models">${list("l0", ["Opus 5.5", "Sonnet 5.5", "Haiku 5.5"])}${list("l1", ["Sol 6.2", "Astra 6.2", "Luna 6"])}</div>
      <div class="trm-effort"><span>Effort</span><i class="on">Auto</i><i>Low</i><i>Medium</i><i>High</i></div>
    </div>
  </div>`;
}

function subagentsScene() {
  const card = (title, kind, st, label, facts, i) => `<div class="tra-card ${st}" style="--i:${i}">
    <div class="tra-ct"><span class="dot${st === "s-working" ? " live" : ""}"></span><b>${title}</b></div>
    <div class="tra-cm"><span class="tra-st">${label}</span> · ${kind}</div>
    <div class="tra-cm">${facts}</div></div>`;
  const main = `<div class="tra-head"><b>Split up the settings screen</b><span class="s-working"><span class="dot live"></span>Working</span></div>
    <div class="tra-chat">
      <div class="tra-user">Break the settings screen into smaller pieces and check dark mode while you're at it.</div>
      <div class="tra-msg">On it. I'm handing three parts to subagents so they run side by side.</div>
    </div>
    <div class="tra-tray">
      ${card("Map the settings screens", "Explore", "s-working", "Working", "14 tool calls · 1m", 0)}
      ${card("Move colors to tokens", "General", "s-working", "Working", "9 tool calls · 52s", 1)}
      ${card("Check dark mode contrast", "Explore", "s-done", "Done", "6 tool calls · 40s", 2)}
    </div>
    <div class="tra-comp"><span class="tra-btn">${icon("agents")}<span class="tra-badge">3</span></span>
      <span class="tra-box">Message the agent</span></div>`;
  return `<div class="tr-art tr-subagents">${windowScene(main, [
    ["Split up the settings screen", "s-working", "on"], ["Port scanner for the home", "s-working"], ["Fix flaky Ctrl+W chord test", "s-needs"],
  ])}</div>`;
}

function phoneScene() {
  return `<div class="tr-art tr-phone">
    <div class="trp-preview">
      <div class="trp-bar">${icon("lock")}<span>rudder-home.tailnet.ts.net:3000</span></div>
      <div class="trp-page"><i class="w40"></i><i class="w90"></i><i class="w70"></i><div class="trp-blocks"><i></i><i></i></div></div>
      <div class="trp-ts">${TS_DOTS}<span>Private to your tailnet</span></div>
    </div>
    <div class="trp-phone"><div class="trp-screen">
      <div class="trp-status"><b>9:41</b><span class="trp-island"></span><span class="trp-sig"><i></i><i></i><i></i></span></div>
      <div class="trp-banner"><img src="/app/icon.svg" alt=""><div><b>Fix flaky Ctrl+W test needs you</b><span>Can I run cargo test with network access?</span></div></div>
      <div class="trp-head">${icon("back")}<div><b>Fix flaky Ctrl+W test</b>
        <span class="trp-state"><span class="s-needs trp-s0"><span class="dot"></span>Needs input</span><span class="s-working trp-s1"><span class="dot live"></span>Working</span></span></div></div>
      <div class="trp-chat">
        <div class="trp-msg">The chord test fetches a crate before it runs.</div>
        <div class="trp-ask">
          <div class="trp-q">Can I run <code>cargo test</code> with network access?</div>
          <div class="trp-btns"><span class="trp-allow">Allow<i class="trp-tap"></i></span><span class="trp-deny">Deny</span></div>
          <div class="trp-done">${icon("check")}<span>Allowed</span></div>
        </div>
        <div class="trp-tool">${icon("terminal")}<b>Bash</b><span>cargo test</span></div>
        <div class="trp-prev"><span>Previews</span><span class="trp-chip">${icon("globe")}<b>board</b><i>:3000</i></span></div>
      </div>
      <div class="trp-comp">Message the agent</div>
    </div></div>
  </div>`;
}

function keysScene() {
  const main = `<div class="trk-head">${["Phone app for agent control", "Port scanner for the home", "Fix flaky Ctrl+W chord test"]
    .map((t, i) => `<b class="trk-t k${i}">${t}</b>`).join("")}</div>
    <div class="trk-chat"><i class="w80"></i><i class="w60"></i><i class="w90"></i><i class="w40"></i></div>
    <div class="trk-comp">Message the agent</div>`;
  const opt = ALT === "Option" ? `<span class="trk-sym">⌥</span><span class="trk-word">option</span>` : `<span class="trk-word">Alt</span>`;
  return `<div class="tr-art tr-keys">${windowScene(main, [
    ["Phone app for agent control", "s-working"], ["Port scanner for the home", "s-working"], ["Fix flaky Ctrl+W chord test", "s-needs"],
  ], { cls: "keys" })}
    <div class="trk-keys"><span class="trk-key trk-opt">${opt}</span><span class="trk-key trk-l">[</span><span class="trk-key trk-r">]</span></div>
  </div>`;
}

/* Steps -------------------------------------------------------------- */

function linkButton(svg, label, href, onClick) {
  const a = h(`<a class="btn tr-dl" target="_blank" rel="noopener noreferrer">${svg}<span></span></a>`);
  setText(a.querySelector("span"), label);
  a.href = href;
  // app.js sends a new-tab link to the default browser in the Mac and iPhone apps.
  a.addEventListener("click", onClick);
  return a;
}

const STEPS = [
  {
    id: "welcome",
    title: "Your agents keep working",
    body: "Rudder runs Claude Code, Codex, OpenCode and Antigravity on a cloud machine that is yours. Close the laptop and they carry on.",
    scene: welcomeScene,
  },
  {
    id: "start",
    title: "Start an agent",
    body: "Open a project, say what you want done and press Enter. An agent starts on it right away. Run as many as you like.",
    scene: startScene,
    play: playStart,
  },
  {
    id: "model",
    title: "Pick the harness and model",
    body: "The model chip switches harness, model and effort. Add account signs in a second subscription, so a busy plan never stops you.",
    scene: modelScene,
  },
  {
    id: "subagents",
    title: "Watch its subagents",
    body: "When an agent hands work to subagents, the button next to the message bar counts them. Open it to see each one work.",
    scene: subagentsScene,
  },
  {
    id: "anywhere",
    title: "Steer from anywhere",
    body: "Your phone, your Mac and any browser show the same agents, so you can answer one from anywhere. Connect Tailscale and your previews open privately on all your devices.",
    scene: phoneScene,
    actions: () => {
      const out = [];
      if (!PLATFORM) {
        out.push(linkButton(MAC_ICON, "Get the Mac app", DOWNLOAD_MAC_URL, () => track("download_mac_clicked", { from: "tour" })));
      }
      if (PLATFORM !== "ios") {
        out.push(linkButton(PHONE_ICON, "Get the iPhone app", PHONE_APP_URL, () => track("phone_link_opened", { kind: "app", from: "tour" })));
      }
      return out;
    },
  },
  {
    id: "keys",
    title: "Jump between agents",
    body: `${ALT}+[ and ${ALT}+] go to the previous and next agent. They work while you type, too.`,
    scene: keysScene,
    // A keyboard shortcut: only where there is a keyboard to press it.
    when: () => !coarse,
  },
];

/* The tour ----------------------------------------------------------- */

let current = null;

/**
 * Opens the tour over the app. `again`: from Settings, so its close button
 * says Close and it ends on Done.
 */
export function openTour({ again = false } = {}) {
  if (current) return;
  const steps = STEPS.filter((s) => !s.when || s.when());
  const total = steps.length;
  const opener = document.activeElement;
  const dlg = h(`<dialog class="tour" aria-labelledby="tr-title" aria-describedby="tr-body">
    <div class="tr-shell">
      <div class="tr-top"><button type="button" class="tr-skip"></button></div>
      <div class="tr-stage" aria-hidden="true"><div class="tr-fit"></div></div>
      <div class="tr-side">
        <div class="tr-copy">
          <p class="tr-count"></p>
          <h2 class="tr-title" id="tr-title"></h2>
          <p class="tr-body" id="tr-body"></p>
          <div class="tr-acts"></div>
        </div>
        <div class="tr-nav">
          <div class="tr-prog"></div>
          <div class="tr-btns">
            <button type="button" class="btn tr-back">${icon("back")}<span>Back</span></button>
            <button type="button" class="btn accent tr-next"><span class="tr-next-l"></span>${coarse ? "" : `<kbd aria-hidden="true">↵</kbd>`}</button>
          </div>
        </div>
      </div>
    </div>
  </dialog>`);
  if (PLATFORM === "mac") dlg.classList.add("mac");
  const shell = dlg.querySelector(".tr-shell");
  const stage = dlg.querySelector(".tr-stage");
  const fit = dlg.querySelector(".tr-fit");
  const copy = dlg.querySelector(".tr-copy");
  const count = dlg.querySelector(".tr-count");
  const titleEl = dlg.querySelector(".tr-title");
  const bodyEl = dlg.querySelector(".tr-body");
  const acts = dlg.querySelector(".tr-acts");
  const prog = dlg.querySelector(".tr-prog");
  const back = dlg.querySelector(".tr-back");
  const next = dlg.querySelector(".tr-next");
  const skip = dlg.querySelector(".tr-skip");
  setText(skip, again ? "Close" : "Skip");
  // The step dots: a pointer can jump with them; keys use the arrows.
  const dots = steps.map((s, i) => {
    const d = h(`<button type="button" class="tr-dot" tabindex="-1"></button>`);
    d.setAttribute("aria-label", `Step ${i + 1}: ${s.title}`);
    d.addEventListener("click", () => go(i));
    prog.appendChild(d);
    return d;
  });

  let at = -1;
  let timers = [];
  const later = (fn, ms) => { timers.push(setTimeout(fn, ms)); };

  // Scale the board to the stage, never past 1.3x. Crossing into or out of
  // a narrow stage draws the scene again on the other board.
  let narrow = false;
  const fitBoard = () => {
    const wide = stage.clientWidth >= NARROW_STAGE;
    if (at >= 0 && wide === narrow) { narrow = !wide; showScene(steps[at], 0); }
    narrow = !wide;
    const w = narrow ? BOARD_W_NARROW : BOARD_W;
    const s = Math.min(stage.clientWidth / w, stage.clientHeight / BOARD_H, 1.3);
    if (!(s > 0)) return;
    fit.style.setProperty("--bw", `${w}px`);
    fit.style.setProperty("--s", s.toFixed(4));
  };
  const ro = new ResizeObserver(fitBoard);
  ro.observe(stage);

  // The step's scene in place of the last one, sliding in from `dir` (0: no motion).
  function showScene(step, dir) {
    timers.forEach(clearTimeout);
    timers = [];
    const motion = dir !== 0 && !reducedMotion.matches;
    for (const old of fit.querySelectorAll(".tr-art")) {
      if (!motion) { old.remove(); continue; }
      old.classList.add("leaving");
      old.style.setProperty("--dir", String(dir));
      setTimeout(() => old.remove(), 320);
    }
    const art = h(step.scene({ narrow }));
    art.classList.toggle("narrow", narrow);
    if (motion) {
      art.classList.add("entering");
      art.style.setProperty("--dir", String(dir));
    }
    fit.appendChild(art);
    if (!reducedMotion.matches) step.play?.(art, later);
  }

  function go(i) {
    if (i < 0 || i >= total || i === at) return;
    const dir = i > at ? 1 : -1;
    const first = at < 0;
    at = i;
    const step = steps[i];

    setText(count, `${i + 1} of ${total}`);
    setText(titleEl, step.title);
    setText(bodyEl, step.body);
    acts.replaceChildren(...(step.actions?.() || []));
    acts.hidden = !acts.children.length;
    dots.forEach((d, n) => { d.classList.toggle("done", n < i); d.classList.toggle("on", n === i); });
    const last = i === total - 1;
    setText(next.querySelector(".tr-next-l"), last ? (again ? "Done" : "Start using Rudder") : "Next");
    if (back.contains(document.activeElement) && i === 0) next.focus();
    back.classList.toggle("off", i === 0);
    back.tabIndex = i === 0 ? -1 : 0;
    shell.style.setProperty("--pan", String(i));

    showScene(step, first ? 0 : dir);
    if (!first && !reducedMotion.matches) {
      copy.classList.remove("swap");
      void copy.offsetWidth;
      copy.classList.add("swap");
    }
    track("tutorial_step", { step: i + 1, steps: total, id: step.id, again });
  }

  let closing = false;
  function finish(done) {
    if (closing) return;
    closing = true;
    ls.set(SEEN_KEY, true);
    if (done) track("tutorial_completed", { steps: total, again });
    else track("tutorial_skipped", { step: at + 1, steps: total, id: steps[at].id, again });
    if (reducedMotion.matches) { dlg.close(); return; }
    dlg.classList.add("closing");
    setTimeout(() => dlg.close(), 260);
  }

  next.addEventListener("click", () => (at === total - 1 ? finish(true) : go(at + 1)));
  back.addEventListener("click", () => go(at - 1));
  skip.addEventListener("click", () => finish(false));
  dlg.addEventListener("keydown", (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey || e.shiftKey) return;
    if (e.key === "ArrowRight") { e.preventDefault(); if (at < total - 1) go(at + 1); }
    else if (e.key === "ArrowLeft") { e.preventDefault(); go(at - 1); }
    // Enter on a button or link is that button's; anywhere else it is Next.
    else if (e.key === "Enter" && !e.target.closest("button, a")) { e.preventDefault(); next.click(); }
  });
  // Escape: the same as Skip.
  dlg.addEventListener("cancel", (e) => { e.preventDefault(); finish(false); });
  // A swipe across the stage on a touch screen.
  let touch = null;
  stage.addEventListener("pointerdown", (e) => { if (e.pointerType === "touch") touch = { x: e.clientX, y: e.clientY }; });
  stage.addEventListener("pointerup", (e) => {
    if (!touch) return;
    const dx = e.clientX - touch.x;
    const dy = e.clientY - touch.y;
    touch = null;
    if (Math.abs(dx) < 48 || Math.abs(dx) < Math.abs(dy) * 1.3) return;
    if (dx < 0 && at < total - 1) go(at + 1);
    else if (dx > 0) go(at - 1);
  });
  stage.addEventListener("pointercancel", () => { touch = null; });

  dlg.addEventListener("close", () => {
    timers.forEach(clearTimeout);
    ro.disconnect();
    dlg.remove();
    current = null;
    if (opener?.isConnected) opener.focus?.();
  });

  current = dlg;
  document.body.appendChild(dlg);
  dlg.showModal();
  fitBoard();
  go(0);
  // Enter is Next from the start; a touch screen has no keys to press.
  if (!coarse) next.focus();
  track("tutorial_viewed", { steps: total, again });
}
