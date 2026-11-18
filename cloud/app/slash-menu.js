// The "/" menu in a chat's composer: what "/" offers in the agent's own CLI,
// Claude Code's or Codex's, with their names and descriptions. The home
// builds each list ahead of time (cloud/worker/slash-commands.mjs); the chat
// loads it once when it opens, so the menu shows the moment "/" is typed.
//
// A message is typed into the agent's terminal, so a picked command runs
// there. Commands that open a picker or a panel in the TUI open the agent's
// terminal view as they are sent. /model (and Claude's /effort) open the
// chat's model sheet instead: that is the app's control for them.

import { h, setText } from "/app/render.js";
import { api, coarse } from "/app/core.js";
import { labelOf, rank } from "/app/slash-rank.js";

const lists = new Map(); // `${workspace}|${run}` -> { model, at, promise, data, error }
let seq = 0;

function load(workspace, run, model, { force = false } = {}) {
  const key = `${workspace}|${run}`;
  const have = lists.get(key);
  // Codex's list has a row per service tier of the agent's model.
  if (have && !force && have.model === model) return have;
  if (have?.promise) return have;
  const entry = { model, at: Date.now(), promise: null, data: have?.data ?? null, error: null };
  entry.promise = api(`/api/app/slash-commands?workspace=${encodeURIComponent(workspace)}&run=${encodeURIComponent(run)}`)
    .then((data) => { entry.data = data; entry.error = null; })
    .catch((err) => { entry.data = null; entry.error = err; })
    .finally(() => { entry.promise = null; entry.at = Date.now(); });
  lists.set(key, entry);
  return entry;
}

/**
 * The "/" menu for one agent's composer.
 * `agent()` returns the agent's row; `modelControl` is the chat's model chip
 * (model-picker.js mountChatModelControl); `openTerminal()` shows the agent's
 * terminal view.
 */
export function mountSlashMenu(composer, { workspace, run, agent, modelControl, openTerminal }) {
  const ta = composer.textarea;
  const form = composer.el.querySelector("form");
  const menu = h(`<div class="slash-menu" role="listbox" aria-label="Commands" hidden></div>`);
  menu.id = `slash-menu-${seq++}`;
  composer.el.appendChild(menu);
  ta.setAttribute("aria-autocomplete", "list");
  ta.setAttribute("aria-controls", menu.id);
  ta.setAttribute("aria-expanded", "false");

  let shown = []; // { command, alias } in the menu now
  let active = 0;
  let dismissed = null; // the text Esc closed the menu on
  let painted = null;

  const entry = () => load(workspace, run, agent()?.model ?? null);
  entry().promise?.then(() => paint());

  // "/name" with nothing after it, on the first line: the menu's query.
  const query = () => {
    const m = /^\/(\S*)$/.exec(ta.value);
    return m ? m[1] : null;
  };
  const chipWorks = () => !!modelControl && !modelControl.el.hidden;
  const opensTerminal = (c) => c.opens === "terminal" || (c.opens === "model" && !chipWorks());
  const find = (name) => {
    const data = lists.get(`${workspace}|${run}`)?.data;
    const n = name.toLowerCase();
    return data?.commands.find((c) => c.name.toLowerCase() === n || (c.aliases || []).some((a) => a.toLowerCase() === n)) ?? null;
  };

  const setValue = (value) => {
    ta.value = value;
    ta.focus();
    // The composer grows, enables Send and saves the draft on input.
    ta.dispatchEvent(new Event("input", { bubbles: true }));
    ta.setSelectionRange(value.length, value.length);
  };
  const close = () => {
    menu.hidden = true;
    painted = null;
    ta.setAttribute("aria-expanded", "false");
    ta.removeAttribute("aria-activedescendant");
  };

  const row = ({ command: c, alias }, i) => {
    const el = h(`<div class="sm-row" role="option"><div class="sm-line"><span class="sm-name"></span><span class="sm-hint"></span><span class="sm-note"></span></div><div class="sm-desc"></div></div>`);
    el.id = `${menu.id}-${i}`;
    setText(el.querySelector(".sm-name"), `/${labelOf(c)}${alias && alias !== labelOf(c) ? ` (${alias})` : ""}`);
    const hint = el.querySelector(".sm-hint");
    if (c.hint) setText(hint, c.hint); else hint.remove();
    const note = el.querySelector(".sm-note");
    if (c.opens === "model" && chipWorks()) setText(note, "Opens the model picker");
    else if (opensTerminal(c)) setText(note, "Opens in the terminal");
    else note.remove();
    setText(el.querySelector(".sm-desc"), c.description);
    el.title = c.description;
    el.addEventListener("mousedown", (e) => e.preventDefault()); // keep the caret in the box
    el.addEventListener("mousemove", () => { if (active !== i) { active = i; markActive(); } });
    el.addEventListener("click", () => pick(shown[i], "click"));
    return el;
  };
  const message = (title, detail) => {
    const el = h(`<div class="sm-row sm-msg" role="option" aria-disabled="true"><div class="sm-line"><span class="sm-name"></span></div><div class="sm-desc"></div></div>`);
    setText(el.querySelector(".sm-name"), title);
    setText(el.querySelector(".sm-desc"), detail);
    return el;
  };
  const markActive = () => {
    [...menu.children].forEach((el, i) => el.setAttribute("aria-selected", String(i === active)));
    const el = menu.children[active];
    if (el && shown.length) {
      ta.setAttribute("aria-activedescendant", el.id);
      el.scrollIntoView({ block: "nearest" });
    }
  };

  function paint() {
    const q = query();
    if (q === null || ta.value === dismissed || document.activeElement !== ta) { close(); return; }
    let e = entry();
    // A failed load is tried again when the menu is wanted, a few seconds on.
    if (e.error && !e.promise && Date.now() - e.at > 4000) {
      e = load(workspace, run, agent()?.model ?? null, { force: true });
    }
    if (e.promise && !e.data) e.promise.then(() => paint());
    const sig = `${q}|${!!e.data}|${e.error?.message ?? ""}|${chipWorks()}`;
    if (sig === painted && !menu.hidden) return;
    painted = sig;
    if (e.error) {
      shown = [];
      const why = e.error.status === 401 ? "Signed out." : e.error.waking ? "Waking the cloud machine…" : e.error.message;
      menu.replaceChildren(message("Commands unavailable", why));
    } else if (!e.data) {
      shown = [];
      menu.replaceChildren(message("Loading commands…", ""));
    } else {
      shown = rank(e.data.commands, q, e.data.backend);
      if (!shown.length) { close(); return; }
      menu.replaceChildren(...shown.map(row));
    }
    active = 0;
    menu.hidden = false;
    ta.setAttribute("aria-expanded", "true");
    markActive();
    menu.scrollTop = 0;
  }

  // As in both CLIs, Enter runs the command at once, except one that waits
  // for its arguments ("required": a Claude prompt that names them); Tab
  // fills it in with a space after. A click fills it in too, except one that
  // opens in the terminal, which goes at once. A row found by its alias puts
  // the alias in the box, as Claude does.
  function pick({ command: c, alias }, how) {
    if (c.opens === "model" && chipWorks()) {
      setValue("");
      close();
      modelControl.el.click();
      return;
    }
    const name = alias ?? c.name;
    if (c.args === "required" || how === "tab" || (how === "click" && !opensTerminal(c))) {
      setValue(`/${name} `);
      close();
      return;
    }
    setValue(`/${name}`);
    close();
    form.requestSubmit();
  }

  // Before the composer sends: a bare /model goes to the model sheet, and a
  // command that opens UI in the TUI brings up the terminal view with it.
  function beforeSend(e) {
    const m = /^\/(\S+)(?:\s+([\s\S]*))?$/.exec(ta.value.trim());
    const c = m && find(m[1]);
    if (!c) return;
    const rest = (m[2] || "").trim();
    if (c.opens === "model" && !rest && chipWorks()) {
      e.preventDefault();
      e.stopPropagation();
      setValue("");
      close();
      modelControl.el.click();
      return;
    }
    if (opensTerminal(c) && (c.when === "always" || !rest || c.opens === "model")) openTerminal();
  }

  // Capture on the composer runs before the composer's own handlers.
  composer.el.addEventListener("keydown", (e) => {
    if (e.target !== ta || e.isComposing) return;
    if (!menu.hidden && shown.length) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        active = (active + (e.key === "ArrowDown" ? 1 : shown.length - 1)) % shown.length;
        markActive();
        return;
      }
      if ((e.key === "Enter" && !e.shiftKey) || (e.key === "Tab" && !e.shiftKey)) {
        e.preventDefault();
        e.stopPropagation();
        const c = shown[active].command;
        const exact = query()?.toLowerCase() === c.name.toLowerCase();
        // Enter on a command already typed out in full sends it as typed.
        if (e.key === "Enter" && exact && c.args !== "required" && !(c.opens === "model" && chipWorks())) {
          close();
          form.requestSubmit();
          return;
        }
        pick(shown[active], e.key === "Enter" ? "enter" : "tab");
        return;
      }
    }
    if (!menu.hidden && e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      dismissed = ta.value;
      close();
      return;
    }
    // The composer sends on Enter like this (app.js createComposer).
    if (e.key === "Enter" && menu.hidden && (e.metaKey || e.ctrlKey || (!coarse && !e.shiftKey))) beforeSend(e);
  }, true);
  composer.el.addEventListener("submit", beforeSend, true);
  ta.addEventListener("input", () => { if (ta.value !== dismissed) dismissed = null; paint(); });
  ta.addEventListener("focus", paint);
  ta.addEventListener("blur", close);
}
