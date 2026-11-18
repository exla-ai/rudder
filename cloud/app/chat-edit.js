// Edit a sent message. Every message in a chat that started a turn, except
// the first, can be edited while the agent is not mid-turn: Edit shows under
// it on hover on a desktop and always on a phone, where a long press also
// opens its menu.
// Sending the edited text takes the conversation back to just before that
// message and runs the new text in its place, so the agent forgets the turns
// after it (native chat_edit.rs). Files the agent changed stay as they are,
// and the old conversation stays on disk.
import { h, copyText } from "/app/render.js";
import { app, api, coarse, openSheet, toast } from "/app/core.js";
import { showAgentMenu } from "/app/agent-menu.js";

const BUSY = new Set(["starting", "working"]);
const LONG_PRESS_MS = 450;

/**
 * One chat's Edit action. Returns `paint(node, message, transcript)` for the
 * transcript renderer to call on every user message it paints: a message
 * the transcript's `edits` lists gets the action, every other one loses it.
 * `agent` returns the chat's agent row now; `onSent` reloads the transcript.
 */
export function chatEdit({ workspace, run, agent, readOnly, onSent }) {
  return function paint(node, message, transcript) {
    const edits = transcript?.edits || [];
    const target = edits.find((edit) => edit.messageId === message.id);
    const a = agent();
    const show = !readOnly && !!a && !a.pending && !BUSY.has(a.status) && !!target;
    // The request names the chat's latest prompt (the list's last) so a
    // chat that got newer messages since is refused, not cut short unseen.
    node._edit = show
      ? { workspace, run, entry: target.entry, latest: edits[edits.length - 1].entry, text: message.text || "", onSent }
      : null;
    node.classList.toggle("editable", show);
    let foot = node.querySelector(":scope > .edit-foot");
    if (!show) {
      foot?.remove();
      return;
    }
    // A phone shows the same Edit button (there is no hover to find it) and
    // a long press opens the message menu as well.
    if (coarse) holdForMenu(node);
    if (!foot) {
      foot = h(`<div class="m-foot edit-foot"><button type="button" aria-label="Edit this message">Edit</button></div>`);
      foot.querySelector("button").addEventListener("click", () => node._edit && openEditSheet(node._edit));
      node.appendChild(foot);
    }
  };
}

// A phone's message menu: a long press on the message. Selecting its text is
// off there (styles.css), so the menu copies it too.
function holdForMenu(node) {
  if (node._editHold) return;
  node._editHold = true;
  let timer = 0, x0 = 0, y0 = 0, opened = false;
  const cancel = () => { clearTimeout(timer); timer = 0; };
  node.addEventListener("touchstart", (e) => {
    cancel();
    opened = false;
    if (!node._edit || e.touches.length !== 1 || e.target.closest("a, button")) return;
    x0 = e.touches[0].clientX;
    y0 = e.touches[0].clientY;
    timer = setTimeout(() => {
      timer = 0;
      if (!node._edit || !node.isConnected) return;
      opened = true;
      messageMenu(node, x0, y0);
    }, LONG_PRESS_MS);
  }, { passive: true });
  node.addEventListener("touchmove", (e) => {
    const t = e.touches[0];
    if (timer && Math.hypot(t.clientX - x0, t.clientY - y0) > 10) cancel();
  }, { passive: true });
  // The finger lifting off the menu it opened must not also press an item.
  node.addEventListener("touchend", (e) => {
    cancel();
    if (opened) { e.preventDefault(); opened = false; }
  });
  node.addEventListener("touchcancel", cancel);
  // Android also fires contextmenu on a long press: this menu is the one.
  node.addEventListener("contextmenu", (e) => { if (node._edit) e.preventDefault(); });
}

function messageMenu(node, x, y) {
  const edit = node._edit;
  showAgentMenu({
    x, y,
    label: "Message",
    items: [
      { label: "Edit", icon: "edit", run: () => openEditSheet(edit) },
      { label: "Copy", icon: "copy", run: () => copyMessage(edit.text) },
    ],
  });
}

async function copyMessage(text) {
  if (await copyText(text)) toast("Copied");
  else toast("Couldn't copy the message", { error: true });
}

function openEditSheet({ workspace, run, entry, latest, text, onSent }) {
  const { dlg, body, close } = openSheet({ title: "Edit message", className: "edit-sheet" });
  const form = h(`<form style="display:flex;flex-direction:column;gap:10px;min-height:0">
    <textarea class="ask-other-text" rows="6" aria-label="Your message" style="max-height:50vh"></textarea>
    <p class="create-note" style="text-align:left;margin:0 2px">The conversation goes back to this message, and the agent forgets everything after it. Files stay as they are now.</p>
    <p class="repo-note err" role="alert" hidden style="padding:0 2px;margin:0"></p>
    <div class="d-actions"><button type="button" class="btn" data-act="cancel">Cancel</button><button type="submit" class="btn accent">Send</button></div>
  </form>`);
  body.appendChild(form);
  const textarea = form.querySelector("textarea");
  const error = form.querySelector(".err");
  const send = form.querySelector('[type="submit"]');
  textarea.value = text;
  let sending = false;
  const sync = () => { send.disabled = sending || !textarea.value.trim(); };
  textarea.addEventListener("input", sync);
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      form.requestSubmit();
    }
  });
  form.querySelector('[data-act="cancel"]').addEventListener("click", close);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const value = textarea.value.trim();
    if (!value || sending) return;
    sending = true;
    sync();
    error.hidden = true;
    try {
      const res = await api("/api/app/agent-edit", { method: "POST", body: { workspace, run, entry, latest, text: value } });
      close();
      toast(res?.status === "queued" ? "Editing when the cloud machine picks it up" : "Sent the edited message");
      onSent();
      app.refreshState();
    } catch (err) {
      // Closed while it was sending: the sheet is gone, so say it here.
      if (!dlg.open) {
        if (err.status !== 401) toast(`Couldn't edit the message: ${err.message}`, { error: true });
        return;
      }
      error.textContent = err.message || "Couldn't edit the message";
      error.hidden = false;
      sending = false;
      sync();
    }
  });
  sync();
  textarea.focus();
  textarea.setSelectionRange(textarea.value.length, textarea.value.length);
}
