// OpenCode's sessions, read from its own database.
//
// OpenCode (1.19) keeps every session in one SQLite database,
// $XDG_DATA_HOME/opencode/opencode.db (~/.local/share/opencode by default):
//
//   session  id ("ses_…"), parent_id (set for a subagent's), directory, title
//   message  one row per user or assistant message, ordered by time_created
//            and id; `data` is its JSON: role, time.created/completed,
//            modelID/providerID, tokens {input, output, reasoning, cache
//            {read, write}}, finish ("stop", "tool-calls"), summary (true for
//            a compaction's summary), error
//   part     a message's pieces, ordered by id; `data` is its JSON with a
//            `type`: text, reasoning, tool {tool, state {status, input,
//            output, error, time}}, compaction, step-start, step-finish,
//            patch {files}, file
//
// The database is in WAL mode and OpenCode writes it while it runs; Rudder
// opens it read-only for each read, which sees the newest committed rows.
// The native dashboard reads the same tables (native/src/opencode.rs).

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

let sqlite = null;

/** OpenCode's database for `home`, where OpenCode puts it. */
export function opencodeDb(home) {
  const data = process.env.XDG_DATA_HOME || path.join(home, ".local", "share");
  return path.join(data, "opencode", "opencode.db");
}

/** An OpenCode session id: "ses_" and its letters and digits. */
export function isOpencodeSessionId(id) {
  return /^ses_[A-Za-z0-9]+$/.test(String(id || ""));
}

function withDb(home, read) {
  const file = opencodeDb(home);
  if (!fs.existsSync(file)) {
    return null;
  }
  // Loaded on first use: Node 22 warns once that node:sqlite is experimental.
  sqlite ??= createRequire(import.meta.url)("node:sqlite");
  const db = new sqlite.DatabaseSync(file, { readOnly: true });
  try {
    return read(db);
  } finally {
    db.close();
  }
}

/** Whether OpenCode has the session `id`. */
export function opencodeSessionExists(home, id) {
  if (!isOpencodeSessionId(id)) {
    return false;
  }
  return Boolean(withDb(home, (db) => db.prepare("SELECT 1 AS found FROM session WHERE id = ?").get(id)));
}

/** When session `id` last changed (ms): its newest part's or its own update. Null when OpenCode lacks it. */
export function opencodeUpdatedAt(home, id) {
  if (!isOpencodeSessionId(id)) {
    return null;
  }
  const row = withDb(home, (db) => db
    .prepare("SELECT MAX(t) AS at FROM (SELECT time_updated AS t FROM session WHERE id = ? UNION ALL SELECT MAX(time_updated) FROM part WHERE session_id = ?)")
    .get(id, id));
  const at = Number(row?.at);
  return Number.isFinite(at) && at > 0 ? at : null;
}

/** Direct children and their latest turn, including unfinished background tasks. */
export function opencodeChildSessions(home, parentId) {
  if (!isOpencodeSessionId(parentId)) {
    return [];
  }
  const children = withDb(home, (db) => db.prepare(`
    SELECT child.id, child.title, child.agent, child.time_created, child.time_updated,
      latest.data AS turn,
      (SELECT COUNT(*) FROM part WHERE session_id = child.id AND json_extract(data, '$.type') = 'tool') AS tool_calls,
      EXISTS (SELECT 1 FROM part WHERE message_id = latest.id AND json_extract(data, '$.type') = 'tool'
        AND json_extract(data, '$.metadata.providerExecuted') IS NOT 1
        AND (json_extract(data, '$.state.status') IS NOT 'error' OR json_extract(data, '$.state.metadata.interrupted') IS NOT 1)) AS has_tool_calls,
      (SELECT data FROM part WHERE message_id = latest.id
        AND json_extract(data, '$.type') IN ('text', 'tool') AND json_extract(data, '$.synthetic') IS NOT 1
        ORDER BY id DESC LIMIT 1) AS step
    FROM session child
    LEFT JOIN message latest ON latest.id = (
      SELECT m.id FROM message m WHERE m.session_id = child.id
        AND json_extract(m.data, '$.summary') IS NOT 1
        AND NOT EXISTS (SELECT 1 FROM part WHERE message_id = m.id AND json_extract(data, '$.type') = 'compaction')
      ORDER BY m.time_created DESC, m.id DESC LIMIT 1
    )
    WHERE child.parent_id = ?`).all(parentId));
  return children?.map((row) => ({ ...row, turn: JSON.parse(row.turn), step: JSON.parse(row.step) })) ?? [];
}

/** The user's first prompt, even when it has scrolled out of the activity. */
export function opencodeFirstPrompt(home, id) {
  const parts = withDb(home, (db) => db.prepare(`
    SELECT json_extract(data, '$.text') AS text FROM part
    WHERE message_id = (SELECT id FROM message WHERE session_id = ? AND json_extract(data, '$.role') = 'user' ORDER BY time_created, id LIMIT 1)
      AND json_extract(data, '$.type') = 'text' AND json_extract(data, '$.synthetic') IS NOT 1
    ORDER BY id`).all(id));
  return parts?.map((part) => part.text).join("\n").trim() ?? "";
}

/**
 * Session `id`'s messages from index `start` up to `end` (null: its end), or
 * without a `start` its last `limit` before `end`; oldest first, each
 * `{ id, data, parts }` with `data` and every part's JSON parsed. `start` is
 * the index of the first, `total` how many messages the session has. Null
 * when OpenCode has no database here yet.
 */
export function opencodeMessages(home, id, { start: from, end: to, limit }) {
  return withDb(home, (db) => {
    const total = Number(db.prepare("SELECT COUNT(*) AS n FROM message WHERE session_id = ?").get(id).n);
    if ((to ?? 0) > total || (from ?? 0) > (to ?? total)) {
      throw new Error("This chat's session is shorter than the page asked for. Reopen the chat.");
    }
    const end = to ?? total;
    const start = from ?? Math.max(0, end - limit);
    const rows = db
      .prepare("SELECT id, data FROM message WHERE session_id = ? ORDER BY time_created, id LIMIT ? OFFSET ?")
      .all(id, end - start, start);
    const messages = rows.map((row) => ({ id: row.id, data: JSON.parse(row.data), parts: [] }));
    if (messages.length) {
      const byId = new Map(messages.map((message) => [message.id, message]));
      const parts = db
        .prepare(`SELECT message_id, data FROM part WHERE session_id = ? AND message_id IN (${messages.map(() => "?").join(",")}) ORDER BY id`)
        .all(id, ...messages.map((message) => message.id));
      for (const part of parts) {
        byId.get(part.message_id).parts.push(JSON.parse(part.data));
      }
    }
    return { messages, start, total };
  });
}

/** The newest text the agent wrote in session `id`, or null. */
export function opencodeLastText(home, id) {
  return withDb(home, (db) => {
    const row = db
      .prepare(`SELECT p.data FROM part p JOIN message m ON m.id = p.message_id
        WHERE p.session_id = ? AND json_extract(p.data, '$.type') = 'text'
          AND json_extract(m.data, '$.role') = 'assistant' AND json_extract(m.data, '$.summary') IS NOT 1
        ORDER BY p.id DESC LIMIT 1`)
      .get(id);
    return row ? String(JSON.parse(row.data).text || "") : null;
  });
}

/**
 * The context window of OpenCode's model `provider`/`model`, from the model
 * list OpenCode caches (~/.cache/opencode/models.json, models.dev's); null
 * for a model it does not list there (a provider set up in its config).
 */
export function opencodeContextWindow(home, provider, model) {
  const file = path.join(process.env.XDG_CACHE_HOME || path.join(home, ".cache"), "opencode", "models.json");
  const windows = cachedWindows(file);
  return windows?.get(`${provider}/${model}`) ?? null;
}

let windowsCache = null; // { file, mtimeMs, size, windows }

function cachedWindows(file) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch (error) {
    if (error?.code === "ENOENT") {
      return null;
    }
    throw error;
  }
  if (windowsCache?.file === file && windowsCache.mtimeMs === stat.mtimeMs && windowsCache.size === stat.size) {
    return windowsCache.windows;
  }
  const windows = new Map();
  for (const [provider, entry] of Object.entries(JSON.parse(fs.readFileSync(file, "utf8")))) {
    for (const [id, model] of Object.entries(entry?.models ?? {})) {
      const context = Number(model?.limit?.context);
      if (Number.isInteger(context) && context > 0) {
        windows.set(`${provider}/${id}`, context);
      }
    }
  }
  windowsCache = { file, mtimeMs: stat.mtimeMs, size: stat.size, windows };
  return windows;
}
