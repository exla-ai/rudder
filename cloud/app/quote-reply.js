// Highlight to reply. Select text in a chat message (an agent's reply or one
// of yours) and a Reply button shows: just above the selection on a desktop,
// and on a phone at the foot of the chat once the selection settles, clear of
// the phone's own selection menu and handles. Reply (a click, or Enter while
// the button shows) puts the text in a "Replying to" strip above that chat's
// composer, so in two panes it goes to the pane it came from. What you send
// then is the quote as Markdown and your prompt (quote-text.js), through the
// chat's usual send. Esc with an empty box, or the strip's x, cancels. One
// quote at a time: Reply again replaces it.
//
// Only a message's own text quotes: tool cards, steps, the composer and the
// rest of the page show no button. A selection that reaches past a message
// keeps the one message it holds text in, and shows nothing when it holds
// text in two.
//
// Sent, the quote shows above your message in the strip's style (paintQuote).
import { h, icon } from "/app/render.js";
import { coarse } from "/app/core.js";
import { cleanQuote, parseQuoted, quoteMessage } from "/app/quote-text.js";

// A message's text in a chat's list (app.js AgentScreen): not tool cards, not
// subagent cards, not your message while it is open for editing.
const BODY = ":scope > .msg.assistant > .md, :scope > .msg.user:not(.editing) > .bubble";

const chats = new Set(); // { root, scroller, reply(text, from) }
let pill = null;
let shown = null;        // { chat, text, from, range }
let pressing = false;    // a press on the button: the selection may go first
let mouseDown = false;   // a mouse drag still selecting
let settle = 0;
let dismissed = null;    // the selection Esc hid the button for
let frame = 0;
let listening = false;

function listen() {
  if (listening) return;
  listening = true;
  document.addEventListener("selectionchange", schedule);
  document.addEventListener("pointerdown", (e) => {
    if (pill?.contains(e.target)) return;
    if (e.pointerType === "mouse") mouseDown = true;
  }, true);
  const up = () => { if (mouseDown) { mouseDown = false; schedule(); } };
  document.addEventListener("pointerup", up, true);
  document.addEventListener("pointercancel", up, true);
  document.addEventListener("keydown", onKey);
  // The chat scrolls under the button: it follows its text.
  document.addEventListener("scroll", reposition, { capture: true, passive: true });
  addEventListener("resize", reposition);
}

function schedule() {
  clearTimeout(settle);
  if (pressing) return;
  const sel = getSelection();
  if (!sel || sel.isCollapsed) { dismissed = null; hide(); return; }
  if (mouseDown) return;
  // A phone's handles move the selection in many small steps.
  settle = setTimeout(check, coarse ? 450 : 80);
}

function check() {
  const found = selected();
  if (!found || sameSelection(dismissed)) { hide(); return; }
  dismissed = null;
  show(found);
}

const selectionKey = () => {
  const s = getSelection();
  return s && { a: s.anchorNode, ao: s.anchorOffset, f: s.focusNode, fo: s.focusOffset };
};
function sameSelection(k) {
  const now = selectionKey();
  return !!k && !!now && k.a === now.a && k.ao === now.ao && k.f === now.f && k.fo === now.fo;
}

// The one message of a registered chat the selection holds text in.
function selected() {
  const sel = getSelection();
  if (!sel || sel.isCollapsed || !sel.rangeCount) return null;
  const range = sel.getRangeAt(0);
  for (const chat of chats) {
    if (!chat.root.isConnected || !range.intersectsNode(chat.root)) continue;
    let hit = null;
    for (const body of chat.root.querySelectorAll(BODY)) {
      if (!range.intersectsNode(body)) continue;
      const part = clampTo(range, body);
      const text = cleanQuote(textOf(part));
      if (!text) continue;
      if (hit) return null;
      hit = { chat, text, range: part, from: body.closest(".msg").classList.contains("user") ? "you" : "agent" };
    }
    if (hit) return hit;
  }
  return null;
}

function clampTo(range, node) {
  const r = document.createRange();
  r.selectNodeContents(node);
  if (range.compareBoundaryPoints(Range.START_TO_START, r) > 0) r.setStart(range.startContainer, range.startOffset);
  if (range.compareBoundaryPoints(Range.END_TO_END, r) < 0) r.setEnd(range.endContainer, range.endOffset);
  return r;
}

// The text as it reads on screen: paragraphs and lines kept, a code block's
// language and Copy button left out.
function textOf(range) {
  const inside = range.commonAncestorContainer;
  const el = inside.nodeType === 1 ? inside : inside.parentElement;
  if (el?.closest("pre")) return range.toString();
  const box = document.createElement("div");
  box.className = "qr-measure";
  box.setAttribute("aria-hidden", "true");
  box.append(range.cloneContents());
  for (const n of box.querySelectorAll(".cb-head")) n.remove();
  document.body.append(box);
  try { return box.innerText; } finally { box.remove(); }
}

function ensurePill() {
  if (pill) return pill;
  pill = h(`<button type="button" class="qr-pill" hidden>${icon("reply")}<span>Reply</span></button>`);
  pill.title = coarse ? "Reply with this quote" : "Reply with this quote (Enter)";
  pill.setAttribute("aria-label", "Reply with the selected text");
  // Pressing it must not drop the selection before the click lands.
  pill.addEventListener("pointerdown", (e) => { pressing = true; if (e.pointerType === "mouse") e.preventDefault(); });
  pill.addEventListener("mousedown", (e) => e.preventDefault());
  const release = () => setTimeout(() => { if (pressing) { pressing = false; schedule(); } }, 0);
  pill.addEventListener("pointerup", release);
  pill.addEventListener("pointercancel", release);
  pill.addEventListener("click", activate);
  document.body.append(pill);
  return pill;
}

function show(found) {
  shown = found;
  ensurePill().hidden = false;
  pill.classList.toggle("docked", coarse);
  position();
}

function hide() {
  clearTimeout(settle);
  shown = null;
  if (pill) { pill.hidden = true; pill.classList.remove("show"); }
}

function activate() {
  pressing = false;
  if (!shown) return;
  const { chat, text, from } = shown;
  hide();
  getSelection()?.removeAllRanges();
  chat.reply(text, from);
}

function onKey(e) {
  if (!shown || !pill.classList.contains("show") || e.defaultPrevented || e.isComposing) return;
  if (e.key === "Escape") {
    e.preventDefault();
    dismissed = selectionKey();
    hide();
    return;
  }
  // Enter takes the reply, unless something else would take that Enter.
  if (e.key === "Enter" && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey
      && !e.target.closest?.("input, textarea, select, button, a, [contenteditable], [role=button], [role=menuitem]")) {
    e.preventDefault();
    activate();
  }
}

function reposition() {
  if (!shown || frame) return;
  frame = requestAnimationFrame(() => { frame = 0; if (shown) position(); });
}

// Desktop: above the selection's first line, or under its last when there is
// no room above, kept inside the chat. Phone: centered at the chat's foot.
function position() {
  const { chat, range } = shown;
  const area = chat.scroller.getBoundingClientRect();
  const pw = pill.offsetWidth;
  const ph = pill.offsetHeight;
  const box = range.getBoundingClientRect();
  const inView = box.bottom > area.top && box.top < area.bottom && area.height > ph + 12;
  pill.classList.toggle("show", inView);
  if (!inView) return;
  let x, y;
  if (coarse) {
    // Above the chat's "New messages" button when that shows.
    const lift = chat.scroller.parentElement?.querySelector(".new-pill.show") ? 46 : 0;
    x = area.left + area.width / 2 - pw / 2;
    y = area.bottom - ph - 14 - lift;
  } else {
    const rects = [...range.getClientRects()].filter((r) => r.width > 0 && r.height > 0);
    const first = rects[0] || box;
    const last = rects[rects.length - 1] || box;
    const gap = 8;
    let at = first;
    y = first.top - ph - gap;
    if (y < area.top + 6) { at = last; y = last.bottom + gap; }
    if (y + ph > area.bottom - 6) y = Math.max(area.top + 6, Math.min(y, area.bottom - ph - 6));
    x = (at.left + at.right) / 2 - pw / 2;
  }
  x = Math.max(area.left + 8, Math.min(x, area.right - pw - 8));
  pill.style.transform = `translate(${Math.round(x)}px, ${Math.round(y)}px)`;
}

/**
 * Reply mode for one chat. `chat` is its list of messages, `scroller` what
 * scrolls it, `composer` its createComposer() result. Returns
 * `{ wrap(text), reply(text, from), destroy() }`: `wrap` makes what is sent
 * (the quoted message while replying, which ends reply mode), `reply` starts
 * replying to `text` ("agent" or "you" wrote it).
 */
export function mountQuoteReply({ chat, scroller, composer }) {
  listen();
  const ta = composer.textarea;
  const strip = h(`<div class="qr-strip" role="group" hidden>
      <div class="qr-main"><div class="qr-label">${icon("reply")}<span></span></div><div class="qr-text"></div>
        <button type="button" class="qr-more" hidden aria-expanded="false">Show more</button></div>
      <button type="button" class="qr-x" aria-label="Cancel the reply" title="Cancel the reply">${icon("x")}</button>
    </div>`);
  const label = strip.querySelector(".qr-label span");
  const textEl = strip.querySelector(".qr-text");
  const more = strip.querySelector(".qr-more");
  composer.el.querySelector("form").before(strip);
  let quote = null; // { text, from }

  // "Show more" only when the quote is clipped.
  const fit = () => {
    if (!quote) return;
    const open = strip.classList.contains("open");
    more.hidden = !open && textEl.scrollHeight <= textEl.clientHeight + 1;
  };
  new ResizeObserver(fit).observe(textEl);
  more.addEventListener("click", () => {
    const open = !strip.classList.contains("open");
    strip.classList.toggle("open", open);
    more.textContent = open ? "Show less" : "Show more";
    more.setAttribute("aria-expanded", String(open));
    fit();
  });

  function reply(text, from) {
    const q = cleanQuote(text);
    if (!q) return;
    quote = { text: q, from };
    const who = from === "you" ? "Replying to your message" : "Replying to the agent";
    label.textContent = who;
    strip.setAttribute("aria-label", who);
    textEl.textContent = q;
    strip.classList.remove("open");
    more.textContent = "Show more";
    more.setAttribute("aria-expanded", "false");
    strip.hidden = false;
    fit();
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }
  function cancel() {
    quote = null;
    strip.hidden = true;
    textEl.textContent = "";
  }
  strip.querySelector(".qr-x").addEventListener("click", () => { cancel(); ta.focus(); });
  ta.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !quote || ta.value || e.defaultPrevented || e.isComposing) return;
    e.preventDefault();
    e.stopPropagation();
    cancel();
  });

  const entry = { root: chat, scroller, reply };
  chats.add(entry);
  return {
    wrap(text) {
      if (!quote) return text;
      const out = quoteMessage(quote.text, text);
      cancel();
      return out;
    },
    reply,
    destroy() {
      chats.delete(entry);
      if (shown?.chat === entry) hide();
    },
  };
}

/**
 * A message you sent, as the chat shows it: a quote it starts with goes in a
 * strip above the bubble (`node`'s), and the rest is returned for the bubble.
 * A long quote shows its first lines; a click opens it.
 */
export function paintQuote(node, text) {
  const { quote, body } = parseQuoted(text);
  let box = node.querySelector(":scope > .qr-quote");
  if (!quote) { box?.remove(); return text; }
  if (!box) {
    box = h(`<div class="qr-quote"><div class="qr-text"></div></div>`);
    box.addEventListener("click", () => {
      if (!box.classList.contains("long") || !getSelection()?.isCollapsed) return;
      box.classList.toggle("open");
    });
    node.querySelector(":scope > .bubble").before(box);
  }
  const t = box.querySelector(".qr-text");
  if (t.textContent !== quote) {
    t.textContent = quote;
    box.classList.remove("open");
    requestAnimationFrame(() => box.classList.toggle("long", t.scrollHeight > t.clientHeight + 1));
  }
  return body;
}
