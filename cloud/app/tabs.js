// Tabs in the desktop shell: several pages open at once, each in its own tab,
// like a browser. A tab is a page (an agent's chat, a project, the home
// screen), plus two agents side by side when it has a split. The tab on screen
// is the URL: moving around in it (the sidebar, links, Back) changes that tab.
//
// A tab you leave keeps its screens mounted, out of sight at the size they
// had, so its chat, scroll, draft and focus are where you left them, and its
// chat keeps following the agent. Tabs belong to this device and come back
// after a reload. Phones have no shell, and no tabs.
//
// The + button opens a tab. In the Mac app the bar appears with the second
// tab, and Cmd+T opens a tab even with the bar hidden. Cmd+W closes it (the
// last one leaves Cmd+W to the window). Control+Tab and Control+Shift+Tab
// step through tabs, as do Cmd+Shift+[ and Cmd+Shift+]. Cmd+1 to Cmd+8 jump
// to one and Cmd+9 to the last. Cmd+click on a link opens it in a tab behind
// this one. A browser keeps those keys for its own tabs.
// Drag a tab to reorder it.
import { h, icon, setText, reconcile } from "/app/render.js";
import { ls, PLATFORM } from "/app/core.js";
import { tabDragPosition } from "/app/tab-drag.js";
import { teamAgent } from "/app/team.js";

const KEY = "rudder.tabs"; // { tabs: [{ id, path, split }], active }
const MAC = PLATFORM === "mac";
const newId = () => Math.random().toString(36).slice(2, 10);
const TAB_MOVE = { id: "tab-reorder", duration: 190, easing: "cubic-bezier(.2,.75,.2,1)" };

/**
 * `main`: the shell's main area, whose panes hold the screens. The shell's
 * hooks: `park()` takes the screens off its panes and returns them, per pane
 * ([{ route, screen } | null]); `loadSplit(split)` gives it the split of the
 * tab coming in. `navigate`, `parseRoute`, `projectPath`, `findAgent`,
 * `findProject` and `agentStatus` are the app's own.
 */
export function Tabs({ main, park, loadSplit, navigate, parseRoute, projectPath, findAgent, findProject, agentStatus }) {
  const saved = ls.get(KEY);
  // `kept`: the screens of a tab that is not on screen, per pane, in memory only.
  let tabs = saved ? saved.tabs.map((t) => ({ ...t, kept: null, focus: null })) : [{ id: newId(), path: location.pathname, split: null, kept: null, focus: null }];
  let active = saved ? saved.active : tabs[0].id;
  const cur = () => tabs.find((t) => t.id === active);
  const save = () => ls.set(KEY, { tabs: tabs.map(({ id, path, split }) => ({ id, path, split })), active });

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

  // A new tab opens last, where you start an agent: the project on screen
  // (its task box takes the typing), or the home screen. A link's tab (`path`)
  // opens next to the tab it came from, as in a browser.
  function open(path = null, { behind = false } = {}) {
    endDrag(false, false);
    const r = parseRoute(location.pathname);
    const fresh = !path;
    const t = { id: newId(), path: path || (r.name === "agent" || r.name === "project" ? projectPath(r.ws) : "/app"), split: null, kept: null, focus: null };
    tabs.splice(fresh ? tabs.length : tabs.findIndex((x) => x.id === active) + 1, 0, t);
    save();
    if (behind) { paint(); return; }
    select(t.id);
    if (fresh) main.querySelector(".dsk-pane > .screen:not(.tab-off) .composer textarea")?.focus();
  }
  // The next tab to the right takes its place, as in a browser.
  function close(id) {
    if (tabs.length < 2) return;
    endDrag(false, false);
    const i = tabs.findIndex((t) => t.id === id);
    const t = tabs[i];
    if (id === active) select((tabs[i + 1] || tabs[i - 1]).id);
    drop(t);
    tabs = tabs.filter((x) => x !== t);
    save();
    paint();
  }
  const step = (dir) => {
    const i = tabs.findIndex((t) => t.id === active);
    select(tabs[(i + dir + tabs.length) % tabs.length].id);
  };

  function reorder(from, to) {
    if (from === to) return;
    const focused = document.activeElement;
    const [tab] = tabs.splice(from, 1);
    tabs.splice(to, 0, tab);
    save();
    paint();
    if (list.contains(focused) && focused !== document.activeElement) focused.focus({ preventScroll: true });
    announcement.textContent = `${describe(tab).title}, tab ${to + 1} of ${tabs.length}`;
  }

  function tabPositions() {
    return [...list.children].map((tab) => [tab, tab.getBoundingClientRect().left]);
  }

  function settle(positions) {
    if (reducedMotion.matches) return;
    const offsets = positions.map(([tab, left]) => [tab, left - tab.getBoundingClientRect().left]);
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
    if (commit) reorder(d.from, d.to);
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
      cancelSettle();
      const origin = list.getBoundingClientRect().left - list.scrollLeft;
      d.items = [...list.children].map((tab) => {
        const rect = tab.getBoundingClientRect();
        return { tab, left: rect.left - origin, center: rect.left - origin + rect.width / 2 };
      });
      d.from = d.to = d.items.findIndex(({ tab }) => tab === d.tab);
      d.width = d.tab.getBoundingClientRect().width;
      d.right = list.lastElementChild.getBoundingClientRect().right - origin;
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
    const tab = e.target.closest(".tab");
    if (!tab) return;
    if (e.target.closest(".tab-x")) close(tab.dataset.key);
    else select(tab.dataset.key);
  });
  // The middle button closes a tab.
  list.addEventListener("auxclick", (e) => {
    const tab = e.button === 1 ? e.target.closest(".tab") : null;
    if (!tab) return;
    e.preventDefault();
    close(tab.dataset.key);
  });
  // Left and Right move between tabs, Enter or Space opens one.
  list.addEventListener("keydown", (e) => {
    if (e.shiftKey || (e.key !== "ArrowLeft" && e.key !== "ArrowRight")) return;
    const btns = [...list.querySelectorAll(".tab-b")];
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
      if (!e.metaKey || e.ctrlKey || e.altKey || document.querySelector("dialog[open]")) return;
      const key = e.key.toLowerCase();
      let act = null;
      if (!e.shiftKey && key === "t") act = () => open();
      else if (!e.shiftKey && key === "w" && tabs.length > 1) act = () => close(active);
      else if (e.shiftKey && (e.code === "BracketLeft" || e.code === "BracketRight")) act = () => step(e.code === "BracketRight" ? 1 : -1);
      else if (!e.shiftKey && /^Digit[1-9]$/.test(e.code)) {
        const n = Number(e.code.slice(5));
        act = () => select((n === 9 ? tabs.at(-1) : tabs[Math.min(n, tabs.length) - 1]).id);
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
  function paint() {
    const many = tabs.length > 1;
    el.hidden = MAC && !many;
    list.toggleAttribute("data-reorderable", many);
    reconcile(list, tabs, (t) => t.id,
      () => h(`<div class="tab" role="presentation"><button type="button" class="tab-b" role="tab"><span class="tab-ic"></span><span class="tab-t"></span></button><button type="button" class="tab-x" tabindex="-1">${icon("x")}</button></div>`),
      (tabEl, t) => {
        const d = describe(t);
        const on = t.id === active;
        tabEl.className = `tab${on ? " on" : ""}${d.status ? ` ${d.status.cls}` : ""}`;
        const b = tabEl.querySelector(".tab-b");
        b.setAttribute("aria-selected", String(on));
        b.tabIndex = on ? 0 : -1;
        b.title = d.tip || (d.status ? `${d.title} · ${d.status.label}` : d.title);
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
  }

  return {
    el,
    revive,
    split: () => cur().split,
    saveSplit(split) { cur().split = split; save(); },
    // Every repaint of the shell; `route` is the page on screen, which is this tab's.
    update(route) {
      const t = cur();
      if (route && t.path !== route.path) { t.path = route.path; save(); }
      paint();
    },
    destroy() {
      endDrag(false, false);
      life.abort();
      for (const t of tabs) drop(t);
    },
  };
}
