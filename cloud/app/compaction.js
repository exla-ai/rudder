// A compaction in the chat (compaction.css). When an agent's context fills
// up, its backend summarizes the conversation and carries on from the summary.
//  - The marker a finished one leaves (worker/transcripts.mjs compactMessage)
//    is a small card folded over the history it replaced: a bar the size the
//    context was, filled to the size it is now, what set it off and how long
//    it took. Opened, it shows the summary the agent continued from.
//  - While one runs, the same card times it, its bar full. The session file
//    gets nothing until it ends, so the home reads it off the agent's pane
//    (transcript `compacting`). When it ends, the marker takes its place and
//    the bar shrinks to what is left.
import { setText } from "/app/render.js";
import { formatTokens } from "/app/agent-stats.js";
import { duration } from "/app/subagents.js";

// Arrows from above and below into a line: the conversation folded into less.
export const FOLD_ICON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path class="c-up" d="M12 3v6M9 6l3 3 3-3"/><path d="M5 12h14"/><path class="c-down" d="M12 21v-6M9 18l3-3 3 3"/></svg>';
const CARET = '<svg class="t-caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9 18l6-6-6-6"/></svg>';

const exact = (n) => n.toLocaleString();
const TRIGGER = { auto: "Auto", manual: "/compact" };

/** A finished compaction's marker. `fresh`: it just replaced the running card. */
export function compactHtml(fresh) {
  return `<div class="compact cpt${fresh ? " c-new" : ""}"><div class="cpt-card"><button type="button" class="cpt-head" aria-expanded="false">
    <span class="cpt-ic">${FOLD_ICON}</span><span class="cpt-title">Context compacted</span><span class="cpt-how"></span>${CARET}
    <span class="cpt-bar" aria-hidden="true"><i></i></span><span class="cpt-fig"></span></button>
    <div class="cpt-sum" hidden><p class="cpt-exact"></p><p class="cpt-label">Summary the agent continued from</p><div class="cpt-body md"></div><p class="cpt-none"></p></div></div></div>`;
}

/** The running one: the same card, its bar full, timing it. */
export const COMPACTING_HTML = `<div class="compact cpt live" role="status"><div class="cpt-card"><div class="cpt-head">
    <span class="cpt-ic">${FOLD_ICON}</span><span class="cpt-title">Compacting context</span><span class="cpt-how" aria-hidden="true"></span>
    <span class="cpt-bar" aria-hidden="true"><i></i></span><span class="cpt-fig" aria-hidden="true"></span></div></div></div>`;

/** Whether a marker has anything to show opened. */
export function compactHasDetail(c) {
  return !!(c.summary || c.preTokens);
}

/** The marker's full reading for a screen reader. */
function aria(c) {
  const parts = ["Context compacted"];
  if (c.preTokens && c.postTokens) parts.push(`from ${exact(c.preTokens)} to ${exact(c.postTokens)} tokens`);
  else if (c.preTokens) parts.push(`${exact(c.preTokens)} tokens summarized`);
  if (c.trigger === "auto") parts.push("automatically");
  if (c.trigger === "manual") parts.push("by /compact");
  if (c.durationMs) parts.push(`in ${duration(c.durationMs)}`);
  return parts.join(", ");
}

/**
 * Paints a finished compaction's marker (compactHtml), opened or not.
 * `backend` names why there is no summary to show; `markdown(el, text)`
 * renders the summary.
 */
export function paintCompaction(node, c, { open, backend, markdown }) {
  const detail = compactHasDetail(c);
  const row = node.querySelector(".cpt-head");
  node.classList.toggle("open", open);
  row.disabled = !detail;
  row.setAttribute("aria-expanded", String(open));
  row.setAttribute("aria-label", `${aria(c)}.${detail ? ` ${open ? "Hide" : "Show"} ${c.summary ? "the summary the agent continued from" : "the details"}.` : ""}`);
  setText(row.querySelector(".cpt-how"), [TRIGGER[c.trigger], c.durationMs ? duration(c.durationMs) : ""].filter(Boolean).join(" · "));
  const both = !!(c.preTokens && c.postTokens);
  node.classList.toggle("sized", both);
  if (both) row.querySelector(".cpt-bar").style.setProperty("--kept", String(Math.min(1, c.postTokens / c.preTokens)));
  setText(row.querySelector(".cpt-fig"), both ? `${formatTokens(c.preTokens)} → ${formatTokens(c.postTokens)}` : c.preTokens ? `${formatTokens(c.preTokens)} summarized` : "");
  const sum = node.querySelector(".cpt-sum");
  sum.hidden = !open;
  if (!open) return;
  const fig = sum.querySelector(".cpt-exact");
  fig.hidden = !c.preTokens;
  setText(fig, both
    ? `${exact(c.preTokens)} → ${exact(c.postTokens)} tokens · ${Math.max(0, Math.round((1 - c.postTokens / c.preTokens) * 100))}% smaller`
    : c.preTokens ? `${exact(c.preTokens)} tokens summarized` : "");
  sum.querySelector(".cpt-label").hidden = !c.summary;
  const body = sum.querySelector(".cpt-body");
  body.hidden = !c.summary;
  if (c.summary) markdown(body, c.summary);
  const none = sum.querySelector(".cpt-none");
  none.hidden = !!c.summary;
  setText(none, backend === "codex" ? "Codex keeps this summary encrypted, so it cannot be shown here." : "No summary was saved for this one.");
}

/**
 * Paints the running card: how long it has run (each second while it is on
 * the page), from `since` (ms), and the size it started from (`before`, the
 * context meter's tokens) when known.
 */
export function paintCompacting(node, since, before) {
  node._since = since;
  setText(node.querySelector(".cpt-fig"), Number.isFinite(before) && before > 0 ? `${formatTokens(before)} → …` : "");
  const tick = () => setText(node.querySelector(".cpt-how"), duration(Date.now() - node._since));
  tick();
  if (node._clock) return;
  node._clock = setInterval(() => {
    if (!node.isConnected) { clearInterval(node._clock); node._clock = 0; return; }
    tick();
  }, 1000);
}
