// Messages you sent that the agent's conversation does not show yet.
//
// A sent message keeps no id on its way: the dashboard types it into the
// agent's terminal (sanitize_steer_instruction collapses runs of spaces and
// trims every line) and Claude Code or Codex records what arrived. So a sent
// bubble settles on a user message that the transcript added after the send,
// that reads the same once whitespace is ignored and that has the same files
// attached. Each message settles one bubble: the same text sent twice shows
// twice until the transcript has both. A slash command settles on the note
// or compaction it left instead (settlePending).

const KEEP_MS = 5 * 60 * 1000;

/**
 * The text sent for a message with files: the saved paths listed after it,
 * which the agent opens like any file. The worker reads the list back out
 * (worker/transcripts.mjs showAttachments), so a transcript message carries
 * the words as `text` and the paths as `attachments`.
 */
export function withAttachments(text, paths) {
  if (!paths.length) return text;
  const list = paths.map((p) => `- ${p}`).join("\n");
  return text ? `${text}\n\nAttached files:\n${list}` : `Attached files:\n${list}`;
}

/** A message as the transcript can match it: whitespace and controls ignored. */
export const sentKey = (text) => String(text || "").replace(/[\s\p{Cc}]+/gu, " ").trim();
const matchKey = (text, paths) => [sentKey(text), ...paths].join("\n");

/**
 * The newest time the transcript shows, taken when a message is sent: only a
 * user message recorded after it can be that message. Transcript times come
 * from the agent's own machine, so this device's clock plays no part.
 */
export function sentAfter(messages) {
  let at = -Infinity;
  for (const m of messages || []) if (m.ts != null && m.ts > at) at = m.ts;
  return at;
}

/**
 * The slash command a message runs ("/goal" for "/goal ship it"), or null.
 * A path ("/tmp/x is gone") is words the agent gets as a message.
 */
const commandOf = (text) => /^\/[^\s/]+(?=\s|$)/.exec(sentKey(text))?.[0] ?? null;

/**
 * The command a transcript item records. The agent's CLI runs a slash
 * command instead of recording what was typed: a note names it
 * (worker/transcripts.mjs `command`), and /compact leaves a compaction.
 */
const recorded = (m) => {
  if (m.kind === "compact" && m.compact?.trigger !== "auto") return "/compact";
  return m.command ?? null;
};

/** The bubbles of `list` the transcript `messages` does not show yet. */
export function settlePending(list, messages, now) {
  const shown = (messages || []).filter((m) => m.ts != null && (m.role === "user" || recorded(m)));
  const taken = new Set();
  return list.filter((p) => {
    if (p.state === "failed") return true;
    const key = matchKey(p.text, (p.files || []).map((f) => f.path));
    const command = commandOf(p.text);
    const hit = shown.find((m) => {
      if (m.ts <= p.after || p.usedIds?.has(m.id)) return false;
      const matches = (m.role === "user" && matchKey(m.text, m.attachments || []) === key)
        || (command !== null && recorded(m) === command);
      if (!matches) return false;
      if (!taken.has(m.id)) return true;
      // Keep this entry consumed on later polls after the earlier bubble leaves.
      (p.usedIds ??= new Set()).add(m.id);
      return false;
    });
    if (!hit) return p.state !== "sent" || now - p.ts <= KEEP_MS;
    taken.add(hit.id);
    return false;
  });
}
