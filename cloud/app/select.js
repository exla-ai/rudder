// A dropdown that looks like the app, laid over a real <select>. The select
// stays the source of truth and stays in its form, so code keeps adding
// options, reading and setting .value, toggling .disabled and listening for
// "change". It is hidden; a button shows the choice and opens the list: a
// popover under (or above) the button, or a bottom sheet at phone width.
//
//   container.appendChild(dropdown(sel));   // or dropdown(sel) in place
//
// An option's data-sub is a second, quieter line under its label.

import { h, icon, setText } from "/app/render.js";
import { openSheet } from "/app/core.js";

// The same widths as the desktop shell: narrower than that gets the sheet.
const WIDE = matchMedia("(min-width: 1024px), (min-width: 768px) and (pointer: coarse)");
const NATIVE_VALUE = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value");
let seq = 0;

export function dropdown(sel) {
  const id = `dd${++seq}`;
  const wrap = h(`<span class="dd-wrap"></span>`);
  const btn = h(`<button type="button" class="dd" aria-haspopup="listbox" aria-expanded="false"><span class="dd-v"></span>${icon("chevron").replace("<svg", '<svg class="dd-chev"')}</button>`);
  // The select's classes size the button where it sits (.role-select rules).
  btn.className = `${sel.className} dd`;
  // A <label for> on the select names the button instead.
  if (sel.id) { btn.id = sel.id; sel.removeAttribute("id"); }
  if (sel.parentNode) sel.replaceWith(wrap);
  wrap.append(btn, sel);
  sel.hidden = true;
  sel.tabIndex = -1;

  const options = () => [...sel.options];
  const nameOf = () => sel.getAttribute("aria-label") || btn.labels?.[0]?.textContent || "";
  function paint() {
    const o = sel.selectedOptions[0];
    setText(btn.querySelector(".dd-v"), o ? o.textContent : "");
    btn.classList.toggle("dd-ph", !sel.value);
    btn.disabled = sel.disabled;
    btn.setAttribute("aria-label", [nameOf(), o?.textContent].filter(Boolean).join(", "));
    if (list) fill();
  }
  // Setting .value from code repaints too; a MutationObserver can't see it.
  Object.defineProperty(sel, "value", {
    configurable: true,
    get: () => NATIVE_VALUE.get.call(sel),
    set: (v) => { NATIVE_VALUE.set.call(sel, v); paint(); },
  });
  new MutationObserver(paint).observe(sel, { childList: true, subtree: true, attributes: true, characterData: true });

  // The open list. `list` is the listbox; `close` puts everything away.
  let list = null, close = null, active = -1;
  let typed = "", typedAt = 0;
  function fill() {
    const cur = sel.selectedIndex;
    list.replaceChildren(...options().map((o, i) => {
      const row = h(`<div class="dd-opt" role="option"><span class="dd-ot"><span class="dd-on"></span></span>${icon("check").replace("<svg", '<svg class="dd-ck"')}</div>`);
      row.id = `${id}-${i}`;
      row.dataset.i = String(i);
      setText(row.querySelector(".dd-on"), o.textContent);
      if (o.dataset.sub) {
        const sub = h(`<span class="dd-os"></span>`);
        setText(sub, o.dataset.sub);
        row.querySelector(".dd-ot").appendChild(sub);
      }
      row.setAttribute("aria-selected", String(i === cur));
      if (o.disabled) row.setAttribute("aria-disabled", "true");
      return row;
    }));
    if (active >= list.children.length) active = -1;
    mark(active < 0 ? cur : active, false);
  }
  function mark(i, scroll = true) {
    active = i;
    for (const row of list.children) row.classList.toggle("on", Number(row.dataset.i) === i);
    const row = list.children[i];
    if (row) {
      list.setAttribute("aria-activedescendant", row.id);
      if (scroll) row.scrollIntoView({ block: "nearest" });
    } else list.removeAttribute("aria-activedescendant");
  }
  const enabled = () => options().map((o, i) => (o.disabled ? -1 : i)).filter((i) => i >= 0);
  function step(dir) {
    const ok = enabled();
    if (!ok.length) return;
    const at = ok.indexOf(active);
    mark(at < 0 ? ok[dir > 0 ? 0 : ok.length - 1] : ok[Math.max(0, Math.min(ok.length - 1, at + dir))]);
  }
  // Type-ahead: letters typed close together find the next label they start,
  // past any leading symbol ("g" finds "#general").
  function typeAhead(ch) {
    const now = Date.now();
    typed = now - typedAt < 700 ? typed + ch.toLowerCase() : ch.toLowerCase();
    typedAt = now;
    const ok = enabled();
    const from = Math.max(0, ok.indexOf(active) + (typed.length === 1 ? 1 : 0));
    const order = [...ok.slice(from), ...ok.slice(0, from)];
    const hit = order.find((i) => {
      const text = sel.options[i].textContent.trim().toLowerCase();
      return text.startsWith(typed) || text.replace(/^[^\p{L}\p{N}]+/u, "").startsWith(typed);
    });
    if (hit !== undefined) mark(hit);
  }
  function choose(i) {
    const o = sel.options[i];
    if (!o || o.disabled) return;
    close({ focus: true });
    if (sel.selectedIndex === i) return;
    sel.selectedIndex = i;
    paint();
    sel.dispatchEvent(new Event("input", { bubbles: true }));
    sel.dispatchEvent(new Event("change", { bubbles: true }));
  }
  function onListKey(e) {
    const typing = Date.now() - typedAt < 700;
    if (e.key === "ArrowDown") step(1);
    else if (e.key === "ArrowUp") step(-1);
    else if (e.key === "Home") mark(enabled()[0] ?? -1);
    else if (e.key === "End") mark(enabled().at(-1) ?? -1);
    else if (e.key === "PageDown") step(8);
    else if (e.key === "PageUp") step(-8);
    else if (e.key === "Enter" || (e.key === " " && !typing)) choose(active);
    else if (e.key === "Escape" || e.key === "Tab") close({ focus: true });
    else if (e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey) typeAhead(e.key);
    else return;
    e.preventDefault();
    e.stopPropagation();
  }

  function open() {
    if (list || sel.disabled) return;
    list = h(`<div class="dd-list" role="listbox" tabindex="-1"></div>`);
    list.setAttribute("aria-label", nameOf());
    active = -1;
    fill();
    list.addEventListener("keydown", onListKey);
    list.addEventListener("click", (e) => {
      const row = e.target.closest(".dd-opt");
      if (row) choose(Number(row.dataset.i));
    });
    list.addEventListener("pointermove", (e) => {
      const row = e.target.closest(".dd-opt:not([aria-disabled])");
      if (row && Number(row.dataset.i) !== active) mark(Number(row.dataset.i), false);
    });
    btn.setAttribute("aria-expanded", "true");
    if (WIDE.matches) openPopover(); else openInSheet();
    list.focus({ preventScroll: true });
    list.children[active]?.scrollIntoView({ block: "nearest" });
  }
  function done(focus) {
    list = null; close = null;
    btn.setAttribute("aria-expanded", "false");
    if (focus && btn.isConnected) btn.focus({ preventScroll: true });
  }

  // Under the button, or above it when there is more room there; always
  // inside the window. A click elsewhere, Esc or the page scrolling closes it.
  function openPopover() {
    const pop = h(`<div class="dd-pop"></div>`);
    pop.appendChild(list);
    document.body.appendChild(pop);
    const r = btn.getBoundingClientRect();
    const gap = 4, edge = 8;
    const width = Math.min(Math.max(r.width, 200), innerWidth - 2 * edge);
    const natural = pop.scrollHeight;
    const below = innerHeight - r.bottom - gap - edge, above = r.top - gap - edge;
    const down = natural <= below || below >= above;
    Object.assign(pop.style, {
      left: `${Math.min(Math.max(edge, r.left), innerWidth - width - edge)}px`,
      width: `${width}px`,
      maxHeight: `${Math.min(340, down ? below : above)}px`,
      top: down ? `${r.bottom + gap}px` : "auto",
      bottom: down ? "auto" : `${innerHeight - r.top + gap}px`,
      transformOrigin: down ? "top" : "bottom",
    });
    const ac = new AbortController();
    const opts = { capture: true, signal: ac.signal };
    document.addEventListener("pointerdown", (e) => { if (!pop.contains(e.target) && !btn.contains(e.target)) close(); }, opts);
    addEventListener("scroll", (e) => { if (!pop.contains(e.target)) close(); }, opts);
    addEventListener("resize", () => close(), opts);
    close = ({ focus = false } = {}) => { ac.abort(); pop.remove(); done(focus); };
  }
  // A phone: the list in a sheet from the bottom, rows big enough to tap.
  function openInSheet() {
    const sheet = openSheet({ title: nameOf(), className: "dd-sheet" });
    sheet.body.appendChild(list);
    let focusBack = false;
    sheet.dlg.addEventListener("close", () => done(focusBack));
    close = ({ focus = false } = {}) => { focusBack = focus; sheet.close(); };
  }

  btn.addEventListener("click", () => (list ? close({ focus: true }) : open()));
  btn.addEventListener("keydown", (e) => {
    if (list) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); open(); }
    else if (e.key.length === 1 && e.key !== " " && !e.metaKey && !e.ctrlKey && !e.altKey) { e.preventDefault(); open(); typeAhead(e.key); }
  });
  paint();
  return wrap;
}
