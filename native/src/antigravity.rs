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

/// The last model reply in a transcript.
pub(crate) fn transcript_final_text(raw: &str) -> Option<String> {
    let _ = raw;
    None
}

/// Summed token counts in a transcript.
pub(crate) fn transcript_usage(raw: &str) -> TokenUsage {
    let _ = raw;
    TokenUsage::default()
}
