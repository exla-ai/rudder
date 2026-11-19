// Claude Code's suggested next prompt in the chat composer, the way Claude Code
// shows it in its own prompt box: dim text in the empty box while the agent is
// idle. Tab fills it in on a keyboard; on a touch screen a chip next to the
// model chip does. It never sends by itself, and typing hides it.
//
// The suggestion comes with the transcript (`suggestion: { text, at }`, from
// cloud/worker/suggestion.mjs). Only Claude Code makes one, and not after
// every turn; with none, the composer is unchanged.

import { coarse } from "/app/core.js";

let seq = 0;

export function mountPromptSuggestion(composer, { chipSlot }) {
  const ta = composer.textarea;
  const placeholder = ta.placeholder;
  // A span of its own at the hint's start: the hint keeps its other parts
  // (the Alt+[ steps span that desktop.css wraps away in a narrow pane).
  const tabHint = document.createElement("span");
  tabHint.className = "ps-tab";
  tabHint.textContent = "Tab to use the suggestion · ";
  tabHint.hidden = true;
  if (!coarse) composer.el.querySelector(".c-hint").prepend(tabHint);
  const describe = document.createElement("span");
  describe.className = "ps-sr";
  describe.id = `ps-sr-${seq++}`;
  composer.el.appendChild(describe);
  const chip = document.createElement("button");
  chip.type = "button";
  chip.className = "tool-chip ps-chip";
  chip.textContent = "Use suggestion";
  chip.hidden = true;
  if (coarse) chipSlot.appendChild(chip);

  let current = null; // { text, at } while the agent's prompt box shows one
  let idle = false;
  let sentAt = null; // `at` of the suggestion that was showing when you last sent
  let painted = null;

  const showing = () => !!current && idle && current.at !== sentAt && ta.value === "";

  // A placeholder never grows the box, so a suggestion longer than one line
  // would be cut off: measure the box as if it held the text, then empty it.
  const fit = (text) => composer.fit(text);

  const paint = () => {
    const text = showing() ? current.text : null;
    if (text === painted) return;
    painted = text;
    composer.el.classList.toggle("ps-on", text !== null);
    ta.placeholder = text ?? placeholder;
    fit(text);
    chip.hidden = text === null;
    tabHint.hidden = text === null;
    describe.textContent = text !== null ? `Suggested next prompt: ${text}. ${coarse ? "Use suggestion fills it in." : "Press Tab to fill it in."}` : "";
    if (text !== null) ta.setAttribute("aria-describedby", describe.id);
    else ta.removeAttribute("aria-describedby");
  };

  const fill = () => {
    if (!showing()) return;
    const { text } = current;
    ta.value = text;
    // The composer grows, enables Send and saves the draft on input.
    ta.dispatchEvent(new Event("input", { bubbles: true }));
    ta.focus();
    ta.setSelectionRange(text.length, text.length);
  };

  ta.addEventListener("keydown", (e) => {
    if (e.key !== "Tab" || e.shiftKey || e.altKey || e.ctrlKey || e.metaKey || e.isComposing || !showing()) return;
    e.preventDefault();
    fill();
  });
  ta.addEventListener("input", paint);
  chip.addEventListener("click", fill);
  // The composer sizes the box for its own value (on input, after a send, at
  // mount) and the pane can change width: fit the suggestion again after it.
  new ResizeObserver(() => { if (painted !== null) fit(painted); }).observe(ta);

  return {
    // From each transcript paint: the transcript's `suggestion`, and whether
    // the agent sits idle at its prompt (not working, no question pending).
    update({ suggestion, idle: isIdle }) {
      current = suggestion ?? null;
      idle = isIdle;
      paint();
    },
    // You sent something: the suggestion it answered never comes back.
    sent() {
      if (current) sentAt = current.at;
      paint();
    },
  };
}
