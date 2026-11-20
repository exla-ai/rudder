// The "/" menu in a chat's composer: what "/" offers in the agent's own CLI,
// Claude Code's, Codex's or OpenCode's, with their names and descriptions. The home
// builds each list ahead of time (cloud/worker/slash-commands.mjs). This
// device keeps the lists it has had, so the menu shows the moment "/" is
// typed: the agent's own last list, or for an agent not opened here yet, the
// newest list of its CLI. The chat asks the home for the current list when it
// opens and when the menu opens, and swaps it in when it arrives.
//
// A message is typed into the agent's terminal, so a picked command runs
// there. Commands that open a picker or a panel in the TUI open the agent's
// terminal view as they are sent. /model (and Claude's /effort) open the
// chat's model sheet instead: that is the app's control for them.

import { h, setText } from "/app/render.js";
import { api, coarse, ls } from "/app/core.js";
import { labelOf, rank } from "/app/slash-rank.js";

// Saved on this device: an index of the last SAVED_LISTS agents' lists,
// newest first ([{ key, backend }]), and each list under its own key.
const SAVED_INDEX = "rudder.slash";
const SAVED_LISTS = 10;
const savedKey = (key) => `rudder.slash.${key}`;
function savedList(key, backend) {
  const index = ls.get(SAVED_INDEX, []);
  const hit = index.find((e) => e.key === key) ?? index.find((e) => e.backend === backend);
  return hit ? ls.get(savedKey(hit.key)) : null;
}
function saveList(key, data) {
  const index = [{ key, backend: data.backend }, ...ls.get(SAVED_INDEX, []).filter((e) => e.key !== key)];
  for (const old of index.splice(SAVED_LISTS)) ls.remove(savedKey(old.key));
  ls.set(savedKey(key), data);
  ls.set(SAVED_INDEX, index);
}

// Asked again when the menu opens, at most this often (a failed ask, after
// RETRY_MS); a first reading (`settling`: Claude's MCP prompts may still
// join) is asked again after SETTLING_MS.
const REFRESH_MS = 10_000;
const RETRY_MS = 4_000;
const SETTLING_MS = 4_000;
const lists = new Map(); // `${workspace}|${run}` -> { data, model, at, promise, error, version }
let seq = 0;

function listFor(workspace, run) {
  const key = `${workspace}|${run}`;
  let entry = lists.get(key);
  if (!entry) {
    entry = { data: null, model: undefined, at: 0, promise: null, error: null, version: 0 };
    lists.set(key, entry);
  }
  return entry;
}

/** Asks the home for the agent's list unless just asked for this model; `done()` runs when the answer is in. */
function refresh(workspace, run, entry, model, done, { force = false } = {}) {
  if (entry.promise) return;
  // Codex's list has a row per service tier of the agent's model.
  if (!force && entry.model === model && Date.now() - entry.at < (entry.error ? RETRY_MS : REFRESH_MS)) return;
  entry.model = model;
  entry.promise = api(`/api/app/slash-commands?workspace=${encodeURIComponent(workspace)}&run=${encodeURIComponent(run)}`)
    .then((data) => {
      entry.data = data;
      entry.error = null;
      saveList(`${workspace}|${run}`, data);
      if (data.settling) setTimeout(() => refresh(workspace, run, entry, model, done, { force: true }), SETTLING_MS);
    })
    // A saved list keeps showing; the menu says it could not be refreshed.
    .catch((err) => { entry.error = err; })
    .finally(() => {
      entry.promise = null;
      entry.at = Date.now();
      entry.version += 1;
      done();
    });
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
  const input = h(`<div class="slash-input"><span class="slash-goal" aria-hidden="true" hidden>/goal</span></div>`);
  const goal = input.firstElementChild;
  ta.before(input);
  input.append(ta);
  // Keep the native text, caret and selection. Only the tint follows scroll.
  const scrollGoal = () => { goal.style.translate = `0 ${-ta.scrollTop}px`; };
  const paintGoal = () => {
    goal.hidden = !/^\/goal(?=\s|$)/.test(ta.value);
    if (!goal.hidden) scrollGoal();
  };
  paintGoal();
  ta.addEventListener("scroll", scrollGoal);
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

  const list = listFor(workspace, run);
  const ask = () => refresh(workspace, run, list, agent()?.model ?? null, paint);
  ask();
  // Until the home answers: the list this device saved (the agent's row may
  // load after the chat, so its CLI is looked up when the list is wanted).
  const data = () => (list.data ??= savedList(`${workspace}|${run}`, agent()?.backend ?? null));

  // "/name" with nothing after it, on the first line: the menu's query.
  const query = () => {
    const m = /^\/(\S*)$/.exec(ta.value);
    return m ? m[1] : null;
  };
  const chipWorks = () => !!modelControl && !modelControl.el.hidden;
  const opensTerminal = (c) => c.opens === "terminal" || (c.opens === "model" && !chipWorks());
  const find = (name) => {
    const n = name.toLowerCase();
    return data()?.commands.find((c) => c.name.toLowerCase() === n || (c.aliases || []).some((a) => a.toLowerCase() === n)) ?? null;
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
    el.classList.toggle("sm-goal", c.name === "goal");
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
    // Opening, the menu asks for the current list; it shows the one it has meanwhile.
    if (menu.hidden) ask();
    const d = data();
    const sig = `${q}|${list.version}|${!!d}|${chipWorks()}`;
    if (sig === painted && !menu.hidden) return;
    // A list swapped in under the same query keeps the row the user was on.
    const keep = !menu.hidden && painted?.startsWith(`${q}|`) ? shown[active]?.command.name : null;
    painted = sig;
    const e = list.error;
    const why = e && (e.status === 401 ? "Signed out." : e.waking ? "Waking the cloud machine…" : e.message);
    if (d) {
      shown = rank(d.commands, q, d.backend);
      if (!shown.length) { close(); return; }
      menu.replaceChildren(...shown.map(row), ...(why ? [message("Could not refresh commands", why)] : []));
    } else {
      shown = [];
      menu.replaceChildren(why ? message("Commands unavailable", why) : message("Loading commands…", ""));
    }
    active = keep ? Math.max(0, shown.findIndex((s) => s.command.name === keep)) : 0;
    menu.hidden = false;
    ta.setAttribute("aria-expanded", "true");
    markActive();
    if (!keep) menu.scrollTop = 0;
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
    const name = data()?.backend === "opencode" ? c.name : alias ?? c.name;
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
    // OpenCode records session actions under their canonical command. In
    // particular /summarize and /compact leave the same compaction marker.
    if (data()?.backend === "opencode" && m[1] !== c.name) {
      ta.value = `/${c.name}${rest ? ` ${rest}` : ""}`;
    }
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
  ta.addEventListener("input", () => {
    paintGoal();
    if (ta.value !== dismissed) dismissed = null;
    paint();
  });
  ta.addEventListener("focus", paint);
  ta.addEventListener("blur", close);
}
