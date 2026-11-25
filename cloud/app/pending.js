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
//
// A bubble never leaves on its own: one that waits too long on a step says
// where it is stuck, and keeps settling if the transcript shows it later.

/**
 * How long a message may sit "queued" (the home answered before the
 * dashboard did) before it reads as not picked up. A dashboard takes a
 * message from its inbox within a second, and the home already waited 12 s
 * for it, so a minute from the send means the dashboard is not taking it.
 */
export const QUEUE_LIMIT_MS = 60_000;

/**
 * How long a delivered message may be missing from the transcript before it
 * reads as not received. The agent records it as it takes it: at once when
 * it is free, within half a minute when its CLI has to start first, and when
 * its turn ends when it is busy, so the clock runs from the later of the
 * send and the last time the agent was seen working.
 */
export const SHOW_LIMIT_MS = 2 * 60_000;

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

/**
 * The bubbles of `list` the transcript `messages` does not show yet. One that
 * waited past its limit turns "stuck" (queued, the dashboard has not taken
 * it) or "lost" (delivered, the agent never recorded it). `p.ts` is when its
 * current step began; `busy` says whether the agent is working now.
 */
export function settlePending(list, messages, now, busy) {
  // Rudder's copy of a prompt it typed (worker/transcripts.mjs recordedTurns)
  // is not the agent's: only the agent's own record settles a message.
  const shown = (messages || []).filter((m) => m.ts != null && !m.recorded && (m.role === "user" || recorded(m)));
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
    if (hit) {
      taken.add(hit.id);
      return false;
    }
    if (p.state === "queued" && now - p.ts > QUEUE_LIMIT_MS) p.state = "stuck";
    if (p.state === "sent" || p.state === "waiting") {
      if (busy) p.busyAt = now;
      if (now - Math.max(p.ts, p.busyAt ?? -Infinity) > SHOW_LIMIT_MS) p.state = "lost";
    }
    return true;
  });
}

/**
 * Whether a failed send got no answer from the cloud machine: the connection
 * dropped, the phone gave up waiting, or the server said 503 or 504. The
 * message or new task may have gone in all the same. Any 4xx is an answer.
 */
export const unanswered = (err) => !(err?.status >= 400 && err.status < 500);

/**
 * The request id of what the user sends from one box. A send that got no
 * answer keeps its id, so sending the same thing again is the same request
 * and the cloud machine takes it once (worker/steer-inbox.mjs queueSteer).
 * `sig` says what is being sent; `settle` takes the error, or nothing once
 * it went in.
 */
export function sendAttempts() {
  let attempt = null;
  return {
    id(sig) {
      if (attempt?.sig !== sig) attempt = { sig, id: crypto.randomUUID() };
      return attempt.id;
    },
    settle(err) {
      if (!err || !unanswered(err)) attempt = null;
    },
  };
}

/** Whether a message is a slash command, which the transcript may not record. */
export const isCommand = (text) => commandOf(text) !== null;

/**
 * Whether the agent's transcript shows a message once the agent has it, so
 * its bubble can wait for that. Every message does but a slash command its
 * CLI runs without a record (worker/transcripts.mjs): Claude Code notes each
 * one it runs and Codex only /goal and /compact. OpenCode records only the
 * ones that run as a prompt, which the app does not know apart, and
 * Antigravity none. A command with no record leaves once it is delivered.
 */
export function leavesRecord(backend, text) {
  const command = commandOf(text);
  if (command === null || backend === "claude") return true;
  return backend === "codex" && (command === "/goal" || command === "/compact");
}
