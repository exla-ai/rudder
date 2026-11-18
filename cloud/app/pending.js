// Messages you sent that the agent's conversation does not show yet.
//
// A sent message keeps no id on its way: the dashboard types it into the
// agent's terminal (sanitize_steer_instruction collapses runs of spaces and
// trims every line) and Claude Code or Codex records what arrived. So a sent
// bubble settles on a user message that the transcript added after the send and
// that reads the same once whitespace is ignored. Each message settles one
// bubble: the same text sent twice shows twice until the transcript has both.

const KEEP_MS = 5 * 60 * 1000;

/** A message as the transcript can match it: whitespace and controls ignored. */
export const sentKey = (text) => String(text || "").replace(/[\s\p{Cc}]+/gu, " ").trim();

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

/** The bubbles of `list` the transcript `messages` does not show yet. */
export function settlePending(list, messages, now) {
  const users = (messages || []).filter((m) => m.role === "user" && m.ts != null);
  const taken = new Set();
  return list.filter((p) => {
    if (p.state === "failed") return true;
    if (now - p.ts > KEEP_MS) return false;
    const key = sentKey(p.sentText || p.text);
    const hit = users.find((m) => !taken.has(m) && m.ts > p.after && sentKey(m.text) === key);
    if (!hit) return true;
    taken.add(hit);
    return false;
  });
}
