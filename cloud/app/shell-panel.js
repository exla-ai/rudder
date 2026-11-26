// The Terminal: a shell on your cloud machine, typed into from the app. It is
// one login shell that stays up on the machine (worker/machine-shell.mjs), so
// a reload or another device picks it back up where it was. It opens from
// Cmd+K and takes the right-hand slot it shares with Files, Canvas, Notes and
// an agent's terminal (side-panel.js), and stays there from page to page.
// Typing `exit` ends the shell and closes the panel.
//
// The socket is opened with a ticket from the signed-in API (src/shell-relay.ts).
import { api, coarse, track } from "/app/core.js";
import { h, icon, setText } from "/app/render.js";
import { openSide, closeSide, placeSide, sideOccupant, dragColumn } from "/app/side-panel.js";
import { newTerm, termTheme } from "/app/xterm-kit.js";

const RETRY_MS = 1500;

const el = h(`<section class="tp" aria-label="Terminal">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="tp-head">
    <span class="tp-title">${icon("terminal")}<span>Terminal</span></span>
    <span class="tp-agent">Your cloud machine</span>
    <button type="button" class="icon-btn tp-close" aria-label="Close terminal" title="Close terminal">${icon("x")}</button>
  </header>
  <div class="tp-body"><div class="tp-term tp-fill" role="region" aria-label="Shell"></div><div class="tp-msg" hidden></div></div>
</section>`);
const bodyEl = el.querySelector(".tp-body"), hostEl = el.querySelector(".tp-term"), msgEl = el.querySelector(".tp-msg");

let term = null, fit = null, making = null;
let ws = null;
let serial = 0; // bumps on every connect and close, so a late answer is dropped
let timer = 0;

const panel = { el, close: async () => { disconnect(); return true; } };
const isOpen = () => sideOccupant() === panel;

el.querySelector(".tp-close").addEventListener("click", () => closeShell());
el.querySelector(".side-grip").addEventListener("pointerdown", dragColumn);
new ResizeObserver(() => { if (term && el.isConnected) fit.fit(); }).observe(bodyEl);
// Settings > Appearance (theme.js) flips html[data-theme]; the terminal follows.
new MutationObserver(() => { if (term && el.isConnected) term.options.theme = termTheme(el); })
  .observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

function showMessage(text) {
  setText(msgEl, text);
  msgEl.hidden = !text;
}

const send = (frame) => { if (ws?.readyState === 1) ws.send(JSON.stringify(frame)); };

async function ensureTerm() {
  if (term) return;
  making ??= newTerm(hostEl, el, { stdin: true, onLost }).finally(() => { making = null; });
  ({ term, fit } = await making);
  term.onData((data) => send({ type: "input", data }));
  term.onResize(({ cols, rows }) => send({ type: "resize", cols, rows }));
}

function onLost(error) {
  term = null; fit = null;
  disconnect();
  showMessage(error.message);
}

function disconnect() {
  serial++;
  clearTimeout(timer);
  ws?.close();
  ws = null;
}

async function connect(first) {
  disconnect();
  const mine = serial;
  showMessage(first ? "Starting the terminal" : "Reconnecting");
  const again = () => { timer = setTimeout(() => connect(false), RETRY_MS); };
  let ticket;
  try { ({ ticket } = await api("/api/app/shell-ticket", { method: "POST" })); }
  catch (err) {
    if (mine !== serial) return;
    if (err.status === 401) showMessage("Signed out.");
    else again();
    return;
  }
  if (mine !== serial) return;
  const url = new URL("/api/app/shell", location.origin);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("ticket", ticket);
  const socket = new WebSocket(url.href);
  ws = socket;
  let refused = "";
  socket.onopen = () => {
    if (mine !== serial) return;
    fit.fit();
    send({ type: "open", cols: term.cols, rows: term.rows });
  };
  socket.onmessage = (event) => {
    if (mine !== serial) return;
    const frame = JSON.parse(event.data);
    if (frame.type === "ready") {
      term.reset();
      term.write(frame.replay);
      showMessage("");
      if (!coarse) term.focus();
    } else if (frame.type === "output") term.write(frame.data);
    else if (frame.type === "exit") closeShell();
    else if (frame.type === "error") refused = frame.message;
  };
  socket.onclose = () => {
    if (mine !== serial) return;
    ws = null;
    // The machine said why (it has none, or runs a version without this): that stays up.
    if (refused) showMessage(refused);
    else again();
  };
}

/** Whether the Terminal is in the slot. */
export function shellOpen() {
  return isOpen();
}

/** Open or close the machine's Terminal. */
export async function setShell(on) {
  if (!on) { closeShell(); return; }
  if (isOpen()) { if (term && !coarse) term.focus(); return; }
  if (!(await openSide(panel))) return;
  track("feature_used", { feature: "machine_terminal" });
  showMessage("Starting the terminal");
  try { await ensureTerm(); }
  catch (error) { if (isOpen()) showMessage(error.message); return; }
  if (!isOpen()) return;
  term.options.theme = termTheme(el);
  void connect(true);
}

function closeShell() {
  if (!isOpen()) return;
  disconnect();
  closeSide(panel);
}

/** The route changed: the desktop shell may have been rebuilt around the slot. */
export function followShell() {
  if (isOpen()) placeSide();
}
