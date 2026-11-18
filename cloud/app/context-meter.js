// How much context the agent has left and where its backend compacts it, as
// one line above the composer: "62% context left · compacts at 160K". The
// numbers are the transcript's `context` (cloud/worker/context-usage.mjs);
// tokens used of the window and where the threshold comes from show on
// hover, or on tap. A value the home does not know is left out.
import { h, setText } from "/app/render.js";
import { coarse } from "/app/core.js";
import { formatTokens } from "/app/agent-stats.js";

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

/**
 * Mount in the row above the composer (`slot`) and call `update(t.context)`
 * after each transcript load. Hidden while there is nothing to show.
 */
export function ContextMeter(slot) {
  const el = h(`<button type="button" class="cm" hidden aria-expanded="false"><span class="cm-bar" aria-hidden="true"><i></i><b></b></span><span class="cm-text"><span class="cm-n"></span><span class="cm-w"></span><span class="cm-l"></span><span class="cm-at"></span></span></button>`);
  slot.appendChild(el);
  const [numEl, wordEl, leftEl, atEl] = el.querySelectorAll(".cm-text > span");
  const barEl = el.querySelector(".cm-bar");
  let shown = null;
  let pop = null;

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
    pop.replaceChildren(...shown.detail.map((line, i) => { const p = h(`<p class="${i ? "cm-sub" : "cm-main"}"></p>`); setText(p, line); return p; }));
    const a = el.getBoundingClientRect();
    const vw = document.documentElement.clientWidth;
    pop.style.left = `${Math.round(Math.max(8, Math.min(vw - pop.offsetWidth - 8, a.right - pop.offsetWidth)))}px`;
    pop.style.top = `${Math.round(Math.max(8, a.top - pop.offsetHeight - 6))}px`;
  };
  el.addEventListener("click", () => {
    if (pop) return close();
    pop = h(`<div class="cm-pop" role="dialog" aria-label="Context"></div>`);
    document.body.appendChild(pop);
    el.setAttribute("aria-expanded", "true");
    paintPop();
    document.addEventListener("pointerdown", outside, true);
    document.addEventListener("keydown", onKey, true);
  });

  function update(context) {
    shown = context ? view(context) : null;
    el.hidden = !shown;
    if (!shown) { close(); return; }
    setText(numEl, shown.left[0]);
    setText(wordEl, shown.left[1]);
    setText(leftEl, shown.left[2]);
    setText(atEl, shown.at ? ` · ${shown.at}` : "");
    el.className = `cm lv-${shown.level}`;
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

  return { el, update, destroy: close };
}
