//! What Rudder reads from the Antigravity CLI's (`agy`) own files: whether a
//! conversation exists, its final reply and its token counts. Everything lives
//! under `~/.gemini/antigravity-cli/`; Rudder never writes there except the
//! trusted-folder entry (agent_trust.rs) and its hook entry (signals.rs).

use std::fs;
use std::path::PathBuf;

/// `~/.gemini/antigravity-cli`, the CLI's data folder.
pub(crate) fn antigravity_cli_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".gemini").join("antigravity-cli"))
}

/// A conversation id as agy prints it: a UUID. Anything else never reaches a
/// path join.
fn valid_conversation_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

/// The folder agy keeps one conversation's working files in.
fn brain_dir(id: &str) -> Option<PathBuf> {
    valid_conversation_id(id).then(|| antigravity_cli_dir()).flatten().map(|dir| dir.join("brain").join(id))
}

/// The conversation's transcript, one JSON object per line.
pub(crate) fn transcript_path(id: &str) -> Option<PathBuf> {
    brain_dir(id).map(|dir| dir.join(".system_generated").join("logs").join("transcript.jsonl"))
}

/// True when agy has this conversation on disk, so `--conversation` can
/// reopen it.
pub(crate) fn antigravity_session_exists(id: &str) -> bool {
    brain_dir(id).is_some_and(|dir| dir.is_dir())
}

/// Every conversation id on disk that starts with `partial`, for `/restore`
/// with an id cut short by a narrow pane.
pub(crate) fn conversation_ids_starting_with(partial: &str) -> Vec<String> {
    let Some(brain) = antigravity_cli_dir().map(|dir| dir.join("brain")) else {
        return Vec::new();
    };
    fs::read_dir(brain)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|id| valid_conversation_id(id) && id.starts_with(partial))
        .collect()
}

/// The last reply the model wrote in this conversation.
pub(crate) fn final_assistant_text(id: &str) -> Option<String> {
    let raw = fs::read_to_string(transcript_path(id)?).ok()?;
    transcript_final_text(&raw)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
}

/// Input and output tokens over the whole conversation.
pub(crate) fn token_usage(id: &str) -> TokenUsage {
    transcript_path(id)
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|raw| transcript_usage(&raw))
        .unwrap_or_default()
}

/// The transcript's steps, one JSON object per line:
/// `{"step_index":7,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE",
/// "model":"…","input_tokens":…,"cache_read_tokens":…,"output_tokens":…,"content":"…"}`.
/// A line that does not parse (one being written) is skipped.
fn steps(raw: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
}

fn is_model_reply(step: &serde_json::Value) -> bool {
    step.get("type").and_then(serde_json::Value::as_str) == Some("PLANNER_RESPONSE")
}

/// The last model reply in a transcript.
pub(crate) fn transcript_final_text(raw: &str) -> Option<String> {
    steps(raw)
        .filter(is_model_reply)
        .filter_map(|step| {
            step.get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .last()
}

/// Summed token counts in a transcript. Input counts cache reads too, the way
/// Rudder counts Claude's.
pub(crate) fn transcript_usage(raw: &str) -> TokenUsage {
    let count = |step: &serde_json::Value, key: &str| step.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    steps(raw).filter(is_model_reply).fold(TokenUsage::default(), |total, step| TokenUsage {
        input: total.input + count(&step, "input_tokens") + count(&step, "cache_read_tokens"),
        output: total.output + count(&step, "output_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped like agy's transcript.jsonl lines (step_index, source, type,
    // status, model, token counts, content).
    const TRANSCRIPT: &str = concat!(
        r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T05:10:00Z","content":"say pong"}"#, "\n",
        r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","model":"gemini-3.8-flash","input_tokens":2233,"cache_read_tokens":46659,"output_tokens":69,"content":"Running it."}"#, "\n",
        r#"{"step_index":3,"source":"MODEL","type":"RUN_COMMAND","status":"DONE"}"#, "\n",
        r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","model":"gemini-3.8-flash","input_tokens":2344,"cache_read_tokens":0,"output_tokens":5,"content":"PONG"}"#, "\n",
        r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESP"#,
    );

    #[test]
    fn the_last_model_reply_is_the_final_text() {
        assert_eq!(transcript_final_text(TRANSCRIPT).as_deref(), Some("PONG"));
        assert_eq!(transcript_final_text(""), None);
    }

    #[test]
    fn usage_sums_model_replies_with_cache_reads_as_input() {
        assert_eq!(transcript_usage(TRANSCRIPT), TokenUsage { input: 2233 + 46659 + 2344, output: 74 });
    }

    #[test]
    fn only_conversation_ids_reach_a_path() {
        assert!(valid_conversation_id("7f0c2a52-1b9e-4c55-9f43-2d0a8e6c1b10"));
        assert!(!valid_conversation_id("../../etc"));
        assert!(!valid_conversation_id(""));
    }
}
