//! OpenCode's sessions, read from its own database.
//!
//! OpenCode (1.19) keeps every session in one SQLite database,
//! `$XDG_DATA_HOME/opencode/opencode.db` (`~/.local/share/opencode` by
//! default), in WAL mode while it runs. Rudder opens it read-only. Rows of
//! `message` hold one user or assistant message each, its JSON in `data`
//! (role, tokens, time); rows of `part` hold a message's pieces (`type` text,
//! tool, reasoning, ...). cloud/worker/opencode.mjs reads the same tables for
//! the phone.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::PathBuf;

/// OpenCode's database, where OpenCode puts it.
pub(crate) fn database() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::cloudio::user_home_dir().map(|home| home.join(".local").join("share")))?;
    Some(data.join("opencode").join("opencode.db"))
}

fn open() -> Option<Connection> {
    let path = database().filter(|path| path.is_file())?;
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

/// The newest text the agent wrote in `session_id`: what a planner's last turn
/// said, when its pane lost the plan block.
pub(crate) fn final_assistant_text(session_id: &str) -> Option<String> {
    final_assistant_text_in(&open()?, session_id)
}

pub(crate) fn final_assistant_text_in(db: &Connection, session_id: &str) -> Option<String> {
    let data: String = db
        .query_row(
            "SELECT p.data FROM part p JOIN message m ON m.id = p.message_id
             WHERE p.session_id = ?1 AND json_extract(p.data, '$.type') = 'text'
               AND json_extract(m.data, '$.role') = 'assistant'
               AND json_extract(m.data, '$.summary') IS NOT 1
             ORDER BY p.id DESC LIMIT 1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .ok()??;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    let text = value.get("text")?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The tokens `session_id` used, as Rudder counts Claude's: input with the
/// cache reads and writes, output with the reasoning. (0, 0) without it.
pub(crate) fn token_usage(session_id: &str) -> (u64, u64) {
    open().map_or((0, 0), |db| token_usage_in(&db, session_id))
}

pub(crate) fn token_usage_in(db: &Connection, session_id: &str) -> (u64, u64) {
    db.query_row(
        "SELECT
           COALESCE(SUM(json_extract(data, '$.tokens.input') + json_extract(data, '$.tokens.cache.read')
             + json_extract(data, '$.tokens.cache.write')), 0),
           COALESCE(SUM(json_extract(data, '$.tokens.output') + json_extract(data, '$.tokens.reasoning')), 0)
         FROM message WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant'",
        [session_id],
        |row| Ok((row.get::<_, i64>(0)?.max(0) as u64, row.get::<_, i64>(1)?.max(0) as u64)),
    )
    .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenCode's own tables, as far as Rudder reads them, with one turn.
    fn fixture() -> Connection {
        let db = Connection::open_in_memory().expect("db");
        db.execute_batch(
            r#"
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);
            INSERT INTO message VALUES ('msg_1', 'ses_a', 1, '{"role":"user","time":{"created":1}}');
            INSERT INTO message VALUES ('msg_2', 'ses_a', 2, '{"role":"assistant","tokens":{"input":100,"output":20,"reasoning":5,"cache":{"read":1000,"write":10}}}');
            INSERT INTO message VALUES ('msg_3', 'ses_a', 3, '{"role":"assistant","tokens":{"input":50,"output":7,"reasoning":0,"cache":{"read":0,"write":0}}}');
            INSERT INTO message VALUES ('msg_4', 'ses_a', 4, '{"role":"assistant","summary":true,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}');
            INSERT INTO part VALUES ('prt_1', 'msg_1', 'ses_a', '{"type":"text","text":"plan it"}');
            INSERT INTO part VALUES ('prt_2', 'msg_2', 'ses_a', '{"type":"text","text":"first answer"}');
            INSERT INTO part VALUES ('prt_3', 'msg_3', 'ses_a', '{"type":"text","text":"RUDDER_PLAN_TASKS done"}');
            INSERT INTO part VALUES ('prt_4', 'msg_3', 'ses_a', '{"type":"step-finish","reason":"stop"}');
            INSERT INTO part VALUES ('prt_5', 'msg_4', 'ses_a', '{"type":"text","text":"Objective: a compaction summary"}');
            "#,
        )
        .expect("fixture");
        db
    }

    #[test]
    fn the_final_text_is_the_agents_newest_words_not_a_compaction_summary() {
        let db = fixture();
        assert_eq!(
            final_assistant_text_in(&db, "ses_a").as_deref(),
            Some("RUDDER_PLAN_TASKS done")
        );
        assert_eq!(final_assistant_text_in(&db, "ses_none"), None);
    }

    #[test]
    fn token_usage_counts_cache_as_input_and_reasoning_as_output() {
        let db = fixture();
        assert_eq!(token_usage_in(&db, "ses_a"), (100 + 1000 + 10 + 50, 20 + 5 + 7));
        assert_eq!(token_usage_in(&db, "ses_none"), (0, 0));
    }
}
