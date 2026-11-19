// How much context the agent has left and where its backend compacts it, as
// one line above the composer: "62% context left · compacts at 160K". The
// numbers are the transcript's `context` (cloud/worker/context-usage.mjs);
// tokens used of the window and where the threshold comes from show on
// hover, or on tap. A value the home does not know is left out. While the
// agent compacts its context it says so, and when the context shrinks the bar
// eases down to the new size, tinted for a moment. When the agent can compact
// now, the details offer it.
import { h, setText } from "/app/render.js";
import { coarse } from "/app/core.js";
import { formatTokens } from "/app/agent-stats.js";
import { FOLD_ICON } from "/app/compaction.js";

const exact = (n) => n.toLocaleString();

// How close the context is to the nearer of compaction and the window.
const level = (fill) => (fill >= 0.9 ? "hot" : fill >= 0.7 ? "warm" : "ok");

// `left` and `at` are the readout's two halves; a narrow row drops the word
// "context" from the first (context-meter.css).
function view(c) {
  if (c.error) return { left: ["Context unavailable", "", ""], at: "", detail: [`The cloud machine could not read it: ${c.error}`], level: "ok", bar: null };
  if (c.used === null || c.used === undefined) return null;
  const detail = [];
  let left;
  if (c.window) {
    left = [`${Math.max(0, Math.round(100 - (c.used / c.window) * 100))}%`, " context", " left"];
    detail.push(`${exact(c.used)} of ${exact(c.window)} tokens used`);
  } else {
    left = [`${formatTokens(c.used)} used`, "", ""];
    detail.push(`${exact(c.used)} tokens used. This model's window is not known here.`);
  }
  if (c.autoCompactAt) detail.push(`Compacts at ${exact(c.autoCompactAt)} tokens`);
  if (c.source) detail.push(c.source);
  const limit = c.autoCompactAt || c.window;
  return {
    left,
    at: c.autoCompactAt ? `compacts at ${formatTokens(c.autoCompactAt)}` : "",
    detail,
    level: limit ? level(c.used / limit) : "ok",
    // The bar is the window, with a tick where compaction starts.
    bar: c.window
      ? { fill: Math.min(1, c.used / c.window), tick: c.autoCompactAt ? Math.min(1, c.autoCompactAt / c.window) : null }
      : c.autoCompactAt ? { fill: Math.min(1, c.used / c.autoCompactAt), tick: null } : null,
  };
}

// While a compaction runs (transcript `compacting`, compaction.js).
const COMPACTING = {
  left: ["Compacting", " context", ""],
  at: "",
  detail: ["Compacting the conversation into a summary so the agent can keep going."],
  level: "ok",
};
// How long the bar stays tinted after the context shrank, and how long the
// percentage takes to count up to its new value.
const DROP_MS = 1800;
const COUNT_MS = 900;
const still = matchMedia("(prefers-reduced-motion: reduce)");

/**
 * Mount in the row above the composer (`slot`) and call
 * `update(t.context, { compacting, compactable })` after each transcript load.
 * Hidden while there is nothing to show. `compactable`: the details offer
 * Compact now, which calls `onCompact` (the chat sends /compact).
 */
export function ContextMeter(slot, { onCompact }) {
  const el = h(`<button type="button" class="cm" hidden aria-expanded="false"><span class="cm-bar" aria-hidden="true"><i></i><b></b></span><span class="cm-text"><span class="cm-n"></span><span class="cm-w"></span><span class="cm-l"></span><span class="cm-at"></span></span></button>`);
  slot.appendChild(el);
  const [numEl, wordEl, leftEl, atEl] = el.querySelectorAll(".cm-text > span");
  const barEl = el.querySelector(".cm-bar");
  let shown = null;
  let canCompact = false;
  let pop = null;
  let lastUsed = null;
  let dropTimer = 0;
  let counting = 0;

  const close = () => {
    if (!pop) return;
    pop.remove(); pop = null;
    el.setAttribute("aria-expanded", "false");
    document.removeEventListener("pointerdown", outside, true);
    document.removeEventListener("keydown", onKey, true);
  };
  const outside = (e) => { if (pop && !pop.contains(e.target) && !el.contains(e.target)) close(); };
  const onKey = (e) => { if (e.key === "Escape") { close(); el.focus(); } };
  const paintPop = () => {
    if (!pop || !shown) return;
    pop.querySelector(".cm-lines").replaceChildren(...shown.detail.map((line, i) => { const p = h(`<p class="${i ? "cm-sub" : "cm-main"}"></p>`); setText(p, line); return p; }));
    pop.querySelector(".cm-compact").hidden = !canCompact;
    const a = el.getBoundingClientRect();
    const vw = document.documentElement.clientWidth;
    pop.style.left = `${Math.round(Math.max(8, Math.min(vw - pop.offsetWidth - 8, a.right - pop.offsetWidth)))}px`;
    pop.style.top = `${Math.round(Math.max(8, a.top - pop.offsetHeight - 6))}px`;
  };
  el.addEventListener("click", (e) => {
    if (pop) return close();
    pop = h(`<div class="cm-pop" role="dialog" aria-label="Context"><div class="cm-lines"></div><button type="button" class="cm-compact" hidden><span class="cm-c-ic">${FOLD_ICON}</span><span class="cm-c-t"><b>Compact now</b><span>Summarizes the conversation so far to free context.</span></span></button></div>`);
    pop.querySelector(".cm-compact").addEventListener("click", () => { close(); onCompact(); });
    document.body.appendChild(pop);
    el.setAttribute("aria-expanded", "true");
    paintPop();
    // Opened from the keyboard: into the details, where the action is.
    if (e.detail === 0 && canCompact) pop.querySelector(".cm-compact").focus();
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", onKey, true);
  });

  function update(context, { compacting, compactable }) {
    const reading = context ? view(context) : null;
    canCompact = compactable && !compacting;
    shown = compacting ? { ...COMPACTING, bar: reading?.bar ?? null } : reading;
    el.hidden = !shown;
    if (!shown) { close(); return; }
    if (!counting) setText(numEl, shown.left[0]);
    setText(wordEl, shown.left[1]);
    setText(leftEl, shown.left[2]);
    setText(atEl, shown.at ? ` · ${shown.at}` : "");
    // The context shrank (a compaction, or a new conversation): the bar eases
    // down, the percentage left counts up to its new value, and both stay
    // tinted for a moment.
    const used = context && !context.error ? context.used : null;
    if (!compacting && Number.isFinite(used) && Number.isFinite(lastUsed) && used < lastUsed) {
      clearTimeout(dropTimer);
      dropTimer = setTimeout(() => { dropTimer = 0; el.classList.remove("cm-drop"); }, DROP_MS);
      if (context.window && !still.matches) countUp(left(lastUsed, context.window), left(used, context.window));
    }
    if (Number.isFinite(used)) lastUsed = used;
    el.className = `cm lv-${shown.level}${compacting ? " cm-busy" : ""}${dropTimer ? " cm-drop" : ""}`;
    barEl.hidden = !shown.bar;
    if (shown.bar) {
      barEl.querySelector("i").style.width = `${shown.bar.fill * 100}%`;
      const tick = barEl.querySelector("b");
      tick.hidden = shown.bar.tick === null;
      if (shown.bar.tick !== null) tick.style.left = `${shown.bar.tick * 100}%`;
    }
    el.setAttribute("aria-label", [shown.left.join("") + (shown.at ? `, ${shown.at}` : ""), ...shown.detail].join(". "));
    // A mouse gets the detail on hover; a tap opens it.
    if (!coarse) el.title = shown.detail.join("\n");
    paintPop();
  }

  const left = (used, window) => Math.max(0, Math.round(100 - (used / window) * 100));
  function countUp(from, to) {
    cancelAnimationFrame(counting);
    const start = performance.now();
    const step = (now) => {
      const k = Math.min(1, (now - start) / COUNT_MS);
      setText(numEl, `${Math.round(from + (to - from) * (1 - (1 - k) ** 3))}%`);
      counting = k < 1 ? requestAnimationFrame(step) : 0;
    };
    counting = requestAnimationFrame(step);
  }

  return { el, update, destroy() { clearTimeout(dropTimer); cancelAnimationFrame(counting); close(); } };
}
