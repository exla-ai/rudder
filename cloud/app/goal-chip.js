// A chat's standing goal, the one `/goal` set in Claude Code or Codex: a
// quiet "Goal active" at the end of the chat header's status line ("Working
// · 88 tok/s · Goal active"). The goal itself shows on hover, or on tap.
// The transcript's `goal` (cloud/worker/chat-goal.mjs) decides; the chip is
// hidden while the chat has none, and goes when the goal is met, cleared or
// the agent stops.
import { h, setText } from "/app/render.js";
import { popover } from "/app/agent-stats.js";

const TARGET = `<svg class="gc-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="4.2"/><circle cx="12" cy="12" r="1.3" fill="currentColor" stroke="none"/></svg>`;

/**
 * Mount at the end of the header's status `line` and call `update(t.goal)`
 * after each transcript load, before AgentStats measures the line.
 */
export function GoalChip(line) {
  const el = h(`<button type="button" class="goal-chip" hidden><span aria-hidden="true">·</span><span class="gc-on">${TARGET}<span class="gc-l"></span></span></button>`);
  line.appendChild(el);
  let goal = null;
  const pop = popover(el, { label: "Goal", render: () => renderPop(goal) });

  function update(next) {
    goal = next || null;
    el.hidden = !goal;
    if (!goal) { pop.close(); return; }
    setText(el.querySelector(".gc-l"), goal.error ? "Goal unknown" : "Goal active");
    el.classList.toggle("gc-error", !!goal.error);
    el.setAttribute("aria-label", goal.error ? "Goal unknown. Show why" : `Goal active: ${goal.text}`);
    if (pop.open) pop.paint();
  }

  return { el, update, destroy: () => pop.destroy() };
}

function renderPop(goal) {
  const box = h(`<div class="st-body"><section class="st-sec"><div class="st-h"><span>Goal</span></div><div class="st-line gc-text"></div><p class="st-note"></p></section></div>`);
  const text = box.querySelector(".gc-text");
  const note = box.querySelector(".st-note");
  if (!goal) return box;
  if (goal.error) {
    text.remove();
    setText(note, `The cloud machine could not read it: ${goal.error}`);
    return box;
  }
  setText(text, goal.text);
  setText(note, "The agent keeps working until it is met. Send /goal clear to end it.");
  return box;
}
