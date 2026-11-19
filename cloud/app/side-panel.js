// The right-hand slot beside the conversation, shared by Files, Canvas, the
// terminal and Notes. On desktop it is the shell's right column (`.dsk-files`,
// app.js DesktopShell), sized by dragging its left edge, with the width kept
// on this device. Narrower, the panel is a full-screen sheet over the page,
// which stays mounted underneath.
//
// One panel at a time. A panel is `{ el, close }`: `close()` resolves false to
// keep the slot (Files holding unsaved edits), true once it has stopped.
// Opening one asks the panel there to close first, and opens only if it did.
import { ls } from "/app/core.js";

const WIDTH_KEY = "rudder.filesWidth";
let occupant = null;

const column = () => document.querySelector(".dsk-files");

/** The panel in the slot, or null. */
export function sideOccupant() {
  return occupant;
}

/** Put `panel` in the slot. Resolves false when the panel there kept it. */
export async function openSide(panel) {
  if (occupant && occupant !== panel) {
    const leaving = occupant;
    if (!(await leaving.close())) return false;
    if (occupant === leaving) detach(leaving);
  }
  occupant = panel;
  placeSide();
  return true;
}

/** Take `panel` out of the slot (only if it is there). */
export function closeSide(panel) {
  if (occupant !== panel) return;
  occupant = null;
  detach(panel);
}

/** Put the panel in the slot where it belongs now: the desktop shell is
 * rebuilt when the window crosses its breakpoint, so the column it was in may
 * be gone, or there may be one now. */
export function placeSide() {
  if (!occupant) return;
  const { el } = occupant;
  const col = column();
  if (!col) {
    el.classList.add("side-sheet");
    if (el.parentElement !== document.body) document.body.appendChild(el);
    return;
  }
  el.classList.remove("side-sheet");
  if (el.parentElement !== col) col.replaceChildren(el);
  col.hidden = false;
  const dsk = col.closest(".dsk");
  dsk?.classList.add("with-files");
  const width = Number(ls.get(WIDTH_KEY));
  if (width) dsk?.style.setProperty("--files-w", `${width}px`);
}

function detach(panel) {
  panel.el.remove();
  const col = column();
  if (col && !col.firstElementChild) {
    col.hidden = true;
    col.closest(".dsk")?.classList.remove("with-files");
  }
}

/** A panel's left-edge grip (pointerdown): drag to size the desktop column. */
export function dragColumn(e) {
  const dsk = e.currentTarget.closest(".dsk");
  if (!dsk) return;
  e.preventDefault();
  const grip = e.currentTarget;
  grip.setPointerCapture(e.pointerId);
  dsk.classList.add("resizing");
  // Every listener of this drag goes when the pointer is released or lost.
  const drag = new AbortController();
  grip.addEventListener("pointermove", (ev) => {
    const width = Math.round(Math.min(Math.max(window.innerWidth - ev.clientX, 360), window.innerWidth - 420));
    dsk.style.setProperty("--files-w", `${width}px`);
  }, { signal: drag.signal });
  grip.addEventListener("lostpointercapture", () => {
    drag.abort();
    dsk.classList.remove("resizing");
    const width = parseInt(getComputedStyle(dsk).getPropertyValue("--files-w"), 10);
    if (width) ls.set(WIDTH_KEY, width);
  }, { signal: drag.signal });
}
