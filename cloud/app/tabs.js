// Tabs in the desktop shell: several pages open at once, each in its own tab,
// like a browser. A tab is a page (an agent's chat, a project, the home
// screen), plus two agents side by side when it has a split. The tab on screen
// is the URL: moving around in it (the sidebar, links, Back) changes that tab.
//
// Tabs group by project (tab-groups.js): a project's tabs sit together after a
// chip with its name, and pages of no project come first. A tab that goes to
// another project's page moves to that project's group. A click on a chip
// collapses its group to the chip, which then says how many tabs it hides and
// whether one of their agents is working or needs you; the tab on screen
// stays out beside it. The chip's menu (right click, the context-menu key or
// Shift+F10) opens a tab in the project, collapses the other projects, sets
// the project's color, or closes the group's tabs. Which groups are collapsed
// belongs to this device; a project's color belongs to the account.
//
// A tab you leave keeps its screens mounted, out of sight at the size they
// had, so its chat, scroll, draft and focus are where you left them, and its
// chat keeps following the agent. Tabs belong to this device and come back
// after a reload. Phones have no shell, and no tabs.
//
// The + button opens a tab. In the Mac app the bar appears with the second
// tab, and Cmd+T opens a tab even with the bar hidden. Cmd+W closes it (the
// last one leaves Cmd+W to the window). Control+Tab and Control+Shift+Tab
// step through the tabs in sight, as do Cmd+Shift+[ and Cmd+Shift+]. Cmd+1 to
// Cmd+8 jump to one and Cmd+9 to the last. Cmd+Option+[ and Cmd+Option+] go to
// the previous and next project, to the tab last used there. Cmd+click on a
// link opens it in a tab at the end of its project's group. A browser keeps
// those keys for its own tabs.
// Drag a tab to reorder it within its group.
import { h, icon, setText, reconcile } from "/app/render.js";
import { ls, PLATFORM } from "/app/core.js";
import { tabDragPosition } from "/app/tab-drag.js";
import { COLORS, arrange, groupColors, groupsOf, ownColor, placeFor, regroup } from "/app/tab-groups.js";
import { rowMenu, showAgentMenu } from "/app/agent-menu.js";
import { teamAgent } from "/app/team.js";

const KEY = "rudder.tabs"; // { tabs: [{ id, path, split }], active, collapsed: [ws] }
const MAC = PLATFORM === "mac";
const newId = () => Math.random().toString(36).slice(2, 10);
const TAB_MOVE = { id: "tab-reorder", duration: 190, easing: "cubic-bezier(.2,.75,.2,1)" };

/**
 * `main`: the shell's main area, whose panes hold the screens. The shell's
 * hooks: `park()` takes the screens off its panes and returns them, per pane
 * ([{ route, screen } | null]); `loadSplit(split)` gives it the split of the
 * tab coming in. `navigate`, `parseRoute`, `projectPath`, `findAgent`,
 * `findProject` and `agentStatus` are the app's own, as are
 * `chosenColor(ws)` (the color the account chose for a project, or null)
 * and `setColor(ws, color)`.
 */
export function Tabs({ main, park, loadSplit, navigate, parseRoute, projectPath, findAgent, findProject, agentStatus, chosenColor, setColor }) {
  // A tab's group: the project of its page (of a split's left agent), or null
  // for a page of no project of yours.
  const groupOf = (t) => {
    const r = parseRoute(t.path);
    if (r.name === "agent" && t.split?.panes.some((p) => p.ws === r.ws && p.run === r.run)) return t.split.panes[0].ws;
    return r.name === "agent" || r.name === "project" ? r.ws : null;
  };
  const projectName = (ws) => {
    const p = findProject(ws);
    return p?.name || p?.repo || "Project";
  };
  const saved = ls.get(KEY);
  // `kept`: the screens of a tab that is not on screen, per pane, in memory only.
  let tabs = arrange(saved ? saved.tabs.map((t) => ({ ...t, kept: null, focus: null })) : [{ id: newId(), path: location.pathname, split: null, kept: null, focus: null }], groupOf);
  let active = saved ? saved.active : tabs[0].id;
  // Projects whose group is collapsed to its chip.
  const collapsed = new Set(saved?.collapsed);
  // Per project, the tab last on screen in its group (Cmd+Option+[ and ]).
  const lastUsed = new Map();
  // The color of each project with a group in the strip (groupColors), as last painted.
  let colors = new Map();
  const cur = () => tabs.find((t) => t.id === active);
  // In sight: the tab on screen, and every tab of an expanded group.
  const shown = (t) => t.id === active || !collapsed.has(groupOf(t));
  const save = () => {
    // A project with no tabs left starts expanded when one opens again.
    for (const key of collapsed) if (!tabs.some((t) => groupOf(t) === key)) collapsed.delete(key);
    ls.set(KEY, { tabs: tabs.map(({ id, path, split }) => ({ id, path, split })), active, collapsed: [...collapsed] });
  };

  const shortcut = (keys) => (MAC ? ` (${keys})` : "");
  const el = h(`<div class="tab-strip">
    <div class="tab-list" role="tablist" aria-label="Tabs"></div>
    <button type="button" class="tab-new" aria-label="New tab">${icon("plus")}</button>
    <span class="sr-only" role="status"></span>
  </div>`);
  const list = el.querySelector(".tab-list");
  const newBtn = el.querySelector(".tab-new");
  newBtn.title = `New tab${shortcut("Cmd+T")}`;
  const announcement = el.querySelector("[role=status]");
  const life = new AbortController();
  const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
  let drag = null;
  let draggedClick = false;
  // The tab last scrolled into sight: a strip too long for the window brings
  // the tab on screen into view when it changes or moves, and only then.
  let inSight = null;

  // A parked screen keeps the size it had, out of sight and out of reach, so
  // nothing in it lays out again or scrolls while it waits.
  const hide = (k) => {
    const r = k.screen.el.getBoundingClientRect();
    k.screen.el.style.width = `${r.width}px`;
    k.screen.el.style.height = `${r.height}px`;
    k.screen.el.classList.add("tab-off");
    k.screen.el.inert = true;
  };
  const unhide = (k) => {
    k.screen.el.style.removeProperty("width");
    k.screen.el.style.removeProperty("height");
    k.screen.el.classList.remove("tab-off");
    k.screen.el.inert = false;
  };
  const drop = (t) => {
    for (const k of t.kept || []) {
      if (!k) continue;
      k.screen.destroy?.();
      k.screen.el.remove();
    }
    t.kept = null;
    t.focus = null;
  };

  // Leaves the tab on screen for tab `id`: its screens park, and the shell
  // opens the other tab's page, taking back any of its screens (revive).
  function select(id) {
    if (id === active) return;
    const from = cur();
    from.kept = park();
    const focused = document.activeElement;
    from.focus = from.kept.some((k) => k?.screen.el.contains(focused)) ? focused : null;
    for (const k of from.kept) if (k) hide(k);
    active = id;
    save();
    const to = cur();
    loadSplit(to.split);
    navigate(to.path, { replace: true });
    // Screens the page no longer has room for (a split on a narrower window).
    const focus = to.focus;
    drop(to);
    if (focus?.isConnected) focus.focus({ preventScroll: true });
    paint();
  }
  // The shell building pane `i` for `path`: this tab's parked screen, if it has one.
  function revive(i, path) {
    const t = cur();
    const k = t.kept?.[i];
    if (!k || k.route.path !== path) return null;
    t.kept[i] = null;
    unhide(k);
    return k.screen;
  }

  // A new tab opens where you start an agent: the project on screen (its task
  // box takes the typing), or the home screen. Every new tab, a link's
  // (`path`) too, goes to the end of its project's group, which opens.
  function open(path = null, { behind = false } = {}) {
    endDrag(false, false);
    const r = parseRoute(location.pathname);
    const t = { id: newId(), path: path || (r.name === "agent" || r.name === "project" ? projectPath(r.ws) : "/app"), split: null, kept: null, focus: null };
    const key = groupOf(t);
    tabs.splice(placeFor(tabs, key, groupOf), 0, t);
    collapsed.delete(key);
    save();
    if (behind) { paint(); return; }
    select(t.id);
    main.querySelector(".dsk-pane > .screen:not(.tab-off) .composer textarea")?.focus();
  }
  // The tab that takes the place of the tabs `gone` (side by side) when they
  // close: the next one in sight to the right, as in a browser, or else to
  // the left.
  function successor(gone) {
    const after = tabs.slice(tabs.indexOf(gone.at(-1)) + 1);
    const before = tabs.slice(0, tabs.indexOf(gone[0])).reverse();
    return after.find(shown) || before.find(shown) || after[0] || before[0];
  }
  function closeAll(gone) {
    if (gone.length >= tabs.length) return;
    endDrag(false, false);
    if (gone.includes(cur())) select(successor(gone).id);
    for (const t of gone) drop(t);
    tabs = tabs.filter((t) => !gone.includes(t));
    save();
    paint();
  }
  const close = (id) => closeAll(tabs.filter((t) => t.id === id));
  const step = (dir) => {
    const seen = tabs.filter(shown);
    const i = seen.indexOf(cur());
    select(seen[(i + dir + seen.length) % seen.length].id);
  };
  // To the previous or next project's group, to the tab last on screen there.
  const stepGroup = (dir) => {
    const groups = groupsOf(tabs, groupOf).filter((g) => g.key !== null);
    const here = groups.findIndex((g) => g.key === groupOf(cur()));
    const to = here < 0 ? groups.at(dir > 0 ? 0 : -1) : groups[(here + dir + groups.length) % groups.length];
    if (!to || to.key === groupOf(cur())) return;
    select((to.tabs.find((t) => t.id === lastUsed.get(to.key)) || to.tabs[0]).id);
  };
  function toggle(key) {
    endDrag(false, false);
    if (collapsed.has(key)) collapsed.delete(key);
    else collapsed.add(key);
    save();
    paint();
  }
  // A chip's menu: right click, or the context-menu key or Shift+F10 on it.
  function groupMenu(key, at, chip) {
    const name = projectName(key);
    const own = tabs.filter((t) => groupOf(t) === key);
    const others = groupsOf(tabs, groupOf).filter((g) => g.key !== null && g.key !== key);
    showAgentMenu({
      ...at,
      label: name,
      returnFocus: chip,
      items: [
        { label: `New tab in ${name}`, icon: "plus", run: () => open(projectPath(key)) },
        {
          label: "Collapse other projects", icon: "chevron", disabled: !others.length,
          run: () => {
            for (const g of others) collapsed.add(g.key);
            collapsed.delete(key);
            save();
            paint();
          },
        },
        "divider",
        {
          label: "Color",
          swatches: COLORS.map((c) => ({ value: c, label: `Color: ${c[0].toUpperCase()}${c.slice(1)}`, checked: c === colors.get(key) })),
          pick: (c) => setColor(key, c),
        },
        "divider",
        // The last tabs stay, as the last tab does.
        { label: own.length === 1 ? "Close tab" : `Close ${own.length} tabs`, icon: "x", disabled: own.length === tabs.length, run: () => closeAll(own) },
      ],
    });
  }

  function reorder(from, to) {
    if (from === to) return;
    const focused = document.activeElement;
    const [tab] = tabs.splice(from, 1);
    tabs.splice(to, 0, tab);
    save();
    paint();
    if (list.contains(focused) && focused !== document.activeElement) focused.focus({ preventScroll: true });
    const group = tabs.filter((t) => groupOf(t) === groupOf(tab));
    announcement.textContent = `${describe(tab).title}, tab ${group.indexOf(tab) + 1} of ${group.length}`;
  }

  function tabPositions() {
    return [...list.children].map((tab) => [tab, tab.getBoundingClientRect().left, !tab.hidden]);
  }

  // Tabs and chips glide from where they were; one coming into sight just appears.
  function settle(positions) {
    if (reducedMotion.matches) return;
    const offsets = positions.filter(([tab, , seen]) => seen && !tab.hidden).map(([tab, left]) => [tab, left - tab.getBoundingClientRect().left]);
    for (const [tab, offset] of offsets) {
      if (Math.abs(offset) > 0.5) {
        tab.animate([{ transform: `translateX(${offset}px)` }, { transform: "translateX(0)" }], TAB_MOVE);
      }
    }
  }

  function cancelSettle() {
    for (const tab of list.children) {
      for (const animation of tab.getAnimations()) {
        if (animation.id === TAB_MOVE.id) animation.cancel();
      }
    }
  }

  function renderDrag() {
    const d = drag;
    d.frame = 0;
    const bounds = list.getBoundingClientRect();
    const edge = Math.min(40, bounds.width / 4);
    const scroll = list.scrollLeft;
    // Holding near an edge reveals tabs outside the visible part of the strip.
    const proximity = Math.max(0, d.x - bounds.right + edge) - Math.max(0, bounds.left + edge - d.x);
    if (edge && proximity) list.scrollLeft += Math.sign(proximity) * Math.min(16, Math.ceil(16 * Math.abs(proximity) / edge));
    const { to, offset } = tabDragPosition(d, bounds, list);
    if (to !== d.to) {
      d.items.forEach(({ tab }, i) => {
        if (i === d.from) return;
        const shift = i > d.from && i <= to ? -d.stride : i < d.from && i >= to ? d.stride : 0;
        tab.style.transform = shift ? `translateX(${shift}px)` : "";
      });
      d.to = to;
    }
    d.tab.style.transform = `translateX(${offset}px)`;
    if (list.scrollLeft !== scroll) d.frame = requestAnimationFrame(renderDrag);
  }

  function endDrag(commit, animated = true) {
    const d = drag;
    drag = null;
    if (!d?.items) return;
    cancelAnimationFrame(d.frame);
    const positions = animated ? tabPositions() : [];
    list.removeAttribute("data-reordering");
    d.tab.removeAttribute("data-dragging");
    for (const { tab } of d.items) tab.style.removeProperty("transform");
    if (list.hasPointerCapture(d.pointer)) list.releasePointerCapture(d.pointer);
    const at = (i) => tabs.findIndex((t) => t.id === d.items[i].tab.dataset.key);
    if (commit) reorder(at(d.from), at(d.to));
    if (animated) settle(positions);
  }

  list.addEventListener("pointerdown", (e) => {
    if (drag || !e.isPrimary || e.button !== 0) return;
    draggedClick = false;
    const button = e.target.closest(".tab-b");
    if (!button || tabs.length < 2) return;
    const tab = button.closest(".tab");
    drag = { tab, pointer: e.pointerId, start: e.clientX, grab: e.clientX - tab.getBoundingClientRect().left };
  });
  window.addEventListener("pointermove", (e) => {
    const d = drag;
    if (!d || e.pointerId !== d.pointer) return;
    if (!d.items) {
      if (Math.abs(e.clientX - d.start) < 6) return;
      // A tab moves among the tabs of its group in sight.
      const peers = [...list.children].filter((x) => x.classList.contains("tab") && !x.hidden && x.dataset.group === d.tab.dataset.group);
      if (peers.length < 2) { drag = null; return; }
      cancelSettle();
      const origin = list.getBoundingClientRect().left - list.scrollLeft;
      d.items = peers.map((tab) => {
        const rect = tab.getBoundingClientRect();
        return { tab, left: rect.left - origin, center: rect.left - origin + rect.width / 2 };
      });
      d.from = d.to = d.items.findIndex(({ tab }) => tab === d.tab);
      d.width = d.tab.getBoundingClientRect().width;
      d.right = peers.at(-1).getBoundingClientRect().right - origin;
      d.stride = d.width + parseFloat(getComputedStyle(list).columnGap);
      list.setPointerCapture(d.pointer);
      list.setAttribute("data-reordering", "");
      d.tab.setAttribute("data-dragging", "");
      draggedClick = true;
    }
    d.x = e.clientX;
    if (!d.frame) d.frame = requestAnimationFrame(renderDrag);
  }, { signal: life.signal });
  window.addEventListener("pointerup", (e) => {
    if (e.pointerId !== drag?.pointer) return;
    if (drag.items) {
      cancelAnimationFrame(drag.frame);
      drag.x = e.clientX;
      renderDrag();
    }
    endDrag(true);
  }, { signal: life.signal });
  const cancelPointer = (e) => { if (e.pointerId === drag?.pointer) endDrag(false); };
  window.addEventListener("pointercancel", cancelPointer, { signal: life.signal });
  list.addEventListener("lostpointercapture", (e) => { if (e.target === list) cancelPointer(e); });
  window.addEventListener("blur", () => endDrag(false, false), { signal: life.signal });
  window.addEventListener("resize", () => endDrag(false, false), { signal: life.signal });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !drag?.items) return;
    e.preventDefault();
    e.stopPropagation();
    endDrag(false);
  }, { capture: true, signal: life.signal });

  newBtn.addEventListener("click", () => open());
  // A double click on the strip's empty part opens a tab, as in a browser.
  el.addEventListener("dblclick", (e) => { if (e.target === el || e.target === list) open(); });
  list.addEventListener("click", (e) => {
    if (draggedClick && e.detail) {
      draggedClick = false;
      return;
    }
    const chip = e.target.closest(".tab-chip");
    if (chip) { toggle(chip.dataset.group); return; }
    const tab = e.target.closest(".tab");
    if (!tab) return;
    if (e.target.closest(".tab-x")) close(tab.dataset.key);
    else select(tab.dataset.key);
  });
  rowMenu(list, ".tab-chip", (chip, at) => groupMenu(chip.dataset.group, at, chip));
  // The middle button closes a tab.
  list.addEventListener("auxclick", (e) => {
    const tab = e.button === 1 ? e.target.closest(".tab") : null;
    if (!tab) return;
    e.preventDefault();
    close(tab.dataset.key);
  });
  // Left and Right move between chips and tabs in sight, Enter or Space opens
  // a tab or collapses a group.
  list.addEventListener("keydown", (e) => {
    if (e.shiftKey || (e.key !== "ArrowLeft" && e.key !== "ArrowRight")) return;
    const btns = [...list.querySelectorAll(".tab-chip, .tab:not([hidden]) > .tab-b")];
    const at = btns.indexOf(document.activeElement);
    if (at < 0) return;
    e.preventDefault();
    const next = at + (e.key === "ArrowRight" ? 1 : -1);
    btns[(next + btns.length) % btns.length].focus();
  });

  if (MAC) {
    // The Mac window catches Control+Tab even if focus is in an iframe.
    document.addEventListener("rudder:cycle-tab", (e) => {
      if (document.querySelector("dialog[open]")) return;
      if (e.detail === 1 || e.detail === -1) step(e.detail);
    }, { signal: life.signal });
    document.addEventListener("keydown", (e) => {
      if (!e.metaKey || e.ctrlKey || document.querySelector("dialog[open]")) return;
      const key = e.key.toLowerCase();
      const bracket = e.code === "BracketLeft" || e.code === "BracketRight";
      const dir = e.code === "BracketRight" ? 1 : -1;
      let act = null;
      if (e.altKey) { if (!e.shiftKey && bracket) act = () => stepGroup(dir); }
      else if (!e.shiftKey && key === "t") act = () => open();
      else if (!e.shiftKey && key === "w" && tabs.length > 1) act = () => close(active);
      else if (e.shiftKey && bracket) act = () => step(dir);
      else if (!e.shiftKey && /^Digit[1-9]$/.test(e.code)) {
        const n = Number(e.code.slice(5));
        const seen = tabs.filter(shown);
        act = () => select((n === 9 ? seen.at(-1) : seen[Math.min(n, seen.length) - 1]).id);
      }
      if (!act) return;
      e.preventDefault();
      e.stopPropagation();
      act();
    }, { capture: true, signal: life.signal });
    // Cmd+click on a link within the app opens it in a tab behind this one
    // (the app's own click handler leaves modified clicks alone).
    document.addEventListener("click", (e) => {
      const a = e.target instanceof Element ? e.target.closest("a[data-link]") : null;
      if (!a || !e.metaKey || e.shiftKey || e.altKey || e.ctrlKey || e.button !== 0 || e.defaultPrevented) return;
      e.preventDefault();
      open(a.getAttribute("href"), { behind: true });
    }, { signal: life.signal });
  }

  // What a tab says: an agent's name and status dot, or the page's name.
  const PAGES = { home: ["Projects", "code"], all: ["All agents", "agents"], "all-agent": ["All agents", "agents"], settings: ["Settings", "settings"], machine: ["Machine & Cache", "server"], digest: ["Digest", "sun"], files: ["Files", "folder"], team: ["Team", "users"] };
  const agentTitle = (a) => a?.title || a?.task || "Untitled agent";
  function describe({ path, split }) {
    const r = parseRoute(path);
    // Two agents side by side: the split icon, the focused one's name, both in the tooltip.
    if (r.name === "agent" && split?.panes.some((p) => p.ws === r.ws && p.run === r.run)) {
      return { title: agentTitle(findAgent(r.ws, r.run)), ic: "split", tip: split.panes.map((p) => agentTitle(findAgent(p.ws, p.run))).join(" and ") };
    }
    if (r.name === "agent" || r.name === "team-agent") {
      const a = r.name === "agent" ? findAgent(r.ws, r.run) : teamAgent(r.member, r.tws, r.run)?.agent;
      return { title: agentTitle(a), status: a ? agentStatus(a) : null };
    }
    if (r.name === "project") return { title: findProject(r.ws)?.name || "Project", ic: "code" };
    const [title, ic] = PAGES[r.name] || PAGES.home;
    return { title, ic };
  }
  // A chip: its project's name, and once collapsed, what it hides. The count
  // is the group's tabs, or "+N" beside the tab on screen; the dot is the
  // most urgent state among the hidden agents, needs you before working.
  function paintChip(chip, { key, tabs: own }) {
    const name = projectName(key);
    const folded = collapsed.has(key);
    const hidden = own.filter((t) => !shown(t));
    const states = hidden.map((t) => describe(t).status).filter(Boolean);
    const needs = states.filter((st) => st.cls === "s-needs").length;
    const working = states.filter((st) => st.cls === "s-working").length;
    const cls = needs ? "s-needs" : working ? "s-working" : "";
    chip.className = `tab-chip${cls ? ` ${cls}` : ""}`;
    chip.dataset.group = key;
    chip.dataset.color = colors.get(key);
    chip.tabIndex = -1;
    chip.setAttribute("aria-expanded", String(!folded));
    setText(chip.querySelector(".tab-chip-n"), name);
    setText(chip.querySelector(".tab-chip-c"), !hidden.length ? "" : hidden.length === own.length ? String(own.length) : `+${hidden.length}`);
    const dot = chip.querySelector(".dot");
    dot.hidden = !cls;
    dot.classList.toggle("live", !needs && states.some((st) => st.live));
    const count = folded ? hidden.length : own.length;
    const said = [name, `${count} ${folded ? "hidden " : ""}${count === 1 ? "tab" : "tabs"}`];
    if (needs) said.push(`${needs} ${needs === 1 ? "needs" : "need"} you`);
    else if (working) said.push(`${working} working`);
    chip.setAttribute("aria-label", said.join(", "));
    chip.title = `${said.join(" · ")}\n${folded ? "Click to show its tabs" : "Click to collapse"}`;
  }

  function paint() {
    const many = tabs.length > 1;
    el.hidden = MAC && !many;
    list.toggleAttribute("data-reorderable", many);
    lastUsed.set(groupOf(cur()), active);
    // Each project's chip, then its tabs; pages of no project have no chip.
    const groups = groupsOf(tabs, groupOf);
    colors = groupColors(groups.filter((g) => g.key !== null).map((g) => g.key), chosenColor);
    const items = groups.flatMap((g) => (g.key === null ? g.tabs : [g, ...g.tabs]));
    reconcile(list, items, (x) => (x.tabs ? `g:${x.key}` : x.id),
      (x) => (x.tabs
        ? h(`<button type="button" class="tab-chip"><span class="dot" hidden></span><span class="tab-chip-n"></span><span class="tab-chip-c"></span></button>`)
        : h(`<div class="tab" role="presentation"><button type="button" class="tab-b" role="tab"><span class="tab-ic"></span><span class="tab-t"></span></button><button type="button" class="tab-x" tabindex="-1">${icon("x")}</button></div>`)),
      (tabEl, t) => {
        if (t.tabs) { paintChip(tabEl, t); return; }
        const d = describe(t);
        const on = t.id === active;
        const key = groupOf(t);
        tabEl.className = `tab${on ? " on" : ""}${key === null ? "" : " grouped"}${d.status ? ` ${d.status.cls}` : ""}`;
        tabEl.dataset.group = key ?? "";
        if (key === null) delete tabEl.dataset.color;
        else tabEl.dataset.color = colors.get(key);
        tabEl.hidden = !shown(t);
        // The chip shows the project; a tab says it too, to a screen reader and in its tooltip.
        const project = key !== null && parseRoute(t.path).name !== "project" ? projectName(key) : "";
        const b = tabEl.querySelector(".tab-b");
        b.setAttribute("aria-selected", String(on));
        b.tabIndex = on ? 0 : -1;
        b.setAttribute("aria-label", [d.title, project, d.status?.label].filter(Boolean).join(", "));
        b.title = [d.tip || d.title, project, d.status?.label].filter(Boolean).join(" · ");
        setText(tabEl.querySelector(".tab-t"), d.title);
        const ic = tabEl.querySelector(".tab-ic");
        const sig = d.status ? `dot ${d.status.live ? "live" : ""}` : d.ic;
        if (ic.dataset.sig !== sig) {
          ic.dataset.sig = sig;
          ic.innerHTML = d.status ? `<span class="dot${d.status.live ? " live" : ""}"></span>` : icon(d.ic);
        }
        const x = tabEl.querySelector(".tab-x");
        x.hidden = !many;
        x.setAttribute("aria-label", `Close ${d.title}`);
        x.title = `Close tab${on ? shortcut("Cmd+W") : ""}`;
      });
    if (inSight !== active) {
      inSight = active;
      list.querySelector(".tab.on")?.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }

  return {
    el,
    revive,
    split: () => cur().split,
    // A project's color: its group's while it has one in the strip.
    colorOf: (ws) => colors.get(ws) || chosenColor(ws) || ownColor(ws),
    saveSplit(split) {
      const t = cur();
      const was = groupOf(t);
      t.split = split;
      if (groupOf(t) !== was) tabs = regroup(tabs, t, groupOf);
      save();
    },
    // Every repaint of the shell; `route` is the page on screen, which is
    // this tab's. A tab now on another project's page glides to its group.
    update(route) {
      const t = cur();
      let positions = null;
      if (route && t.path !== route.path) {
        const was = groupOf(t);
        t.path = route.path;
        if (groupOf(t) !== was) {
          positions = tabPositions();
          tabs = regroup(tabs, t, groupOf);
          inSight = null;
        }
        save();
      }
      paint();
      if (positions) settle(positions);
    },
    destroy() {
      endDrag(false, false);
      life.abort();
      for (const t of tabs) drop(t);
    },
  };
}
