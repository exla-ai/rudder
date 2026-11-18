// Edit a sent message. The chat's latest prompt gets an Edit action while the
// agent is not mid-turn. Sending the edited text takes the conversation back
// to just before that prompt and runs the new text in its place (native
// chat_edit.rs). Files the agent changed stay as they are, and the old
// conversation stays on disk.
import { h } from "/app/render.js";
import { app, api, openSheet, toast, track } from "/app/core.js";

const BUSY = new Set(["starting", "working"]);

/**
 * One chat's Edit action. Returns `paint(node, message, transcript)` for the
 * transcript renderer to call on every user message it paints: the message
 * the transcript's `edit` names gets the action, every other one loses it.
 * `agent` returns the chat's agent row now; `onSent` reloads the transcript.
 */
export function chatEdit({ workspace, run, agent, readOnly, onSent }) {
  return function paint(node, message, transcript) {
    const target = transcript?.edit;
    const a = agent();
    const show = !readOnly && !!a && !a.pending && !BUSY.has(a.status) && target?.messageId === message.id;
    let foot = node.querySelector(":scope > .edit-foot");
    if (!show) {
      foot?.remove();
      return;
    }
    if (!foot) {
      foot = h(`<div class="m-foot edit-foot"><button type="button" aria-label="Edit this message">Edit</button></div>`);
      foot.querySelector("button").addEventListener("click", () => openEditSheet({ workspace, run, ...foot._edit, onSent }));
      node.appendChild(foot);
    }
    foot._edit = { entry: target.entry, text: message.text || "" };
  };
}

function openEditSheet({ workspace, run, entry, text, onSent }) {
  const { dlg, body, close } = openSheet({ title: "Edit message", className: "edit-sheet" });
  const form = h(`<form style="display:flex;flex-direction:column;gap:10px;min-height:0">
    <textarea class="ask-other-text" rows="6" aria-label="Your message" style="max-height:50vh"></textarea>
    <p class="create-note" style="text-align:left;margin:0 2px">The conversation goes back to this message. Files stay as they are.</p>
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
      const res = await api("/api/app/agent-edit", { method: "POST", body: { workspace, run, entry, text: value } });
      track("chat_message_edited");
      close();
      toast(res?.status === "queued" ? "Editing when the cloud machine picks it up" : "Sent the edited message");
      onSent();
      app.refreshState();
    } catch (err) {
      if (!dlg.open) return;
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
