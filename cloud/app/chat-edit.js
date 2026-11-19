// Edit a sent message. Every message in a chat that started a turn can be
// edited, the first one too and while the agent is mid-turn: Edit shows under
// it on hover on a desktop and always on a phone, where a long press also
// opens its menu. Edit turns the message into a text box in place, and the
// turns after it fade while it is open.
// Sending the edited text takes the conversation back to just before that
// message and runs the new text in its place, so the agent forgets the turns
// after it (native chat_edit.rs). Editing the first message starts the
// conversation over. Mid-turn, the turn is stopped first. Files the agent
// changed stay as they are, and the old conversation stays on disk.
import { h, copyText } from "/app/render.js";
import { app, api, coarse, toast } from "/app/core.js";
import { showAgentMenu } from "/app/agent-menu.js";

const BUSY = new Set(["starting", "working"]);
const LONG_PRESS_MS = 450;

/**
 * One chat's Edit action. Returns `paint(node, message, transcript)` for the
 * transcript renderer to call on every user message it paints: a message
 * the transcript's `edits` lists gets the action, every other one loses it.
 * `paint.editing()` says whether a message is open for editing, so the chat
 * does not scroll it away. `agent` returns the chat's agent row now;
 * `onSent` reloads the transcript.
 */
export function chatEdit({ workspace, run, agent, readOnly, onSent }) {
  // The message open for editing: one at a time.
  let open = null;
  const busy = () => BUSY.has(agent()?.status);

  function paint(node, message, transcript) {
    const edits = transcript?.edits || [];
    const target = edits.find((edit) => edit.messageId === message.id);
    const a = agent();
    const show = !readOnly && !!a && !a.pending && !!target;
    // The request names the chat's latest prompt (the list's last) so a
    // chat that got newer messages since is refused, not cut short unseen.
    node._edit = show ? { entry: target.entry, latest: edits[edits.length - 1].entry, text: message.text || "" } : null;
    node.classList.toggle("editable", show);
    node._editor?.note(busy());
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
      foot.querySelector("button").addEventListener("click", () => startEdit(node));
      node.appendChild(foot);
    }
  }
  paint.editing = () => !!open?.isConnected;

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
      if (!node._edit || node._editor || e.touches.length !== 1 || e.target.closest("a, button")) return;
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
    node.addEventListener("contextmenu", (e) => { if (node._edit && !node._editor) e.preventDefault(); });
  }

  function messageMenu(node, x, y) {
    const text = node._edit.text;
    showAgentMenu({
      x, y,
      label: "Message",
      items: [
        { label: "Edit", icon: "edit", run: () => startEdit(node) },
        { label: "Copy", icon: "copy", run: () => copyMessage(text) },
      ],
    });
  }

  // The message becomes a text box with its text, where its bubble was.
  function startEdit(node) {
    if (node._editor) return node._editor.focus();
    if (!node._edit) return;
    open?._editor?.close();
    // Taken now: a newer prompt arriving while this is open refuses the edit.
    const { entry, latest, text } = node._edit;
    const form = h(`<form class="msg-edit">
      <textarea rows="1" aria-label="Edit your message"></textarea>
      <p class="me-note"></p>
      <p class="me-err" role="alert" hidden></p>
      <div class="me-actions"><button type="button" class="btn small" data-act="cancel">Cancel</button><button type="submit" class="btn small accent">Send</button></div>
    </form>`);
    const textarea = form.querySelector("textarea");
    const noteEl = form.querySelector(".me-note");
    const error = form.querySelector(".me-err");
    const send = form.querySelector('[type="submit"]');
    textarea.value = text;
    let sending = false;
    const sync = () => { send.disabled = sending || !textarea.value.trim(); };
    const grow = () => { textarea.style.height = "auto"; textarea.style.height = `${textarea.scrollHeight}px`; };
    const editor = {
      note(isBusy) {
        noteEl.textContent = isBusy ? "Stops the current turn first. Later messages will be dropped." : "Later messages will be dropped.";
      },
      focus() { textarea.focus(); },
      close() {
        form.remove();
        node.classList.remove("editing");
        node._editor = null;
        if (open === node) open = null;
      },
    };
    textarea.addEventListener("input", () => { grow(); sync(); });
    textarea.addEventListener("keydown", (e) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        editor.close();
        node.querySelector(":scope > .edit-foot button")?.focus();
        return;
      }
      // Enter sends on a desktop, Shift+Enter is a new line. A phone's
      // Enter is a new line; Send sends.
      if (e.key === "Enter" && !e.shiftKey && !coarse && !e.isComposing) {
        e.preventDefault();
        form.requestSubmit();
      }
    });
    form.querySelector('[data-act="cancel"]').addEventListener("click", () => editor.close());
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const value = textarea.value.trim();
      if (!value || sending) return;
      const stopping = busy();
      sending = true;
      sync();
      error.hidden = true;
      try {
        const res = await api("/api/app/agent-edit", { method: "POST", body: { workspace, run, entry, latest, text: value } });
        editor.close();
        const queued = stopping ? "Stopping the turn, then sending the edited message" : "Editing when the cloud machine picks it up";
        toast(res?.status === "queued" ? queued : "Sent the edited message");
        onSent();
        app.refreshState();
      } catch (err) {
        // Closed while it was sending: the box is gone, so say it here.
        if (node._editor !== editor || !node.isConnected) {
          if (err.status !== 401) toast(`Couldn't edit the message: ${err.message}`, { error: true });
          return;
        }
        error.textContent = err.message || "Couldn't edit the message";
        error.hidden = false;
        sending = false;
        sync();
      }
    });
    node._editor = editor;
    open = node;
    editor.note(busy());
    node.classList.add("editing");
    node.querySelector(":scope > .bubble").after(form);
    sync();
    grow();
    textarea.focus({ preventScroll: true });
    textarea.setSelectionRange(textarea.value.length, textarea.value.length);
    form.scrollIntoView({ block: "nearest" });
  }

  return paint;
}

async function copyMessage(text) {
  if (await copyText(text)) toast("Copied");
  else toast("Couldn't copy the message", { error: true });
}
