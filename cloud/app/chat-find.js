// Cmd+F on a chat (Ctrl+F off a Mac): find in this conversation, not the
// sidebar or the panels beside it. A bar at the top of the chat, above the
// conversation rather than over it: type to find, "3 of 12", Enter or the up
// button for the next older match,
// Shift+Enter or the down button for the next newer one (Cmd+G and
// Shift+Cmd+G too), Esc to close and give the keyboard back. Elsewhere Cmd+F
// is the browser's (or the Mac app's) own find.
//
// Matches count from the newest, at the bottom, where a chat is read from,
// so the count holds while earlier pages load in above. It covers the whole
// conversation: past the oldest match loaded, the next one loads the page
// before (the chat's own Load earlier messages) until a page has one or the
// conversation starts. "12+" says pages before are still to load.
//
// Tool cards are searched too: their input, and their output once typing
// pauses, when the chat opens the cards whose output has the words
// (`openTools`). The ones it opened close again when the words change or the
// find closes, except the card of the match you were on.
//
// The words found are painted with the CSS Custom Highlight API, never by
// changing the chat's DOM, which its paint owns. The highlights are the
// page's, not a pane's, so one find is open at a time.
//
// Search everything (global-search.js) opens a chat at a message with
// `reveal`, loading earlier pages until it is shown. That find is the
// search's: scrolling the chat closes it. Sending a message closes any find.
import { h, icon, setText } from "/app/render.js";
import { coarse, postNative, toast, track } from "/app/core.js";

const MAC = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
const SUPPORTED = typeof CSS !== "undefined" && !!CSS.highlights && typeof Highlight === "function";
const ALL = "chat-find";
const CURRENT = "chat-find-current";
// The text searched: the user's and the agents' words, a reply's quote, a
// question the agent asked and its answer, and tool cards' input and output.
const BOXES = [
  ":scope > .msg .md", ":scope > .msg .qr-text", ":scope > .msg .asked-q", ":scope > .msg .asked-a",
  ":scope > .tool .t-input", ":scope > .tool .tool-out pre",
  ":scope > .steps .tool .t-input", ":scope > .steps .tool .tool-out pre",
].join(", ");
// Inside them, a code block's language and Copy button are not the message.
const SKIP = ".cb-head, button";
// Room above a match, and below for the composer's shadow.
const ROOM_TOP = 24;
const ROOM_BOTTOM = 24;
// A query typed with no match loaded looks further back once typing pauses,
// and the tool cards with the words open then.
const LOOK_BACK_MS = 400;

const mod = (e) => (MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey);
const isKey = (e, key) => !e.altKey && mod(e) && e.key?.toLowerCase() === key;
const escapeRe = (s) => s.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const frame = () => new Promise((r) => requestAnimationFrame(() => r()));

// The open find, of the chats on screen.
let active = null;

/**
 * Cmd+F and Cmd+G for the chat on screen (`current()`, the focused pane's
 * find or null), and `window.rudderFindInChat(open)` for the Mac app's Edit >
 * Find (mac/App/AppModel.swift showFind): whether a chat is on screen, its
 * find opened when `open`. After the page's own keys: one that already took
 * Cmd+F (the Files editor's search) keeps it.
 */
export function mountChatFindKey(current) {
  window.rudderFindInChat = (open = true) => {
    const find = current();
    if (!find) return false;
    if (open) find.open();
    return true;
  };
  document.addEventListener("keydown", (e) => {
    if (e.defaultPrevented) return;
    if (isKey(e, "f") && !e.shiftKey) {
      const find = current();
      if (!find || document.querySelector("dialog[open]") || document.activeElement?.closest(".tp")) return;
      e.preventDefault();
      find.open();
    } else if (isKey(e, "g") && active) {
      e.preventDefault();
      if (e.shiftKey) active.newer();
      else void active.older();
    }
  });
}

/**
 * The find of one chat (AgentScreen). `wrap` holds the bar, `scroller` and
 * `msgsEl` are the chat's; `earlier()` is { cursor, loading } of the page
 * before the oldest shown, `loadEarlier()` loads it, `loaded()` says the
 * first page came, and `settle()` tells the chat its view moved.
 * `openTools(query, keep)` opens the tool cards whose input or output has
 * `query` and closes the ones it opened before that do not, all of them for
 * "", except the card holding the node `keep`. Null where the browser cannot
 * paint highlights: Cmd+F stays the browser's.
 */
export function chatFind({ wrap, scroller, msgsEl, earlier, loadEarlier, loaded, settle, openTools }) {
  if (!SUPPORTED) return null;
  const bar = h(`<div class="cf" role="search" hidden>
    <label class="cf-in">${icon("search")}<input type="text" autocomplete="off" autocapitalize="off" spellcheck="false" enterkeyhint="search" placeholder="Find in chat" aria-label="Find in chat"><span class="cf-n" role="status"></span></label>
    <button type="button" class="cf-b" data-step="older" aria-label="Older match" title="Older match (Enter)">${icon("up")}</button>
    <button type="button" class="cf-b" data-step="newer" aria-label="Newer match" title="Newer match (Shift+Enter)">${icon("down")}</button>
    <button type="button" class="cf-b" data-act="close" aria-label="Close find" title="Close (Esc)">${icon("x")}</button>
  </div>`);
  wrap.prepend(bar);
  const input = bar.querySelector("input");
  const countEl = bar.querySelector(".cf-n");
  const olderBtn = bar.querySelector('[data-step="older"]');
  const newerBtn = bar.querySelector('[data-step="newer"]');

  let query = "";
  let matches = []; // { range, box, nth }, oldest first
  let current = -1;
  let searching = false;
  // Each query, step or reveal: a page load for an older one stops.
  let run = 0;
  let lookBack = 0;
  let returnTo = null;
  let destroyed = false;
  let queued = false;
  // The query the tool cards were opened for, and the wait for typing to pause.
  let deep = "";
  let deepTimer = 0;
  // Opened by Search everything: the user's next scroll closes it.
  let transient = false;

  const isOpen = () => !bar.hidden;

  // The chat repaints as it polls and as pages load: the matches follow,
  // the current one kept, the view left where it is.
  const observer = new MutationObserver(() => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      if (!isOpen()) return;
      // Pages loaded above hold tool cards too.
      if (deep && deep === query) openTools(deep, null);
      recompute();
      paint();
    });
  });

  // Once typing pauses: the tool cards with the words open, and are searched.
  function dig() {
    clearTimeout(deepTimer);
    deepTimer = setTimeout(() => {
      if (!isOpen() || !query) return;
      deep = query;
      openTools(query, null);
      recompute();
      show();
    }, LOOK_BACK_MS);
  }

  function collect() {
    const found = [];
    if (!query) return found;
    const re = new RegExp(escapeRe(query), "giu");
    for (const box of msgsEl.querySelectorAll(BOXES)) {
      if (box.closest("[hidden]")) continue;
      const nodes = [];
      let text = "";
      const walker = document.createTreeWalker(box, NodeFilter.SHOW_TEXT, {
        acceptNode: (n) => (n.parentElement.closest(SKIP) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
      });
      for (let n = walker.nextNode(); n; n = walker.nextNode()) {
        nodes.push({ n, at: text.length });
        text += n.data;
      }
      let nth = 0;
      for (const m of text.matchAll(re)) {
        const range = document.createRange();
        const start = locate(nodes, m.index, false);
        const end = locate(nodes, m.index + m[0].length, true);
        range.setStart(start.n, m.index - start.at);
        range.setEnd(end.n, m.index + m[0].length - end.at);
        found.push({ range, box, nth: nth++ });
      }
    }
    return found;
  }

  // The text node an offset falls in: a match's start in the node it begins,
  // its end in the node it ends.
  function locate(nodes, offset, end) {
    let i = nodes.length - 1;
    while (i > 0 && (end ? nodes[i].at >= offset : nodes[i].at > offset)) i -= 1;
    return nodes[i];
  }

  // Recounted, keeping the current match when it is still there, else the
  // nearest one above `anchor` (a screen y), the bottom of the view by default.
  function recompute(anchor = null) {
    const was = matches[current];
    const line = anchor ?? (was ? was.range.getBoundingClientRect().top : null);
    matches = collect();
    current = was ? matches.findIndex((m) => m.box === was.box && m.nth === was.nth) : -1;
    if (current < 0) current = nearest(line ?? scroller.getBoundingClientRect().bottom);
  }

  function nearest(line) {
    for (let i = matches.length - 1; i >= 0; i -= 1) {
      if (matches[i].range.getBoundingClientRect().top <= line + 1) return i;
    }
    return matches.length ? 0 : -1;
  }

  function paint() {
    CSS.highlights.set(ALL, new Highlight(...matches.map((m) => m.range)));
    const cur = matches[current];
    if (cur) {
      const hl = new Highlight(cur.range);
      hl.priority = 1;
      CSS.highlights.set(CURRENT, hl);
    } else CSS.highlights.delete(CURRENT);
    const more = !!earlier().cursor;
    let text = "";
    if (query && cur) text = `${(matches.length - current).toLocaleString()} of ${matches.length.toLocaleString()}${more ? "+" : ""}`;
    else if (query && searching) text = "Searching";
    else if (query && !more) text = "No matches";
    setText(countEl, text);
    olderBtn.disabled = !query || (!matches.length && !more) || searching;
    newerBtn.disabled = !matches.length;
  }

  // The current match in view, under the bar.
  function show() {
    paint();
    const cur = matches[current];
    if (!cur) return;
    const r = cur.range.getBoundingClientRect();
    const box = scroller.getBoundingClientRect();
    if (r.top >= box.top + ROOM_TOP && r.bottom <= box.bottom - ROOM_BOTTOM) return;
    scroller.scrollTop += r.top - (box.top + Math.max(ROOM_TOP, box.height / 3));
    settle();
  }

  // Pages before the oldest shown, until `found()` or the conversation's start.
  async function loadUntil(found, mine) {
    searching = true;
    paint();
    try {
      for (;;) {
        recompute();
        if (found()) return true;
        if (destroyed || mine !== run || !isOpen()) return false;
        const { cursor, loading } = earlier();
        if (!cursor) return false;
        // The chat's own load (Load earlier messages, Alt+V): wait for it.
        if (loading) { await sleep(100); continue; }
        await loadEarlier();
        // It did not load (the chat said why, or its conversation changed).
        if (earlier().cursor === cursor) return false;
      }
    } finally {
      if (mine === run) {
        searching = false;
        if (isOpen()) paint();
      }
    }
  }

  async function older() {
    if (!isOpen() || !query) return;
    transient = false;
    const mine = ++run;
    clearTimeout(lookBack);
    if (current > 0) { current -= 1; show(); return; }
    const oldest = matches[0] || null;
    const more = await loadUntil(() => (oldest ? matches[0] !== undefined && current > 0 : matches.length > 0), mine);
    if (mine !== run) return;
    if (more) current = oldest ? current - 1 : matches.length - 1;
    // The conversation's start: round to the newest.
    else if (matches.length) current = matches.length - 1;
    show();
  }

  function newer() {
    if (!isOpen() || !matches.length) return;
    transient = false;
    ++run;
    clearTimeout(lookBack);
    current = current < matches.length - 1 ? current + 1 : 0;
    show();
  }

  function typed() {
    const was = matches[current];
    const line = was ? was.range.getBoundingClientRect().top : null;
    query = input.value;
    const mine = ++run;
    clearTimeout(deepTimer);
    if (!query && deep) { deep = ""; openTools("", null); }
    matches = [];
    current = -1;
    recompute(line);
    show();
    clearTimeout(lookBack);
    if (query) dig();
    if (!query || matches.length || !earlier().cursor) return;
    lookBack = setTimeout(async () => {
      if (await loadUntil(() => matches.length > 0, mine) && mine === run) {
        current = matches.length - 1;
        show();
      }
    }, LOOK_BACK_MS);
  }

  function open() {
    if (destroyed) return;
    if (active && active !== api) active.close({ restore: false });
    active = api;
    if (!isOpen()) {
      const at = document.activeElement;
      returnTo = at && at !== document.body && !bar.contains(at) ? at : null;
      // The conversation stays where it is on screen: its top slides under the bar's row.
      const top = scroller.getBoundingClientRect().top;
      bar.hidden = false;
      scroller.scrollTop += scroller.getBoundingClientRect().top - top;
      settle();
      observer.observe(msgsEl, { childList: true, subtree: true, characterData: true });
      if (input.value) typed();
      else paint();
      // The Mac app closes its own find bar: one find at a time.
      postNative({ type: "findInChat" });
      track("feature_used", { feature: "chat_find" });
    }
    input.focus({ preventScroll: true });
    input.select();
  }

  function close({ restore = true } = {}) {
    if (!isOpen()) return;
    const top = scroller.getBoundingClientRect().top;
    bar.hidden = true;
    scroller.scrollTop += scroller.getBoundingClientRect().top - top;
    settle();
    ++run;
    clearTimeout(lookBack);
    clearTimeout(deepTimer);
    transient = false;
    observer.disconnect();
    const keep = matches[current]?.box ?? null;
    if (deep) { deep = ""; openTools("", keep); }
    matches = [];
    current = -1;
    searching = false;
    CSS.highlights.delete(ALL);
    CSS.highlights.delete(CURRENT);
    if (active === api) active = null;
    const back = returnTo;
    returnTo = null;
    if (restore && back?.isConnected) back.focus({ preventScroll: true });
    else if (bar.contains(document.activeElement)) input.blur();
  }

  // Search everything's message: this chat opened at it, the words found.
  async function reveal({ messageId, query: words }) {
    input.value = words;
    open();
    transient = true;
    // Opened from Search everything, which is gone: Esc leaves the page the keyboard.
    returnTo = null;
    query = words;
    const mine = ++run;
    clearTimeout(lookBack);
    while (!loaded()) {
      await sleep(50);
      if (destroyed || mine !== run) return;
    }
    // The first paint holds the chat at its bottom for a frame.
    await frame();
    await frame();
    if (destroyed || mine !== run) return;
    const node = () => msgsEl.querySelector(`:scope > [data-key="${CSS.escape(String(messageId))}"]`);
    if (!await loadUntil(() => !!node(), mine)) {
      if (mine === run && isOpen()) {
        show();
        toast("That message is not in this chat anymore", { error: true });
      }
      return;
    }
    const msg = node();
    const at = matches.findIndex((m) => msg.contains(m.box));
    if (at >= 0) current = at;
    show();
    if (at < 0) {
      msg.scrollIntoView({ block: "center" });
      settle();
    }
    msg.classList.remove("cf-flash");
    void msg.offsetWidth;
    msg.classList.add("cf-flash");
    msg.addEventListener("animationend", () => msg.classList.remove("cf-flash"), { once: true });
  }

  input.addEventListener("input", () => { transient = false; typed(); });
  // Scrolling away from what Search everything opened ends its find.
  const scrolled = () => { if (transient) close({ restore: false }); };
  scroller.addEventListener("wheel", scrolled, { passive: true });
  scroller.addEventListener("touchmove", scrolled, { passive: true });
  input.addEventListener("keydown", (e) => {
    if (e.isComposing) return;
    if (e.key === "Enter") {
      e.preventDefault();
      if (e.shiftKey) newer();
      else void older();
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      close();
    }
  });
  // Buttons keep the field focused, so Enter goes on from where they left.
  bar.addEventListener("mousedown", (e) => { if (e.target.closest("button")) e.preventDefault(); });
  bar.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.act === "close") close();
    else if (b.dataset.step === "older") void older();
    else newer();
  });
  if (coarse) input.setAttribute("inputmode", "search");

  const api = {
    open,
    close,
    older,
    newer,
    reveal,
    destroy() {
      close({ restore: false });
      destroyed = true;
      bar.remove();
    },
  };
  return api;
}
