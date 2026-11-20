// The terminal panel: an agent's live terminal beside its conversation, drawn
// by xterm.js (/app/vendor/xterm.js, built from cloud/editor/xterm.js). It
// takes the right-hand slot it shares with Files, Canvas and Notes
// (side-panel.js): a desktop column that slides open, or narrower a
// full-screen sheet. It opens and closes from Cmd+K, the agent's More menu,
// its close button and Ctrl+`. Esc belongs to the terminal and never closes it.
//
// The screen is the agent's canonical terminal (terminal-client.js). Every
// frame is a complete ANSI repaint of the agent's PTY, so the panel shows its
// geometry, colors and cursor exactly, read only. Saved and legacy agents
// keep the HTTP text screen and the key strip.
import { api, coarse, openExternal, postNative, toast, track, PLATFORM } from "/app/core.js";
import { h, icon, copyText, setText } from "/app/render.js";
import { CanonicalTerminalView } from "/app/terminal-client.js";
import { openSide, closeSide, placeSide, dragColumn } from "/app/side-panel.js";

const SLIDE_MS = 200; // .dsk's grid-template-columns transition (desktop.css)
const SHEET_OUT_MS = 160;
const POLL_MS = 1000;

// Type and behavior, modeled on Ghostty's defaults. The web loads JetBrains
// Mono (free, OFL; its @font-face rides in /app/vendor/xterm.css); the Mac
// app draws with the system's SF Mono, then Menlo.
const FONT_FAMILY = PLATFORM === "mac"
  ? `ui-monospace, "SF Mono", Menlo, monospace`
  : `"JetBrains Mono Variable", ui-monospace, "SF Mono", Menlo, monospace`;
const FONT_SIZE = 13;
const MIN_FONT_SIZE = 9; // an agent's wide screen shrinks to the panel down to this
const LINE_HEIGHT = 1.15;
const COPY_ON_SELECT = true;

const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");

const state = {
  open: false,
  target: null,     // { ws, run, findAgent }
  view: null,       // the target's CanonicalTerminalView
  canonical: false, // its live screen arrived; until then, and for saved runs, the HTTP screen
  epoch: 0,         // bumps on every target and mode change, so a late poll is dropped
  timer: 0,         // the HTTP screen's next poll
  history: [],      // the agent's scrollback lines already in xterm
  closing: 0,       // the slide-out's timer
};

const el = h(`<section class="tp" aria-label="Terminal">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="tp-head">
    <span class="tp-title">${icon("terminal")}<span>Terminal</span></span>
    <span class="tp-agent"></span>
    <span class="tp-ro" hidden title="This is the agent's own terminal. Message the agent to steer it.">Read only</span>
    <button type="button" class="icon-btn tp-close" aria-label="Close terminal" title="Close terminal (Ctrl+\`)">${icon("x")}</button>
  </header>
  <div class="tp-body"><div class="tp-term" role="region" aria-label="Agent terminal"></div><div class="tp-msg" hidden></div></div>
  <div class="key-strip" role="toolbar" aria-label="Keys" hidden>
    <button type="button" data-key="esc">Esc</button><button type="button" data-key="up" aria-label="Up">↑</button>
    <button type="button" data-key="down" aria-label="Down">↓</button><button type="button" data-key="left" aria-label="Left">←</button>
    <button type="button" data-key="right" aria-label="Right">→</button><button type="button" data-key="enter">Enter</button>
    <button type="button" data-key="tab">Tab</button><button type="button" data-key="shift-tab" aria-label="Shift Tab">⇧Tab</button>
    <button type="button" data-key="y">y</button><button type="button" data-key="n">n</button>
    <button type="button" data-key="ctrl-c" class="danger">^C</button>
  </div>
</section>`);
const $ = (s) => el.querySelector(s);
const bodyEl = $(".tp-body"), hostEl = $(".tp-term"), msgEl = $(".tp-msg"), agentEl = $(".tp-agent"), roEl = $(".tp-ro"), keysEl = $(".key-strip");

$(".tp-close").addEventListener("click", () => closePanel());
$(".side-grip").addEventListener("pointerdown", dragColumn);
keysEl.addEventListener("click", async (e) => {
  const b = e.target.closest("[data-key]");
  const target = state.target;
  if (!b || !target) return;
  postNative({ type: "haptic" });
  try {
    await api("/api/app/keys", { method: "POST", body: { workspace: target.ws, run: target.run, keys: b.dataset.key } });
    if (state.target === target && !state.canonical) { clearTimeout(state.timer); state.timer = setTimeout(pollScreen, 250); }
  } catch (err) { if (err.status !== 401) toast(err.message, { error: true }); }
});
document.addEventListener("visibilitychange", () => {
  if (!state.open || !state.view) return;
  if (document.hidden) { state.view.stop(); clearTimeout(state.timer); }
  else startStream();
});

// ---------- xterm.js ----------

let lib = null;
let term = null, fit = null, failed = null;
const loadCss = () => new Promise((resolve, reject) => {
  const link = h(`<link rel="stylesheet" href="/app/vendor/xterm.css">`);
  link.addEventListener("load", resolve, { once: true });
  link.addEventListener("error", () => reject(new Error("Couldn't load the terminal's stylesheet")), { once: true });
  document.head.appendChild(link);
});
const loadLib = () => (lib ??= Promise.all([import("/app/vendor/xterm.js"), loadCss(), document.fonts.load(`${FONT_SIZE}px ${FONT_FAMILY}`)])
  .then(([mod]) => mod).catch((error) => { lib = null; throw error; }));

/** The terminal's colors, from the app's palette (terminal-panel.css). */
function readTheme() {
  const css = getComputedStyle(el);
  const color = (name) => {
    const value = css.getPropertyValue(`--term-${name}`).trim();
    if (!value) throw new Error(`terminal-panel.css has no --term-${name}`);
    return value;
  };
  const theme = {
    background: color("bg"), foreground: color("fg"), cursor: color("cursor"), cursorAccent: color("bg"),
    selectionBackground: color("selection"), selectionInactiveBackground: color("selection-inactive"),
    scrollbarSliderBackground: color("scrollbar"), scrollbarSliderHoverBackground: color("scrollbar-hover"),
    scrollbarSliderActiveBackground: color("scrollbar-hover"),
  };
  for (const name of ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]) {
    theme[name] = color(name);
    theme[`bright${name[0].toUpperCase()}${name.slice(1)}`] = color(`bright-${name}`);
  }
  return theme;
}
// Settings > Appearance (theme.js) flips html[data-theme]; the terminal follows.
new MutationObserver(() => { if (term && el.isConnected) term.options.theme = readTheme(); })
  .observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

/** One terminal for the page, made on first open. WebGL draws it, or nothing
 * does: a browser without WebGL 2 gets a message, not a slower renderer. */
async function ensureTerm() {
  if (term) return term;
  if (failed) throw failed;
  const { Terminal, FitAddon, WebLinksAddon, Unicode11Addon, WebglAddon } = await loadLib();
  if (term) return term;
  const t = new Terminal({
    fontFamily: FONT_FAMILY, fontSize: FONT_SIZE, lineHeight: LINE_HEIGHT, letterSpacing: 0,
    cursorStyle: "block", cursorBlink: true, cursorInactiveStyle: "outline",
    scrollback: 10000, disableStdin: true, allowProposedApi: true,
    macOptionIsMeta: true, macOptionClickForcesSelection: true,
    drawBoldTextInBrightColors: false, minimumContrastRatio: 1,
    theme: readTheme(),
  });
  fit = new FitAddon();
  t.loadAddon(fit);
  t.loadAddon(new Unicode11Addon());
  t.unicode.activeVersion = "11";
  t.loadAddon(new WebLinksAddon((e, uri) => { e.preventDefault(); openExternal(uri); }));
  t.open(hostEl);
  try { loadWebgl(t, WebglAddon); }
  catch (error) { t.dispose(); fit = null; failed = error; throw error; }
  // Copy on select, as Ghostty and iTerm2 do by default: once a drag or a
  // double-click ends, not on every step of it.
  if (COPY_ON_SELECT) {
    hostEl.addEventListener("mouseup", () => setTimeout(() => {
      const text = t.hasSelection() ? t.getSelection() : "";
      if (text) void copyText(text).then((ok) => { if (!ok) toast("Couldn't copy the selection", { error: true }); });
    }, 0));
  }
  term = t;
  return t;
}

function loadWebgl(t, WebglAddon) {
  let webgl;
  try {
    webgl = new WebglAddon();
    t.loadAddon(webgl);
  } catch {
    throw new Error("This browser can't draw the terminal. It needs WebGL 2.");
  }
  // The GPU dropped the context (a sleep, a driver reset): draw with a new one.
  // Without one, the terminal goes rather than fall back to xterm's DOM renderer.
  webgl.onContextLoss(() => {
    webgl.dispose();
    try { loadWebgl(t, WebglAddon); }
    catch (error) {
      failed = error;
      state.view?.stop();
      state.view = null;
      t.dispose();
      term = null; fit = null;
      showMessage(error.message);
    }
  });
}

/** An agent's screen keeps its own columns, so a wide one shrinks its type to
 * fit the panel (down to MIN_FONT_SIZE, then it scrolls sideways). */
const measure = document.createElement("canvas").getContext("2d");
function fitFont() {
  if (!term || !state.canonical) return;
  const pad = parseFloat(getComputedStyle(bodyEl).paddingLeft) * 2;
  measure.font = `${FONT_SIZE}px ${FONT_FAMILY}`;
  const cell = measure.measureText("W").width;
  const room = (bodyEl.clientWidth - pad) / (term.cols * cell);
  const size = Math.max(MIN_FONT_SIZE, Math.min(FONT_SIZE, Math.floor(FONT_SIZE * room * 0.98 * 2) / 2));
  if (term.options.fontSize !== size) term.options.fontSize = size;
}
new ResizeObserver(() => {
  if (!term || !el.isConnected) return;
  if (state.canonical) fitFont();
  else fit.fit();
}).observe(bodyEl);

// ---------- Screens ----------

/** Lines of `next` that are not yet in xterm's scrollback: `next` is the old
 * history with lines dropped off its head and new ones on its tail. Null when
 * it is some other history (the agent cleared its screen): start over. */
function historyDelta(old, next) {
  if (!next.length) return []; // The alternate screen sends no history; keep what is shown.
  if (!old.length) return next;
  const last = old[old.length - 1];
  for (let i = next.length - 1, tries = 0; i >= 0 && tries < 4; i--) {
    if (next[i] !== last) continue;
    tries++;
    const keep = i + 1;
    if (keep > old.length) continue;
    let same = true;
    for (let j = 0; j < keep && same; j++) same = next[j] === old[old.length - keep + j];
    if (same) return next.slice(keep);
  }
  return null;
}

// Lines written at the top and scrolled off it, so they land in xterm's own
// scrollback above the screen the frame then repaints.
const intoScrollback = (lines, rows) => (lines.length
  ? `\x1b[m\x1b[H\x1b[J${lines.join("\r\n")}\x1b[${rows};1H${"\n".repeat(Math.min(lines.length, rows))}`
  : "");

function drawScreen(screen, done) {
  if (!term) return;
  if (term.cols !== screen.columns || term.rows !== screen.rows) term.resize(screen.columns, screen.rows);
  fitFont();
  let added = historyDelta(state.history, screen.scrollback);
  if (added === null) { term.reset(); added = screen.scrollback; }
  if (screen.scrollback.length) state.history = screen.scrollback;
  term.write(intoScrollback(added, screen.rows) + screen.screen, done);
}

/** Saved runs and legacy owners: the agent's screen as text, fitted to the panel. */
async function pollScreen() {
  state.timer = 0;
  const target = state.target, epoch = state.epoch;
  if (!state.open || state.canonical || !target || !term) return;
  if (!document.hidden) {
    try {
      const res = await api(`/api/app/screen?workspace=${encodeURIComponent(target.ws)}&run=${encodeURIComponent(target.run)}`);
      if (epoch !== state.epoch) return;
      if (res?.text != null) {
        const lines = res.text.split("\n");
        while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
        showMessage("");
        term.write(`\x1b[m\x1b[H\x1b[2J\x1b[3J${lines.join("\r\n")}`);
      } else showMessage("Waiting for the agent's screen");
    } catch (err) {
      if (epoch !== state.epoch) return;
      showMessage(err.status === 401 ? "Signed out." : err.waking ? "Waking the cloud machine" : err.message);
    }
  }
  if (epoch === state.epoch && state.open && !state.canonical) state.timer = setTimeout(pollScreen, POLL_MS);
}

function setMode(canonical) {
  state.canonical = canonical;
  state.epoch++;
  clearTimeout(state.timer);
  state.history = [];
  term.reset();
  hostEl.classList.toggle("tp-fill", !canonical);
  roEl.hidden = !canonical;
  keysEl.hidden = canonical;
  if (canonical) showMessage("");
  else { fit.fit(); void pollScreen(); }
}

function startStream() {
  if (!state.view || document.hidden) return;
  state.view.stop();
  state.view.start();
}

function showMessage(text) {
  setText(msgEl, text);
  msgEl.hidden = !text;
}

// ---------- The right-hand slot (side-panel.js) ----------

// Another panel taking the slot closes the terminal at once, without a slide.
const panel = { el, close: async () => { endSlide(); stopPanel(); return true; } };

const dskOf = () => (el.parentElement?.classList.contains("dsk-files") ? el.parentElement.closest(".dsk") : null);

/** Into the slot. A desktop column that was shut slides open, the chat giving way. */
async function place() {
  const shut = !document.querySelector(".dsk-files:not([hidden])");
  endSlide();
  if (!(await openSide(panel))) return false;
  const dsk = dskOf();
  if (dsk && shut && !reducedMotion.matches) {
    dsk.classList.add("tp-shut");
    void getComputedStyle(dsk).gridTemplateColumns;
    dsk.classList.remove("tp-shut");
  }
  return true;
}

function endSlide() {
  clearTimeout(state.closing);
  el.classList.remove("tp-out");
  document.querySelector(".dsk.tp-shut")?.classList.remove("tp-shut");
}

// ---------- Public surface ----------

const sameTarget = (a, b) => !!a && !!b && a.ws === b.ws && a.run === b.run;

/** Whether the panel shows this agent's terminal. */
export function terminalOpen(ws, run) {
  return state.open && sameTarget(state.target, { ws, run });
}

/** Open (on) or close the terminal of agent `target` ({ ws, run, findAgent }). */
export async function setTerminal(target, on) {
  if (!on) { if (terminalOpen(target.ws, target.run)) closePanel(); return; }
  const wasOpen = state.open;
  if (!(await place())) return;
  state.open = true;
  if (!wasOpen) track("feature_used", { feature: "terminal" });
  if (term) term.options.theme = readTheme();
  if (!sameTarget(state.target, target) || !state.view) await retarget(target);
  else if (!wasOpen) startStream();
  if (term && state.open && !coarse) term.focus();
}

/** The route changed, or the other pane took focus: an open terminal follows
 * that agent, and closes on any other page. */
export function followTerminal(route, findAgent) {
  if (!state.open) return;
  placeSide(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  if (!["agent", "all-agent"].includes(route?.name)) { closePanel(); return; }
  const target = { ws: route.ws, run: route.run, findAgent };
  if (!sameTarget(state.target, target)) void retarget(target);
}

async function retarget(target) {
  state.view?.stop();
  state.view = null;
  state.target = target;
  state.epoch++;
  clearTimeout(state.timer);
  setText(agentEl, target.findAgent(target.ws, target.run)?.title || "");
  roEl.hidden = true;
  keysEl.hidden = true;
  hostEl.classList.remove("tp-fill");
  showMessage("Loading the terminal");
  try { await ensureTerm(); }
  catch (error) { if (state.target === target) showMessage(error.message); return; }
  if (state.target !== target || !state.open) return;
  showMessage("");
  state.history = [];
  term.reset();
  state.canonical = true;
  state.view = new CanonicalTerminalView(drawScreen, { workspaceId: target.ws, runId: target.run,
    onCapability: () => { if (state.target === target) setMode(true); },
    onUnavailable: () => { if (state.target === target && state.open) setMode(false); },
  });
  startStream();
}

function stopPanel() {
  state.open = false;
  state.epoch++;
  clearTimeout(state.timer);
  state.view?.stop();
}

/** Close with a short slide: the column shuts, or the sheet slides away. */
function closePanel() {
  if (!state.open) return;
  stopPanel();
  const finish = () => { endSlide(); closeSide(panel); };
  if (reducedMotion.matches || !el.isConnected) { finish(); return; }
  const dsk = dskOf();
  if (dsk) dsk.classList.add("tp-shut");
  else el.classList.add("tp-out");
  state.closing = setTimeout(finish, dsk ? SLIDE_MS : SHEET_OUT_MS);
}
