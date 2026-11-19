// A file a chat message names, opened read only beside the chat: a Markdown
// link to a local path or a file:// URL, or inline code that is a path, with
// its line when the message gives one (file-refs.js marks them). The panel
// takes the right-hand slot (side-panel.js) that Files, Canvas, the terminal
// and Notes share, and shows the file in the Files panel's viewer
// (file-view.js): Markdown as a preview with its source a toggle away, code
// with its syntax colored, scrolled to the line.
//
// Your cloud machine resolves the path: the agent's workspace first, then the
// project checkout, or anywhere in the machine user's own folders, minus files
// that can hold keys or tokens (src/project-files/file-refs.ts). Only agents
// on your own machine open files; a teammate's shared agent does not.
import { api } from "/app/core.js";
import { h, icon } from "/app/render.js";
import { FileView, errorRow } from "/app/file-view.js";
import { fileRef, fileRefOfLink } from "/app/file-refs.js";
import { openSide, closeSide, sideOccupant, placeSide, dragColumn } from "/app/side-panel.js";
import { UpdateNow } from "/app/home-update.js";

const el = h(`<section class="fp fl has-file show-file" aria-label="File">
  <div class="side-grip" aria-hidden="true"></div>
  <header class="fp-head">
    <span class="fp-title">${icon("file")}<span>File</span></span>
    <button type="button" class="icon-btn fl-close" aria-label="Close file" title="Close file">${icon("x")}</button>
  </header>
  <div class="fp-body"><div class="fl-message" hidden></div></div>
</section>`);
const bodyEl = el.querySelector(".fp-body"), messageEl = el.querySelector(".fl-message");
el.querySelector(".side-grip").addEventListener("pointerdown", dragColumn);
el.querySelector(".fl-close").addEventListener("click", () => closePanel());

// target: the chat the file was named in, { ws, run }. `opening` drops the
// answer to a click another click replaced.
const state = { target: null, opening: 0, updater: null };
const view = FileView({ onLink: (path) => { if (state.target) void openFileRef(state.target, fileRef(path)); } });
bodyEl.appendChild(view.el);

// The panel in the slot. Nothing here is ever unsaved, so it always gives way.
const panel = { el, close: async () => { stop(); return true; } };
const isOpen = () => sideOccupant() === panel;

/** Make the file links in a chat's messages open here. `target`: { ws, run }. */
export function mountFileRefs(chat, target) {
  const activate = (e) => {
    const link = e.target.closest?.("a.file-ref");
    if (!link || !chat.contains(link)) return;
    e.preventDefault();
    void openFileRef(target, fileRefOfLink(link));
  };
  chat.addEventListener("click", activate);
  chat.addEventListener("keydown", (e) => { if (e.key === "Enter") activate(e); });
}

/** Open `ref` ({ path, line, col }) as the chat `target` ({ ws, run }) names it. */
export async function openFileRef(target, ref) {
  if (!isOpen() && !(await openSide(panel))) return;
  state.target = target;
  const mine = ++state.opening;
  dropUpdater();
  view.close();
  showMessage(h(`<div class="fp-empty">Opening</div>`));
  try {
    const doc = await read(target, ref.path);
    if (mine !== state.opening) return;
    showMessage(null);
    await view.open(source(target, doc), doc.path, { line: ref.line, col: ref.col });
  } catch (e) {
    if (mine !== state.opening) return;
    const parts = [errorRow(e, "Unable to open this file")];
    // An older cloud machine: Update now, then the file again.
    if (e?.needsUpdate) {
      state.updater = UpdateNow({ onDone: () => { if (mine === state.opening) void openFileRef(target, ref); } });
      parts.push(state.updater.el);
    }
    showMessage(...parts);
  }
}

/** The route changed, or the other of two panes took focus: the file stays
 * while its chat is on screen and closes with it. */
export function followFileLink(route) {
  if (!isOpen()) return;
  placeSide(); // The desktop shell is rebuilt when the window crosses its breakpoint.
  const t = state.target;
  const chat = route?.name === "agent" || route?.name === "all-agent";
  if (!t || !chat || route.ws !== t.ws || route.run !== t.run) closePanel();
}

function closePanel() {
  stop();
  closeSide(panel);
}

function stop() {
  state.opening++;
  state.target = null;
  dropUpdater();
  view.close();
  showMessage(null);
}

function dropUpdater() {
  state.updater?.destroy();
  state.updater = null;
}

function showMessage(...nodes) {
  const shown = nodes.filter(Boolean);
  messageEl.replaceChildren(...shown);
  messageEl.hidden = !shown.length;
}

const read = (target, path, knownRevision) =>
  api(`/api/app/file-ref?${new URLSearchParams({ workspaceId: target.ws, runId: target.run, path, ...(knownRevision ? { knownRevision } : {}) })}`);

/** The viewer's source for one opened file. Its first read is the one that
 * resolved the path; the viewer's refreshes ask for the resolved path. */
function source(target, first) {
  let pending = first;
  return {
    key: JSON.stringify(["file-ref", target.ws, target.run]),
    readOnly: true,
    async read(path, knownRevision) {
      if (pending && path === pending.path && !knownRevision) { const doc = pending; pending = null; return doc; }
      return read(target, path, knownRevision);
    },
  };
}
