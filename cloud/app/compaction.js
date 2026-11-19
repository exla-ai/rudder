// A compaction in the chat (compaction.css). When an agent's context fills
// up, its backend summarizes the conversation and carries on from the summary.
//  - The marker a finished one leaves (worker/transcripts.mjs compactMessage)
//    says on its line how far the context shrank. Opened, it shows by how
//    much, what set it off, how long it took and the summary the agent
//    continued from.
//  - While one runs, a row in the same place times it. The session file gets
//    nothing until it ends, so the home reads it off the agent's pane
//    (transcript `compacting`).
import { setText } from "/app/render.js";
import { formatTokens } from "/app/agent-stats.js";
import { duration } from "/app/subagents.js";

// Arrows from above and below into a line: the conversation folded into less.
export const FOLD_ICON = '<svg class="c-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3v6M9 6l3 3 3-3M12 21v-6M9 18l3-3 3 3M5 12h14"/></svg>';

const exact = (n) => n.toLocaleString();
const TRIGGER = { auto: "Automatic", manual: "Ran /compact" };

/** The line's tokens: "367K → 25K", or the size before alone. */
export function compactMeta(c) {
  if (c.preTokens && c.postTokens) return `${formatTokens(c.preTokens)} → ${formatTokens(c.postTokens)}`;
  if (c.preTokens) return `${formatTokens(c.preTokens)} tokens summarized`;
  return "";
}

/** Whether an opened marker has anything to show. */
export function compactHasDetail(c) {
  return !!(c.summary || c.preTokens || c.durationMs || TRIGGER[c.trigger]);
}

/** The marker's full reading for a screen reader. */
export function compactAria(c) {
  const parts = ["Context compacted"];
  if (c.preTokens && c.postTokens) parts.push(`from ${exact(c.preTokens)} to ${exact(c.postTokens)} tokens`);
  else if (c.preTokens) parts.push(`${exact(c.preTokens)} tokens summarized`);
  if (TRIGGER[c.trigger]) parts.push(TRIGGER[c.trigger].toLowerCase());
  if (c.durationMs) parts.push(`took ${duration(c.durationMs)}`);
  return parts.join(", ");
}

/**
 * The card above the summary of an opened marker: a bar the size before,
 * filled to the size after, and in words by how much, what set it off and
 * how long it took. `backend` names why there is no summary to show.
 */
export function paintCompactCard(sum, c, backend) {
  const card = sum.querySelector(".c-card");
  const both = !!(c.preTokens && c.postTokens);
  const how = [TRIGGER[c.trigger], c.durationMs ? `took ${duration(c.durationMs)}` : ""].filter(Boolean).join(" · ");
  card.hidden = !c.preTokens && !how;
  const bar = card.querySelector(".c-squeeze");
  bar.hidden = !both;
  if (both) bar.style.setProperty("--kept", String(Math.min(1, c.postTokens / c.preTokens)));
  const fig = card.querySelector(".c-fig");
  fig.hidden = !c.preTokens;
  setText(fig, both
    ? `${exact(c.preTokens)} → ${exact(c.postTokens)} tokens · ${Math.max(0, Math.round((1 - c.postTokens / c.preTokens) * 100))}% smaller`
    : c.preTokens ? `${exact(c.preTokens)} tokens summarized` : "");
  const howEl = card.querySelector(".c-how");
  howEl.hidden = !how;
  setText(howEl, how);
  sum.querySelector(".c-sum-label").hidden = !c.summary;
  sum.querySelector(".c-sum-body").hidden = !c.summary;
  const none = sum.querySelector(".c-sum-none");
  none.hidden = !!c.summary;
  setText(none, backend === "codex" ? "Codex keeps this summary encrypted, so it cannot be shown here." : "No summary was saved for this one.");
}

/** The row standing in for a compaction that is running. */
export const COMPACTING_HTML = `<div class="compact live" role="status"><div class="compact-row"><span class="c-line"></span><span class="c-label">${FOLD_ICON}<span class="c-text">Compacting context</span><span class="c-meta" aria-hidden="true"></span></span><span class="c-line"></span></div></div>`;

/** Times the running row from `since` (ms), each second while it is on the page. */
export function paintCompacting(node, since) {
  node._since = since;
  const tick = () => setText(node.querySelector(".c-meta"), duration(Date.now() - node._since));
  tick();
  if (node._clock) return;
  node._clock = setInterval(() => {
    if (!node.isConnected) { clearInterval(node._clock); node._clock = 0; return; }
    tick();
  }, 1000);
}
