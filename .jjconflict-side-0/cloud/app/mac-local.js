// The Mac app's "Open a copy on this Mac" (mac/README.md). The shell runs the
// local `rudder cloud pull` in the person's clone; it is a COPY, the cloud
// agent keeps running. Answers come back on window callbacks, matched by id.
import { h, icon, setText, copyText } from "/app/render.js";
import { NATIVE, PLATFORM, postNative, openSheet, toast } from "/app/core.js";

const INSTALL_LINE = "npm i -g @exla/rudder && rudder login";
export const MAC_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/></svg>';
const PULL_TIMEOUT = 4 * 60_000; // the shell allows about 3 minutes

/** This page runs in the Mac app and it can copy agents here. */
export const canOpenOnMac = () => PLATFORM === "mac" && !!NATIVE?.openLocal;

let seq = 0;
const nextId = () => `m${Date.now().toString(36)}${(seq++).toString(36)}`;
const waiting = { status: new Map(), local: new Map(), opened: new Map() };
let lastStatus = null; // the shell also answers once per page load, unasked

function settle(map, key, value) {
  const fn = map.get(key);
  if (fn) { map.delete(key); fn(value); }
}
if (PLATFORM === "mac") {
  window.rudderLocalStatus = (s) => {
    lastStatus = s || null;
    if (s?.id && waiting.status.has(s.id)) settle(waiting.status, s.id, s);
    else for (const id of [...waiting.status.keys()]) settle(waiting.status, id, s);
  };
  window.rudderLocal = (r) => settle(waiting.local, r?.id, r || {});
  window.rudderOpenedPath = (r) => settle(waiting.opened, `${r?.path}|${r?.app}`, r || {});
}

function ask(map, key, msg, ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => settle(map, key, null), ms);
    map.set(key, (v) => { clearTimeout(timer); resolve(v); });
    postNative(msg);
  });
}
function localStatus() {
  const id = nextId();
  return ask(waiting.status, id, { type: "localStatus", id }, 15_000).then((s) => s || lastStatus);
}
function openPath(path, app) {
  return ask(waiting.opened, `${path}|${app}`, { type: "openPath", path, app }, 15_000);
}

const OPENERS = [
  { app: "dashboard", label: "Open Rudder in Terminal", ic: "terminal", primary: true },
  { app: "finder", label: "Finder", ic: "folder" },
  { app: "terminal", label: "Terminal", ic: "terminal" },
  { app: "xcode", label: "Xcode", ic: "code" },
];

/** The sheet: check the CLI, pull the copy, then offer where to open it. */
export function openCopyOnMac({ runId, repo, title }) {
  if (!canOpenOnMac()) return;
  const { dlg, body } = openSheet({ title: "Open a copy on this Mac", className: "mac-copy-sheet" });
  const lead = h(`<p class="mc-lead"></p>`);
  setText(lead, `Copies ${title ? `"${title}"` : "this agent"} and its work into your clone of ${repo || "the repo"}. The cloud agent keeps running.`);
  const slot = h(`<div class="mc-slot"></div>`);
  body.append(lead, slot);
  let busy = false;

  const wait = () => slot.replaceChildren(h(`<div class="ls-wait mc-wait"><span class="spin"></span><div class="ls-title"></div><div class="ls-sub"></div></div>`));
  function waitText(t, sub = "") { setText(slot.querySelector(".ls-title"), t); setText(slot.querySelector(".ls-sub"), sub); }

  function showInstall(status) {
    slot.replaceChildren(h(`<div class="mc-install">
      <p>Copying needs the Rudder CLI on this Mac. Install it, then try again.</p>
      <button type="button" class="c-cmd ob-cmd" aria-label="Copy the install command"><code></code>${icon("copy")}</button>
      <button type="button" class="btn block mc-retry">${icon("refresh")}<span>Check again</span></button></div>`));
    setText(slot.querySelector("code"), INSTALL_LINE);
    slot.querySelector(".c-cmd").addEventListener("click", async () => { if (await copyText(INSTALL_LINE)) toast("Copied"); });
    slot.querySelector(".mc-retry").addEventListener("click", start);
    if (status && status.rudder && !status.claude && !status.codex) {
      slot.prepend(h(`<p class="hint">Claude Code or Codex is needed too.</p>`));
    }
  }

  function showStart() {
    slot.replaceChildren(h(`<button type="button" class="btn accent block mc-go">${MAC_ICON}<span>Copy to this Mac</span></button>`));
    slot.querySelector(".mc-go").addEventListener("click", pull);
  }

  function showResult(r) {
    slot.replaceChildren(h(`<div class="mc-done">
      <div class="mc-ok">${icon("check")}<span>Copied. The cloud agent is still running.</span></div>
      <div class="mc-path"><code></code><button type="button" class="icon-btn small mc-cp" aria-label="Copy path">${icon("copy")}</button></div>
      <div class="mc-open"></div></div>`));
    setText(slot.querySelector(".mc-path code"), r.path);
    slot.querySelector(".mc-cp").addEventListener("click", async () => { if (await copyText(r.path)) toast("Path copied"); });
    const row = slot.querySelector(".mc-open");
    for (const o of OPENERS) {
      const b = h(`<button type="button" class="btn ${o.primary ? "accent block" : "small"}">${icon(o.ic)}<span></span></button>`);
      setText(b.querySelector("span"), o.label);
      b.addEventListener("click", async () => {
        b.disabled = true;
        const res = await openPath(r.path, o.app);
        b.disabled = false;
        if (res && res.ok === false) toast(`Couldn't open it in ${o.label.replace(/^Open Rudder in /, "")}`, { error: true });
        else if (o.app === "dashboard") dlg.close();
      });
      row.appendChild(b);
    }
  }

  function showError(text, retry = true) {
    slot.replaceChildren(h(`<div class="mc-err"><p class="err"></p>${retry ? `<button type="button" class="btn block mc-retry">${icon("refresh")}<span>Try again</span></button>` : ""}</div>`));
    setText(slot.querySelector("p"), text);
    slot.querySelector(".mc-retry")?.addEventListener("click", pull);
  }

  async function start() {
    wait(); waitText("Checking this Mac…");
    const s = await localStatus();
    if (!dlg.open) return;
    if (s && s.rudder === false) return showInstall(s);
    showStart();
  }

  async function pull() {
    if (busy) return;
    busy = true;
    wait(); waitText("Copying the agent…", "This can take a few minutes. You can keep using Rudder.");
    const id = nextId();
    const r = await ask(waiting.local, id, { type: "openLocal", id, runId, repo }, PULL_TIMEOUT);
    busy = false;
    if (!r) { if (dlg.open) showError("No answer from the Mac app. Try again."); else toast("The copy did not finish", { error: true }); return; }
    if (r.cancelled) { if (dlg.open) showStart(); return; }
    if (!r.ok) {
      const msg = r.error || "The copy did not finish.";
      if (/not (found|installed)|command not found/i.test(msg)) return showInstall(null);
      if (dlg.open) showError(msg); else toast(msg, { error: true });
      return;
    }
    if (!dlg.open) toast(`A copy is on this Mac at ${r.path}`);
    else showResult(r);
  }

  start();
}
