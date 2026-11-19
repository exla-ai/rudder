// The files a tool card names, as links that open them beside the chat
// (file-link.js): the files a call read or changed (`tool.files`), in its row
// in place of its input, and the paths a search's output lists
// (`tool.found`), on their rows. A change opens with its lines selected. The
// cloud machine reads them off each harness's log (worker/tool-files.mjs).
// A teammate's agent has none (`links` false), as its messages have none.
//
// A link is its own target: tool rows toggle their card on a click anywhere
// else (app.js), and file-link.js opens the file.
import { fileRef, linkTo } from "/app/file-refs.js";

/** What a tool's links show, for a card's repaint check. */
export function toolFilesSig(tool) {
  const files = (tool.files || []).map((f) => `${f.path}:${f.line}-${f.end}`).join(",");
  return `${files}|${tool.found?.length || 0}`;
}

/**
 * Paint `tool.files` into a row's `.t-files`. One file shows its path, more
 * show their names. Returns whether there were any, for the row to show its
 * input instead.
 */
export function paintToolFiles(el, tool, links) {
  const files = links ? tool.files || [] : [];
  el.hidden = !files.length;
  el.replaceChildren(...files.map((f) => link(fileRef(f.path, f.line, 0, f.end), files.length > 1 ? f.path.split("/").pop() : f.path)));
  return files.length > 0;
}

/** Fill `pre` with `text`, a tool's output as shown, the rows `tool.found` names linked. */
export function paintToolOutput(pre, text, tool, links) {
  const found = links ? tool.found || [] : [];
  if (!found.length) {
    pre.textContent = text;
    return;
  }
  const byRow = new Map(found.map((f) => [f.row, f]));
  const parts = [];
  text.split("\n").forEach((row, i) => {
    if (i) parts.push("\n");
    const f = byRow.get(i);
    const at = f ? row.indexOf(f.text) : -1;
    // A row cut short where the output is shown in part.
    if (at < 0) {
      parts.push(row);
      return;
    }
    parts.push(row.slice(0, at), link(fileRef(f.path, f.line), f.text), row.slice(at + f.text.length));
  });
  pre.replaceChildren(...parts);
}

function link(ref, text) {
  const a = document.createElement("a");
  linkTo(a, ref);
  a.classList.add("t-file");
  a.setAttribute("aria-label", `Open ${ref.path}${ref.line ? `, line ${ref.line}` : ""}${ref.end ? ` to ${ref.end}` : ""}`);
  // Left to right inside a right-to-left box that ellipsizes its start: a long path keeps its file name.
  const bdi = document.createElement("bdi");
  bdi.textContent = text;
  a.appendChild(bdi);
  return a;
}
