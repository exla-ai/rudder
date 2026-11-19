// A project's notes: RUDDER_NOTES.md at the root of its checkout on your cloud
// machine. Only you write it, here. Agents are not told about it and nothing
// else in Rudder writes to it. The editor saves as you type; a save is taken
// only while the file still holds the text it was loaded with, so an edit from
// another device is shown, never overwritten silently. A new notes file is
// kept out of Git (the repository's local exclude) until you turn that off.
// The home side is src/project-files/notes.ts.
import { api, app, toast } from "/app/core.js";
import { h, icon } from "/app/render.js";

const SAVE_DELAY_MS = 700;
const CHECK_MS = 15000;
const FILE = "RUDDER_NOTES.md";
export const NOTES_ICON =`<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 3.5h9.5L19 7v12.5a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1v-15a1 1 0 0 1 1-1z"/><path d="M8.5 9.5h7M8.5 13h7M8.5 16.5h4.5"/></svg>`;

export const notesPath = (ws) => `/app/p/${encodeURIComponent(ws)}/notes`;

/** The project header's Notes button. */
export function notesLink(ws) {
  const a = h(`<a class="icon-btn nt-open" data-link aria-label="Notes" title="Notes">${NOTES_ICON}</a>`);
  a.href = notesPath(ws);
  return a;
}

const asleep = (e) => e?.status === 503;

export function NotesScreen(ws) {
  const el = h(`<section class="screen nt-screen">
    <header class="topbar">
      <a class="icon-btn back" aria-label="Back to the project">${icon("back")}</a>
      <div class="titles"><div class="title">Notes</div><div class="subtitle"><span class="nt-proj"></span></div></div>
      <span class="nt-status" role="status"></span>
    </header>
    <div class="nt-banner" role="alert" hidden></div>
    <div class="nt-msg" hidden></div>
    <textarea class="nt-text" aria-label="Notes" spellcheck="true" hidden
      placeholder="Anything you want to keep about this project. Only you write here; agents are not told about this file."></textarea>
    <footer class="nt-foot" hidden>
      <code class="nt-file">${FILE}</code>
      <button type="button" class="nt-git" role="switch" aria-checked="false" hidden><span>Keep out of Git</span><span class="nt-switch"><span class="nt-knob"></span></span></button>
      <p class="nt-note"></p>
    </footer>
  </section>`);
  const $ = (s) => el.querySelector(s);
  const text = $(".nt-text"), statusEl = $(".nt-status"), bannerEl = $(".nt-banner"), msgEl = $(".nt-msg");
  const footEl = $(".nt-foot"), noteEl = $(".nt-note"), switchEl = $(".nt-git");
  const back = $(".back");
  back.href = `/app/p/${encodeURIComponent(ws)}`;
  back.addEventListener("click", (e) => { e.preventDefault(); app.goBack?.(back.getAttribute("href")); });

  // What the file on the machine holds: `base` at `revision` (null: no file yet).
  let base = "", revision = null, git = null;
  let loaded = false, saving = false, again = false;
  // A save the machine refused because the file changed: the notes on disk now,
  // until you choose to load them or keep yours.
  let conflict = null;
  let saveTimer = 0, checkTimer = 0, flashTimer = 0, gone = false;
  const dirty = () => loaded && text.value !== base;

  // ---------- Status ----------

  function status(textValue, { flash = false, detail = "" } = {}) {
    clearTimeout(flashTimer);
    statusEl.textContent = textValue;
    statusEl.title = detail;
    if (flash) flashTimer = setTimeout(() => { if (statusEl.textContent === textValue) statusEl.textContent = ""; }, 2500);
  }

  function banner(message, actions) {
    const line = h(`<span class="nt-banner-text"></span>`);
    line.textContent = message;
    const buttons = actions.map(([label, run]) => {
      const b = h(`<button type="button" class="btn small"></button>`);
      b.textContent = label;
      b.addEventListener("click", run);
      return b;
    });
    bannerEl.replaceChildren(line, ...buttons);
    bannerEl.hidden = false;
  }
  const clearBanner = () => { bannerEl.hidden = true; bannerEl.replaceChildren(); };

  function paintGit() {
    const exists = revision !== null;
    const choosable = exists && (git?.state === "excluded" || git?.state === "included");
    switchEl.hidden = !choosable;
    switchEl.setAttribute("aria-checked", String(git?.state === "excluded"));
    noteEl.textContent = !git ? ""
      : git.state === "committed" ? "Committed in this repository, so edits show up as changes to commit."
      : git.state === "ignored" ? `${git.by} ignores it, so Git never shows it.`
      : !exists ? "Created on your first edit, and kept out of Git."
      : git.state === "excluded" ? "Listed in this clone's .git/info/exclude, so it never shows up to commit."
      : "Shows up in the repository like any other file, ready to commit.";
  }

  // ---------- Loading ----------

  function showMessage(title, detail, retry) {
    text.hidden = true;
    footEl.hidden = true;
    const box = h(`<div class="nt-msg-box">${icon("alert")}<div><p class="nt-msg-title"></p><p class="nt-msg-detail"></p></div></div>`);
    box.querySelector(".nt-msg-title").textContent = title;
    box.querySelector(".nt-msg-detail").textContent = detail;
    if (retry) {
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
    git = notes.git;
    const at = [text.selectionStart, text.selectionEnd];
    if (text.value !== base) text.value = base;
    if (document.activeElement === text) text.setSelectionRange(Math.min(at[0], base.length), Math.min(at[1], base.length));
    paintGit();
  }

  async function load() {
    msgEl.replaceChildren(h(`<p class="nt-loading">Opening notes</p>`));
    msgEl.hidden = false;
    text.hidden = true;
    let notes;
    try { notes = await api(`/api/app/project-notes?${new URLSearchParams({ workspaceId: ws })}`); }
    catch (e) {
      if (gone || e?.status === 401) return;
      if (asleep(e)) showMessage("Your cloud machine is asleep", "Notes live on it, so they open once it is running.", () => { void load(); });
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
    status("Saving");
    try {
      const result = await api("/api/app/project-notes", { method: "POST", body: {
        version: 1, workspaceId: ws, operation: "write", baseRevision: over === undefined ? revision : over, text: value,
      } });
      // The file already holds this text: an earlier try that timed out landed.
      if (result.outcome === "saved" || result.text === value) {
        base = value;
        revision = result.revision;
        if (result.git) git = result.git;
        conflict = null;
        clearBanner();
        paintGit();
        status(dirty() ? "Edited" : "Saved", { flash: !dirty() });
      } else {
        conflict = result;
        status("Not saved");
        banner(result.revision === null
          ? `${FILE} was deleted on your cloud machine since you opened it. Nothing was saved.`
          : `${FILE} changed on another device since you opened it. Nothing was saved.`, [
          ["Load latest", () => { const latest = conflict; conflict = null; clearBanner(); status(""); if (latest) show(latest); }],
          ["Keep mine", () => { if (conflict) void save(conflict.revision); }],
        ]);
      }
    } catch (e) {
      if (e?.status === 401) return;
      status("Not saved");
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
    status("Edited");
    scheduleSave();
  });
  text.addEventListener("blur", () => { if (dirty()) void save(); });

  // ---------- Edits from another device ----------

  /** Read the file again while nothing here is unsaved. */
  async function check() {
    if (!loaded || gone || saving || conflict || dirty() || document.visibilityState !== "visible") return;
    try {
      const notes = await api(`/api/app/project-notes?${new URLSearchParams({ workspaceId: ws })}`);
      if (gone || saving || conflict || dirty()) return;
      if (notes.revision !== revision || JSON.stringify(notes.git) !== JSON.stringify(git)) show(notes);
      if (statusEl.textContent === "Not up to date") status("");
    } catch (e) {
      if (!gone && e?.status !== 401) status("Not up to date", { detail: e.message });
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

  // ---------- Keep out of Git ----------

  switchEl.addEventListener("click", async () => {
    if (switchEl.disabled || !git) return;
    const keepOut = git.state !== "excluded";
    switchEl.disabled = true;
    switchEl.setAttribute("aria-checked", String(keepOut));
    try {
      const result = await api("/api/app/project-notes", { method: "POST", body: { version: 1, workspaceId: ws, operation: "git", keepOut } });
      git = result.git;
    } catch (e) {
      if (e?.status !== 401) toast(asleep(e) ? "Your cloud machine is asleep. Try again once it is running." : e.message, { error: true });
    } finally {
      switchEl.disabled = false;
      paintGit();
    }
  });

  void load();
  return {
    el,
    update() {
      const p = app.store.state?.projects?.find((x) => x.workspaceId === ws);
      $(".nt-proj").textContent = p ? p.name || p.repo || "" : "";
    },
    destroy() {
      gone = true;
      clearTimeout(checkTimer);
      clearTimeout(flashTimer);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("beforeunload", onUnload);
      // Leaving with edits not saved yet: save them now.
      if (dirty() && !conflict) void save();
    },
  };
}
