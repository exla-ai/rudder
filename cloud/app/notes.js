// One agent's notes, beside its chat: notes.md in its run folder on your cloud
// machine, so they stay with the chat through a rename or a harness switch
// and go when it is deleted. Only you write them, here. The agent is not told
// about them and nothing else in Rudder writes to them.
//
// The panel takes the right-hand slot (side-panel.js) that Files, Canvas and
// the terminal share: on desktop a resizable column next to the conversation,
// narrower a full sheet. It follows the chat on screen. The editor saves as
// you type; a save is taken only while the file still holds the text it was
// loaded with, so an edit from another device is shown, never overwritten
// silently. The home side is src/project-files/agent-notes.ts.
import { api } from "/app/core.js";
import { h, icon } from "/app/render.js";
import { iconButton } from "/app/file-view.js";
import { UpdateNow } from "/app/home-update.js";
import { openSide, closeSide, sideOccupant, placeSide, dragColumn } from "/app/side-panel.js";

const SAVE_DELAY_MS = 700;
const CHECK_MS = 15000;
export const NOTES_ICON =`<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 3.5h9.5L19 7v12.5a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1v-15a1 1 0 0 1 1-1z"/><path d="M8.5 9.5h7M8.5 13h7M8.5 16.5h4.5"/></svg>`;

const el = h(`<section class="nt" aria-label="Notes">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="nt-head">
    <span class="nt-title">${NOTES_ICON}<span>Notes</span></span>
    <span class="nt-agent"></span>
    <span class="nt-status" role="status"></span>
    <button type="button" class="icon-btn nt-close" aria-label="Close notes" title="Close notes">${icon("x")}</button>
  </header>
  <div class="nt-body"></div>
</section>`);
const agentEl = el.querySelector(".nt-agent"), statusEl = el.querySelector(".nt-status"), bodyEl = el.querySelector(".nt-body");
el.querySelector(".side-grip").addEventListener("pointerdown", dragColumn);
el.querySelector(".nt-close").addEventListener("click", () => { void closePanel(); });

// The panel in the slot: `close` lets another panel take it. Notes never hold
// it: unsaved text is saved on the way out.
const panel = { el, close: async () => { stop(); return true; } };
const state = { target: null, editor: null }; // target: { ws, run, findAgent }
const open = () => sideOccupant() === panel;

// ---------- Public surface ----------

/** The chat header's Notes button. */
export function notesButton({ ws, run, findAgent }) {
  return iconButton("icon-btn nt-toggle", "Notes", NOTES_ICON, () => { void toggleNotes({ ws, run, findAgent }); });
}

/** Open the notes of the chat on screen (Cmd+K, the agent menu after it opened the chat). */
export async function openNotes(target) {
  if (open() && same(state.target, target)) return;
  if (!open() && !(await openSide(panel))) return;
  retarget(target);
}

async function toggleNotes(target) {
  if (open() && same(state.target, target)) { await closePanel(); return; }
  await openNotes(target);
}

/** The route changed, or the other of two panes took focus: open notes follow
 * that chat, and close on a page that is not one. */
export function followNotes(route, findAgent) {
  if (!open()) return;
  placeSide(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  if (route?.name !== "agent" || !route.ws || !route.run) { void closePanel(); return; }
  const target = { ws: route.ws, run: route.run, findAgent };
  if (!same(state.target, target)) retarget(target);
  else paintAgent();
}

const same = (a, b) => !!a && !!b && a.ws === b.ws && a.run === b.run;

async function closePanel() {
  stop();
  closeSide(panel);
}

function stop() {
  state.editor?.destroy();
  state.editor = null;
  state.target = null;
  status("");
}

function retarget(target) {
  state.editor?.destroy();
  state.target = target;
  status("");
  state.editor = NotesEditor(target.ws, target.run);
  bodyEl.replaceChildren(state.editor.el);
  paintAgent();
}

function paintAgent() {
  const t = state.target;
  const a = t ? t.findAgent(t.ws, t.run) : null;
  agentEl.textContent = a ? a.title || a.task || "Untitled agent" : "";
  agentEl.title = agentEl.textContent;
}

let flashTimer = 0;
function status(text, { flash = false, detail = "" } = {}) {
  clearTimeout(flashTimer);
  statusEl.textContent = text;
  statusEl.title = detail;
  if (flash) flashTimer = setTimeout(() => { if (statusEl.textContent === text) statusEl.textContent = ""; }, 2500);
}

// ---------- One chat's notes ----------

const asleep = (e) => e?.status === 503;

function NotesEditor(ws, run) {
  const el = h(`<div class="nt-editor">
    <div class="nt-banner" role="alert" hidden></div>
    <div class="nt-msg" hidden></div>
    <textarea class="nt-text" aria-label="Notes" spellcheck="true" hidden
      placeholder="Anything you want to keep about this agent. Only you write here; the agent is not told about these notes."></textarea>
    <footer class="nt-foot" hidden>
      <p class="nt-note">Kept with this agent on your cloud machine, out of Git, and deleted with it.</p>
    </footer>
  </div>`);
  const $ = (s) => el.querySelector(s);
  const text = $(".nt-text"), bannerEl = $(".nt-banner"), msgEl = $(".nt-msg"), footEl = $(".nt-foot");
  const query = () => `/api/app/agent-notes?${new URLSearchParams({ workspaceId: ws, runId: run })}`;

  // What the file on the machine holds: `base` at `revision` (null: no file yet).
  let base = "", revision = null;
  let loaded = false, saving = false, again = false;
  // A save the machine refused because the file changed: the notes on disk now,
  // until you choose to load them or keep yours.
  let conflict = null;
  let saveTimer = 0, checkTimer = 0, gone = false;
  const dirty = () => loaded && text.value !== base;
  // Status belongs to the chat on screen: a save finishing after the panel
  // moved on says nothing there.
  const say = (...args) => { if (!gone) status(...args); };

  function banner(message, actions) {
    const line = h(`<span class="nt-banner-text"></span>`);
    line.textContent = message;
    const buttons = actions.map(([label, act]) => {
      const b = h(`<button type="button" class="btn small"></button>`);
      b.textContent = label;
      b.addEventListener("click", act);
      return b;
    });
    bannerEl.replaceChildren(line, ...buttons);
    bannerEl.hidden = false;
  }
  const clearBanner = () => { bannerEl.hidden = true; bannerEl.replaceChildren(); };

  // ---------- Loading ----------

  // A home too old for notes: Update now (home-update.js), then they open.
  let updater = null;
  function showMessage(title, detail, retry, { update = false } = {}) {
    text.hidden = true;
    footEl.hidden = true;
    const box = h(`<div class="nt-msg-box">${icon("alert")}<div><p class="nt-msg-title"></p><p class="nt-msg-detail"></p></div></div>`);
    box.querySelector(".nt-msg-title").textContent = title;
    box.querySelector(".nt-msg-detail").textContent = detail;
    updater?.destroy();
    updater = null;
    if (update) {
      updater = UpdateNow({ onDone: () => { void load(); } });
      box.lastElementChild.appendChild(updater.el);
    } else if (retry) {
      const b = h(`<button type="button" class="btn small">Try again</button>`);
      b.addEventListener("click", retry);
      box.lastElementChild.appendChild(b);
    }
    msgEl.replaceChildren(box);
    msgEl.hidden = false;
  }

  function show(notes) {
    base = notes.text;
    revision = notes.revision;
    const at = [text.selectionStart, text.selectionEnd];
    if (text.value !== base) text.value = base;
    if (document.activeElement === text) text.setSelectionRange(Math.min(at[0], base.length), Math.min(at[1], base.length));
  }

  async function load() {
    msgEl.replaceChildren(h(`<p class="nt-loading">Opening notes</p>`));
    msgEl.hidden = false;
    text.hidden = true;
    let notes;
    try { notes = await api(query()); }
    catch (e) {
      if (gone || e?.status === 401) return;
      if (asleep(e)) showMessage("Your cloud machine is asleep", "Notes live on it, so they open once it is running.", () => { void load(); });
      else if (e?.needsUpdate) showMessage("Update your cloud machine to use notes", e.message, null, { update: true });
      else showMessage("Could not open the notes", e.message, () => { void load(); });
      return;
    }
    if (gone) return;
    loaded = true;
    show(notes);
    msgEl.hidden = true;
    text.hidden = false;
    footEl.hidden = false;
    scheduleCheck();
  }

  // ---------- Saving ----------

  function scheduleSave() {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => { void save(); }, SAVE_DELAY_MS);
  }

  /** Save what is in the editor. `over` saves over the notes a conflict found. */
  async function save(over = undefined) {
    clearTimeout(saveTimer);
    if (!loaded) return;
    if (saving) { again = true; return; }
    if (conflict && over === undefined) return;
    if (!dirty() && over === undefined) return;
    const value = text.value;
    saving = true;
    say("Saving");
    try {
      const result = await api("/api/app/agent-notes", { method: "POST", body: {
        version: 1, workspaceId: ws, runId: run, operation: "write", baseRevision: over === undefined ? revision : over, text: value,
      } });
      // The file already holds this text: an earlier try that timed out landed.
      if (result.outcome === "saved" || result.text === value) {
        base = value;
        revision = result.revision;
        conflict = null;
        clearBanner();
        say(dirty() ? "Edited" : "Saved", { flash: !dirty() });
      } else {
        conflict = result;
        say("Not saved");
        banner(result.revision === null
          ? "These notes were deleted on your cloud machine since you opened them. Nothing was saved."
          : "These notes changed on another device since you opened them. Nothing was saved.", [
          ["Load latest", () => { const latest = conflict; conflict = null; clearBanner(); say(""); if (latest) show(latest); }],
          ["Keep mine", () => { if (conflict) void save(conflict.revision); }],
        ]);
      }
    } catch (e) {
      if (e?.status === 401) return;
      say("Not saved");
      banner(asleep(e)
        ? "Not saved: your cloud machine is asleep. Your text stays here and saves once it is running."
        : `Not saved: ${e.message}`, [["Try again", () => { clearBanner(); void save(over); }]]);
    } finally {
      saving = false;
      if (again) { again = false; if (dirty() && !conflict) scheduleSave(); }
    }
  }

  text.addEventListener("input", () => {
    if (conflict) return;
    say("Edited");
    scheduleSave();
  });
  text.addEventListener("blur", () => { if (dirty()) void save(); });

  // ---------- Edits from another device ----------

  /** Read the file again while nothing here is unsaved. */
  async function check() {
    if (!loaded || gone || saving || conflict || dirty() || document.visibilityState !== "visible") return;
    try {
      const notes = await api(query());
      if (gone || saving || conflict || dirty()) return;
      if (notes.revision !== revision) show(notes);
      if (statusEl.textContent === "Not up to date") say("");
    } catch (e) {
      if (!gone && e?.status !== 401) say("Not up to date", { detail: e.message });
    }
  }
  // Text a save could not land (a sleeping machine) is tried again on the same
  // beat, so it saves once the machine is running.
  function scheduleCheck() {
    clearTimeout(checkTimer);
    if (gone) return;
    checkTimer = setTimeout(async () => { await (dirty() && !conflict ? save() : check()); scheduleCheck(); }, CHECK_MS);
  }
  const onVisible = () => {
    if (document.visibilityState === "visible") void check();
    else if (dirty()) void save();
  };
  document.addEventListener("visibilitychange", onVisible);
  const onUnload = (e) => { if (dirty() || saving) e.preventDefault(); };
  window.addEventListener("beforeunload", onUnload);

  void load();
  return {
    el,
    destroy() {
      // Leaving with edits not saved yet: save them now.
      if (dirty() && !conflict) void save();
      gone = true;
      updater?.destroy();
      clearTimeout(checkTimer);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("beforeunload", onUnload);
    },
  };
}
