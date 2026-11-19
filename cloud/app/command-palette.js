// The command palette: Cmd+K on a Mac (Ctrl+K elsewhere) from anywhere in the
// app, text fields included, opens a search box over the page. Typing filters
// its rows (command-rank.js); Up and Down move, Enter runs, Esc or a click
// outside closes, and focus goes back where it was. The same keys close it
// again. Not while another dialog or sheet is open, as with Alt+[ and Alt+].
//
// app.js passes `commands()`, which builds the rows fresh for the page on
// screen: { label, detail?, icon (svg html), keywords?, searchOnly?, run }.
import { h, icon, setText } from "/app/render.js";
import { rankCommands } from "/app/command-rank.js";

const MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform);

const isPaletteKey = (e) => !e.repeat && !e.altKey && !e.shiftKey && e.key?.toLowerCase() === "k"
  && (MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey);

export function mountCommandPalette(commands) {
  let palette = null;
  document.addEventListener("keydown", (e) => {
    if (!isPaletteKey(e)) return;
    if (palette?.dlg.isConnected) { e.preventDefault(); palette.close(); return; }
    if (document.querySelector("dialog[open]")) return;
    const rows = commands();
    if (!rows.length) return;
    e.preventDefault();
    palette = openPalette(rows);
  }, { capture: true });
}

function openPalette(rows) {
  const dlg = h(`<dialog class="cmdk" aria-label="Commands"><div class="cmdk-box">
    <label class="cmdk-in">${icon("search")}<input type="text" role="combobox" aria-expanded="true" aria-controls="cmdk-list" aria-autocomplete="list" autocomplete="off" autocapitalize="off" spellcheck="false" enterkeyhint="go" placeholder="Search agents, projects and commands" aria-label="Search agents, projects and commands"></label>
    <div class="cmdk-list" id="cmdk-list" role="listbox" aria-label="Results"></div>
    <div class="cmdk-foot" aria-hidden="true"><span>↑ ↓ to move</span><span>Enter to open</span><span>Esc to close</span></div>
  </div></dialog>`);
  const input = dlg.querySelector("input");
  const list = dlg.querySelector(".cmdk-list");
  let shown = [];
  let active = 0;

  const paintActive = () => {
    list.querySelectorAll(".cmdk-opt").forEach((o, i) => o.setAttribute("aria-selected", String(i === active)));
    const opt = list.children[active];
    if (opt?.classList.contains("cmdk-opt")) {
      input.setAttribute("aria-activedescendant", opt.id);
      opt.scrollIntoView({ block: "nearest" });
    } else input.removeAttribute("aria-activedescendant");
  };
  const fill = () => {
    shown = rankCommands(rows, input.value);
    active = 0;
    list.replaceChildren(...shown.map((row, i) => {
      const opt = h(`<div class="cmdk-opt" role="option" id="cmdk-o${i}"><span class="cmdk-ic" aria-hidden="true">${row.icon}</span><span class="cmdk-t"><span class="cmdk-l"></span><span class="cmdk-d"></span></span></div>`);
      setText(opt.querySelector(".cmdk-l"), row.label);
      setText(opt.querySelector(".cmdk-d"), row.detail || "");
      opt.dataset.i = String(i);
      return opt;
    }));
    if (!shown.length) list.replaceChildren(h(`<div class="cmdk-none">No matches</div>`));
    list.scrollTop = 0;
    paintActive();
  };
  // Gone at once, not on the close event, which can land after the next
  // Cmd+K. Closing puts focus back where it was.
  const close = () => { dlg.close(); dlg.remove(); };
  const run = (i) => {
    const row = shown[i];
    if (!row) return;
    // Closed first, so a command can move focus on.
    close();
    row.run();
  };

  input.addEventListener("input", fill);
  input.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!shown.length) return;
      active = (active + (e.key === "ArrowDown" ? 1 : -1) + shown.length) % shown.length;
      paintActive();
    } else if (e.key === "Enter") {
      e.preventDefault();
      run(active);
    }
  });
  // The pointer picks too; the field keeps focus.
  list.addEventListener("mousedown", (e) => e.preventDefault());
  list.addEventListener("pointermove", (e) => {
    const opt = e.target.closest(".cmdk-opt");
    if (opt && Number(opt.dataset.i) !== active) { active = Number(opt.dataset.i); paintActive(); }
  });
  list.addEventListener("click", (e) => {
    const opt = e.target.closest(".cmdk-opt");
    if (opt) run(Number(opt.dataset.i));
  });
  dlg.addEventListener("click", (e) => { if (e.target === dlg) close(); });
  dlg.addEventListener("cancel", (e) => { e.preventDefault(); close(); });

  fill();
  document.body.appendChild(dlg);
  dlg.showModal();
  input.focus();
  return { dlg, close };
}
