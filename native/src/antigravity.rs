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

/// A live Remote Control link printed by agy. Opening it lets Google show and
/// answer the current tool prompt using its own request identity. The terminal
/// status OSC only reports a wait and cannot safely route an approval.
pub(crate) fn remote_control_url(
    output: &str,
    conversation_id: &str,
    previous_url: Option<&str>,
) -> Option<String> {
    const DASHBOARD: &str = "https://antigravity.google.com";
    const ROUTE: &str = "https://antigravity.google.com/r/";
    const CONNECTED: &str = "Remote control on for this session.";
    const DISCONNECTED: &str = "Remote control off. This machine is no longer reachable";
    let (output, previous_url) = match output.rsplit_once(DISCONNECTED) {
        Some((_, remaining)) => (remaining, None),
        None => (output, previous_url),
    };
    let notice = output.rsplit_once(CONNECTED)
        .map(|(_, message)| super::strip_ansi_for_plan(message));
    let printed_url = notice.as_deref().and_then(|text| {
        let (_, open) = text.split_once("Open ")?;
        let (link, _) = open.split_once(" on another device")?;
        Some(link.trim())
    });
    let url = printed_url.or(previous_url)?;
    if url == DASHBOARD {
        return Some(url.to_string());
    }
    let (instance, conversation) = url.strip_prefix(ROUTE)?.split_once("?p=c%2F")?;
    (valid_conversation_id(conversation_id)
        && conversation == conversation_id
        && !instance.is_empty()
        && instance.len() <= 128
        && instance.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)))
        .then(|| url.to_string())
}

/// The folder agy keeps one conversation's working files in.
fn brain_dir(id: &str) -> Option<PathBuf> {
    valid_conversation_id(id).then(|| antigravity_cli_dir()).flatten().map(|dir| dir.join("brain").join(id))
}

/// The conversation's transcript, one JSON object per line. agy's
/// `transcript.jsonl` cuts each step at about 4 KB; `transcript_full.jsonl`
/// beside it has the same steps whole.
pub(crate) fn transcript_path(id: &str) -> Option<PathBuf> {
    brain_dir(id).map(|dir| dir.join(".system_generated").join("logs").join("transcript_full.jsonl"))
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

    // Shaped like agy's transcript_full.jsonl lines (step_index, source, type,
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
    fn remote_control_opens_only_the_live_conversation() {
        let conversation = "7f0c2a52-1b9e-4c55-9f43-2d0a8e6c1b10";
        let url = format!("https://antigravity.google.com/r/cli-42?p=c%2F{conversation}");
        let message = format!(
            "Remote control on for this session.\r\nHostname: devbox\r\nOpen \x1b[4m{url}\x1b[0m on another device to continue this conversation.\r\n"
        );
        assert_eq!(remote_control_url(&message, conversation, None), Some(url.clone()));
        assert_eq!(remote_control_url("later output", conversation, Some(&url)), Some(url.clone()));
        let startup = "Remote control on for this session.\nHostname: devbox\nOpen https://antigravity.google.com on another device and look for this hostname.";
        assert_eq!(remote_control_url(startup, conversation, None).as_deref(), Some("https://antigravity.google.com"));
        assert_eq!(remote_control_url(&message, "ab", Some(&url)), None);
        assert_eq!(remote_control_url(&format!("{message}Remote control off. This machine is no longer reachable"), conversation, Some(&url)), None);
        for bad in [
            url.replace(".com/", ".com.evil/"),
            url.replace("cli-42?", "../cli-42?"),
            format!("{url}&p=elsewhere"),
        ] {
            assert_eq!(remote_control_url("", conversation, Some(&bad)), None);
        }
    }

    #[test]
    fn only_conversation_ids_reach_a_path() {
        assert!(valid_conversation_id("7f0c2a52-1b9e-4c55-9f43-2d0a8e6c1b10"));
        assert!(!valid_conversation_id("../../etc"));
        assert!(!valid_conversation_id(""));
    }
}
