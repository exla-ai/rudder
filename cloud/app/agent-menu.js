// Rename, pin, branch, open beside and close an agent from its row. Desktop
// rows get a menu on right click, the context-menu key or Shift+F10; a
// sidebar title renames on double click, and the agent screen's title on a
// click. The dashboard saves a name the way its own rename does (native
// rename_agent_at), so every screen shows it. A pin keeps an agent from the
// cleanup that deletes agents nobody touched for a day. Close is the app's
// one delete (closeAgent here, after deleteAgent in app.js asks).
import { h, icon } from "/app/render.js";
import { app, api, openSheet, toast } from "/app/core.js";
import { hideClosed } from "/app/closing.js";

/** The longest name the dashboard keeps (native AGENT_TITLE_MAX_CHARS). */
export const TITLE_MAX = 200;
// How long a rename or pin made here waits for the polled state to agree.
const AGREE_MS = 60_000;

// A rename or pin made here wins over the polled state until the state says
// the same (the dashboard publishes it a moment later), so a row does not
// flick back to its old name in between.
const edits = new Map(); // `${ws}|${run}` -> { at, title?, pinned? }
const editKey = (ws, run) => `${ws}|${run}`;

function remember(ws, run, patch) {
  const k = editKey(ws, run);
  edits.set(k, { ...edits.get(k), ...patch, at: Date.now() });
}

function forget(ws, run, field) {
  const e = edits.get(editKey(ws, run));
  if (e) delete e[field];
}

// Agents closed here and not yet gone from the polled state (closing.js).
const closing = new Map(); // `${ws}|${run}` -> { title, answered }

/** Lay this viewer's unconfirmed renames, pins and closes over a fresh `/api/app/state`. */
export function applyAgentEdits(state) {
  const now = Date.now();
  for (const c of hideClosed(state, closing, now)) {
    toast(`"${c.title || "The agent"}" is still open: its dashboard did not close it`, { error: true });
  }
  for (const p of state?.projects || []) {
    for (const a of p.agents || []) {
      const k = editKey(p.workspaceId, a.id);
      const e = edits.get(k);
      if (!e) continue;
      if (now - e.at > AGREE_MS) { edits.delete(k); continue; }
      if ("title" in e) { if (a.title === e.title) delete e.title; else a.title = e.title; }
      if ("pinned" in e) { if (!!a.pinned === e.pinned) delete e.pinned; else a.pinned = e.pinned; }
      if (!("title" in e) && !("pinned" in e)) edits.delete(k);
    }
  }
}

export const cleanTitle = (text) => String(text || "").replace(/\s+/g, " ").trim();

/**
 * Rename an agent. It shows at once (`find()` returns the agent's current
 * row, `repaint` redraws) and goes back with a toast if the dashboard refuses.
 */
export async function renameAgent({ ws, run, find, title, repaint }) {
  const next = cleanTitle(title);
  const before = find()?.title || "";
  if (!next) {
    toast("A name can't be empty", { error: true });
    repaint();
    return false;
  }
  if (next === before) { repaint(); return true; }
  const set = (value) => { const a = find(); if (a) a.title = value; repaint(); };
  remember(ws, run, { title: next });
  set(next);
  try {
    await api("/api/app/agent-rename", { method: "POST", body: { workspace: ws, run, title: next } });
    return true;
  } catch (err) {
    forget(ws, run, "title");
    set(before);
    if (err.status !== 401) toast(`Couldn't rename it: ${err.message}`, { error: true });
    return false;
  }
}

/** Pin or unpin an agent, shown at once and undone with a toast on failure. */
export async function pinAgent({ ws, run, find, pinned, repaint }) {
  const set = (value) => { const a = find(); if (a) a.pinned = value; repaint(); };
  remember(ws, run, { pinned });
  set(pinned);
  try {
    await api("/api/app/action", { method: "POST", body: { workspace: ws, run, action: pinned ? "pin" : "unpin" } });
    toast(pinned ? "Pinned. The day-old cleanup leaves it alone" : "Unpinned");
    return true;
  } catch (err) {
    forget(ws, run, "pinned");
    set(!pinned);
    if (err.status !== 401) toast(`Couldn't ${pinned ? "pin" : "unpin"} it: ${err.message}`, { error: true });
    return false;
  }
}

/**
 * Close (delete) an agent. Its row leaves every list now, the dashboard stops
 * it and removes its workspace in the background, and a refusal puts the row
 * back with the reason. `project()` returns the agent's current project.
 */
export async function closeAgent({ ws, run, title, project, repaint }) {
  const k = editKey(ws, run);
  closing.set(k, { title, answered: null });
  const list = project()?.agents;
  const at = list ? list.findIndex((a) => a.id === run) : -1;
  const row = at >= 0 ? list.splice(at, 1)[0] : null;
  repaint();
  try {
    const res = await api("/api/app/action", { method: "POST", body: { workspace: ws, run, action: "delete" } });
    closing.set(k, { title, answered: Date.now() });
    // A dashboard busy for longer than the home waits still has it queued.
    toast(res?.status === "queued" ? "Closing. Its dashboard is busy, so it goes in a moment" : "Agent closed");
    app.refreshState();
    return true;
  } catch (err) {
    closing.delete(k);
    const current = project()?.agents;
    if (row && current && !current.some((a) => a.id === run)) current.splice(Math.min(at, current.length), 0, row);
    repaint();
    if (err.status !== 401) toast(`Couldn't close "${title || "the agent"}": ${err.message}`, { error: true });
    app.refreshState();
    return false;
  }
}

/**
 * Branch an agent: a sheet for an optional first message, then the
 * dashboard starts a new agent with a copy of its conversation and files
 * (native branch.rs). The agent itself keeps running as it is.
 */
export function branchAgent({ ws, run, title }) {
  const { dlg, body, close } = openSheet({ title: "Branch agent", className: "branch-sheet" });
  const form = h(`<form style="display:flex;flex-direction:column;gap:10px;min-height:0">
    <p class="create-note" style="text-align:left;margin:0 2px"></p>
    <textarea class="ask-other-text" rows="4" aria-label="First message for the branch" placeholder="What should the branch try? Leave it empty to start it waiting for you." style="max-height:50vh"></textarea>
    <p class="repo-note err" role="alert" hidden style="padding:0 2px;margin:0"></p>
    <div class="d-actions"><button type="button" class="btn" data-act="cancel">Cancel</button><button type="submit" class="btn accent">Branch</button></div>
  </form>`);
  form.querySelector(".create-note").textContent =
    `A new agent starts with a copy of the conversation and files of "${title || "this agent"}", which keeps going as it is.`;
  body.appendChild(form);
  const textarea = form.querySelector("textarea");
  const error = form.querySelector(".err");
  const send = form.querySelector('[type="submit"]');
  let sending = false;
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      form.requestSubmit();
    }
  });
  form.querySelector('[data-act="cancel"]').addEventListener("click", close);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (sending) return;
    sending = true;
    send.disabled = true;
    error.hidden = true;
    const text = textarea.value.trim();
    try {
      const res = await api("/api/app/agent-branch", { method: "POST", body: { workspace: ws, run, text } });
      close();
      // Making its workspace can outlast the wait for an answer.
      toast(res?.status === "queued" ? "Branching. The new agent shows up in a moment" : "Branched into a new agent");
      app.refreshState();
    } catch (err) {
      // Closed while it was sending: the sheet is gone, so say it here.
      if (!dlg.open) {
        if (err.status !== 401) toast(`Couldn't branch it: ${err.message}`, { error: true });
        return;
      }
      error.textContent = err.message || "Couldn't branch it";
      error.hidden = false;
      sending = false;
      send.disabled = false;
    }
  });
  textarea.focus();
}

/** Whether `node`'s text is being edited in place (paints must leave it alone). */
export const isRenaming = (node) => !!node?.querySelector(":scope > input.rename");

/**
 * Edit `node`'s text in place. Enter or leaving the field saves, Esc puts the
 * old text back. `onSave(text)` gets what was typed; `onEnd(byKey)` runs
 * either way, `byKey` when Enter or Esc ended it (not a click elsewhere).
 */
export function editInPlace(node, { value, label, onSave, onEnd }) {
  if (isRenaming(node)) return;
  const input = h(`<input class="rename" type="text" spellcheck="false" autocomplete="off" enterkeyhint="done">`);
  input.maxLength = TITLE_MAX;
  input.value = value;
  input.setAttribute("aria-label", label);
  const before = node.textContent;
  node.replaceChildren(input);
  let done = false;
  const finish = (save, byKey) => {
    if (done) return;
    done = true;
    const text = input.value;
    node.textContent = before;
    onEnd?.(byKey);
    if (save && cleanTitle(text) !== cleanTitle(value)) onSave(text);
  };
  input.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "Enter") { e.preventDefault(); finish(true, true); }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); finish(false, true); }
  });
  input.addEventListener("blur", () => finish(true, false));
  // The field sits in a link: a click in it must not open the agent.
  input.addEventListener("click", (e) => { e.preventDefault(); e.stopPropagation(); });
  input.addEventListener("dblclick", (e) => e.stopPropagation());
  input.focus();
  input.select();
}

let openMenu = null;

/** Close the open agent menu, if any. */
export function closeAgentMenu() { openMenu?.(); }

/**
 * A menu at a point (`x`, `y`: a right click) or under a row (`anchor`: the
 * keyboard). `items` are `{ label, icon, run, danger, disabled }` or "divider".
 * Up and Down move, Esc or a click elsewhere closes, focus goes back to
 * `returnFocus`.
 */
export function showAgentMenu({ x, y, anchor, label, items, returnFocus }) {
  closeAgentMenu();
  const wrap = h(`<div class="agent-menu-wrap"><div class="scrim"></div><div class="menu agent-menu" role="menu"></div></div>`);
  const menu = wrap.querySelector(".menu");
  menu.setAttribute("aria-label", label);
  const close = (refocus = true) => {
    if (openMenu !== close) return;
    openMenu = null;
    wrap.remove();
    document.removeEventListener("keydown", onKey, true);
    removeEventListener("resize", onAway);
    removeEventListener("blur", onAway);
    if (refocus && returnFocus?.isConnected) returnFocus.focus();
  };
  const onAway = () => close(false);
  const buttons = () => [...menu.querySelectorAll("button:not([disabled])")];
  const onKey = (e) => {
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(); return; }
    if (e.key === "Tab") { e.preventDefault(); close(); return; }
    const list = buttons();
    if (!list.length) return;
    const at = list.indexOf(document.activeElement);
    const to = { ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: list.length - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    list[(to + list.length) % list.length].focus();
  };
  for (const item of items) {
    if (item === "divider") { menu.appendChild(h("<hr>")); continue; }
    const b = h(`<button type="button" role="menuitem">${icon(item.icon)}<span></span></button>`);
    b.querySelector("span").textContent = item.label;
    if (item.danger) b.classList.add("danger");
    b.disabled = !!item.disabled;
    b.addEventListener("click", () => { close(false); item.run(); });
    menu.appendChild(b);
  }
  const scrim = wrap.querySelector(".scrim");
  scrim.addEventListener("click", () => close());
  // A right click elsewhere closes this menu and goes to what it was on, so
  // another row opens its own.
  scrim.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    close(false);
    document.elementFromPoint(e.clientX, e.clientY)?.dispatchEvent(new MouseEvent("contextmenu", {
      bubbles: true, cancelable: true, clientX: e.clientX, clientY: e.clientY, button: 2,
    }));
  });
  document.body.appendChild(wrap);
  // At the pointer, or under the row; kept inside the window.
  const r = anchor?.getBoundingClientRect();
  const left = x ?? (r ? r.left + 12 : 0);
  const top = y ?? (r ? r.bottom + 2 : 0);
  const w = menu.offsetWidth, ht = menu.offsetHeight;
  Object.assign(menu.style, {
    position: "fixed", right: "auto", bottom: "auto",
    left: `${Math.max(8, Math.min(left, innerWidth - w - 8))}px`,
    top: `${top + ht + 8 > innerHeight ? Math.max(8, top - ht) : top}px`,
    transformOrigin: "top left",
  });
  openMenu = close;
  document.addEventListener("keydown", onKey, true);
  addEventListener("resize", onAway);
  addEventListener("blur", onAway);
  buttons()[0]?.focus();
  return close;
}

/** The context-menu key or Shift+F10: open a row's menu from the keyboard. */
export const isMenuKey = (e) => e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey);
