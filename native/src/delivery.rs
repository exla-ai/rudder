//! Whether a message sent to a Claude Code or Codex chat reached the agent.
//!
//! Writing to a pane proves nothing. A CLI that is still starting keeps what
//! it is sent in its input box and drops the Enter (Claude Code 2.1.330 and
//! Codex 0.170, measured), and the phone was told "delivered" for a message
//! that sat there until the next one was glued onto it. The agent's own
//! transcript is the proof: Claude Code records a user turn, or the queued
//! message while a turn runs, and Codex records a user message, each with the
//! text as it arrived. A message's receipt waits for that record
//! (`App::drive_deliveries`), and one the agent did not take fails with why.
//!
//! Slash commands are not checked: Codex records most of them nowhere, and
//! Claude records `/compact` only once the compaction is over.
//!
//! The transcripts are read on their own thread; the App loop only applies
//! the results (AGENTS.md: no file IO in the loop).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::{AgentStatus, Backend};

/// Why a request failed: the receipt's `code`, which the app reads. The
/// `error` next to it is copy and may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailCode {
    /// A menu or picker has the keyboard.
    MenuOpen,
    /// The agent waits for a permission decision.
    PermissionPrompt,
    /// Codex asks what to do about a usage limit.
    RateLimitPrompt,
    /// Codex asks whether to replace its goal (`codexGoalMenu`).
    GoalMenu,
    /// The input box already holds text, which the message would be glued onto.
    ComposerNotEmpty,
    /// An interrupt did not bring the agent back to its composer.
    TurnDidNotStop,
    /// Sent, but the agent never took it.
    NotSubmitted,
    /// The process it was sent to ended before it took it, or there is none.
    AgentExited,
    /// The chat was closed while the message waited.
    ChatClosed,
    /// No chat has that id.
    NotFound,
    /// Not acted on: the request is invalid or the chat cannot take it now.
    Refused,
}

impl FailCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MenuOpen => "menu_open",
            Self::PermissionPrompt => "permission_prompt",
            Self::RateLimitPrompt => "rate_limit_prompt",
            Self::GoalMenu => "goal_menu",
            Self::ComposerNotEmpty => "composer_not_empty",
            Self::TurnDidNotStop => "turn_did_not_stop",
            Self::NotSubmitted => "not_submitted",
            Self::AgentExited => "agent_exited",
            Self::ChatClosed => "chat_closed",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
        }
    }
}

/// How often a waiting message's transcript is read.
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// A message given at launch is the CLI's first turn, recorded within
/// seconds of the start even on a busy machine.
pub(crate) const LAUNCH_WAIT: Duration = Duration::from_secs(90);
/// How long an agent at rest may leave a typed message unrecorded. A running
/// turn records one at once (Claude queues it) or when it next calls the
/// model (Codex), and a turn that ends submits what it queued.
pub(crate) const IDLE_WAIT: Duration = Duration::from_secs(20);
/// The longest any message waits, a long tool call included.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long a new pane may take to draw its composer before typed input
/// stops waiting for it (`App::agent_is_starting`).
pub(crate) const STARTUP_WAIT: Duration = Duration::from_secs(30);
/// How long input waits for the pane to settle after a key Rudder sent (an
/// answer to a Codex dialog) or after the last input (a draft still being
/// cleared or typed), before a block there is a refusal.
pub(crate) const SETTLE_WAIT: Duration = Duration::from_secs(2);
/// The first read of a transcript takes this much of its end. Later reads
/// start where the last one stopped.
const FIRST_READ_BYTES: u64 = 4 << 20;
/// Transcript times come from the agent on this machine, rounded to the
/// millisecond.
const CLOCK_SLACK_MS: i64 = 1_000;

/// A message sent to a chat whose receipt waits for the agent to take it.
#[derive(Debug, Clone)]
pub(crate) struct SentMessage {
    pub(crate) run_id: String,
    /// The process it went to. Another process cannot take it.
    pub(crate) instance_id: String,
    pub(crate) text: String,
    /// When it was sent, in Unix milliseconds.
    pub(crate) sent_ms: i64,
    /// Given to the CLI at launch (a reopened conversation), not typed into
    /// a composer.
    pub(crate) launched: bool,
    /// The turn's source in the run record once the agent takes it.
    pub(crate) source: &'static str,
    /// For a launch: the status the row goes back to when the agent does not
    /// take it, so a turn that never ran does not show as working.
    pub(crate) resting: Option<AgentStatus>,
}

/// Whether a message to `backend` is checked against its transcript.
pub(crate) fn checked(backend: Backend, text: &str) -> bool {
    matches!(backend, Backend::Claude | Backend::Codex) && !text.trim_start().starts_with('/')
}

/// One message waiting for the agent to take it.
pub(crate) struct PendingDelivery {
    pub(crate) id: u64,
    /// The inbox request it answers, when it came from one.
    pub(crate) request_id: Option<String>,
    pub(crate) label: String,
    pub(crate) backend: Backend,
    pub(crate) cwd: PathBuf,
    pub(crate) message: SentMessage,
    pub(crate) sent: Instant,
    /// Since when the agent has been at rest (not Running).
    pub(crate) idle_since: Option<Instant>,
    /// Since when the process it went to has been gone.
    pub(crate) gone_since: Option<Instant>,
    /// The transcript, once found, and how far it has been read.
    pub(crate) path: Option<PathBuf>,
    pub(crate) offset: Option<u64>,
}

impl PendingDelivery {
    /// Why the agent did not take it, judged at `at` (the start of the read
    /// that did not find it), or None while it may still.
    pub(crate) fn not_taken(&self, at: Instant) -> Option<(FailCode, &'static str)> {
        let since = |mark: Option<Instant>| mark.map_or(Duration::ZERO, |mark| at.saturating_duration_since(mark));
        if self.gone_since.is_some_and(|gone| gone <= at) {
            return Some((FailCode::AgentExited, "the agent stopped before it took the message"));
        }
        if self.message.launched && at.saturating_duration_since(self.sent) >= LAUNCH_WAIT {
            return Some((FailCode::NotSubmitted, "the agent did not start a turn with it"));
        }
        if self.idle_since.is_some() && since(self.idle_since) >= IDLE_WAIT {
            return Some((
                FailCode::NotSubmitted,
                "the agent did not take it. The text may still be in its input box",
            ));
        }
        (at.saturating_duration_since(self.sent) >= MAX_WAIT)
            .then_some((FailCode::NotSubmitted, "the agent did not take it"))
    }
}

/// What a scan reads for one message.
pub(crate) struct ScanRequest {
    pub(crate) id: u64,
    pub(crate) backend: Backend,
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: Option<String>,
    pub(crate) text: String,
    pub(crate) sent_ms: i64,
    pub(crate) launched: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) offset: Option<u64>,
}

pub(crate) struct ScanResult {
    pub(crate) id: u64,
    pub(crate) started: Instant,
    pub(crate) taken: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) offset: Option<u64>,
}

/// Reads each message's transcript from where the last read stopped. Runs on
/// the scan thread.
pub(crate) fn scan(requests: Vec<ScanRequest>) -> Vec<ScanResult> {
    let started = Instant::now();
    let codex_sessions = crate::chat_edit::codex_home().ok().map(|home| home.join("sessions"));
    requests
        .into_iter()
        .map(|request| {
            let path = locate(&request, codex_sessions.as_deref());
            // A new transcript (the conversation moved) is read from its end.
            let offset = request.offset.filter(|_| path.is_some() && path == request.path);
            let read = path.as_deref().and_then(|path| read_from(path, offset));
            let taken = read.as_ref().is_some_and(|(lines, _)| {
                lines_take(request.backend, lines, &request.text, request.sent_ms, request.launched)
            });
            ScanResult {
                id: request.id,
                started,
                taken,
                offset: read.map(|(_, end)| end).or(offset),
                path,
            }
        })
        .collect()
}

fn locate(request: &ScanRequest, codex_sessions: Option<&Path>) -> Option<PathBuf> {
    let session = request.session_id.as_deref().filter(|id| !id.trim().is_empty())?;
    match request.backend {
        Backend::Claude => crate::claude_transcript_path(&request.cwd, session),
        Backend::Codex => crate::codex_rollout::rollout_file(codex_sessions?, session),
        Backend::Opencode | Backend::Antigravity => None,
    }
}

/// The complete lines from `offset` (or the last `FIRST_READ_BYTES`) to the
/// end, and where they end.
fn read_from(path: &Path, offset: Option<u64>) -> Option<(String, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = match offset {
        Some(at) if at <= len => at,
        _ => len.saturating_sub(FIRST_READ_BYTES),
    };
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes).ok()?;
    // A read from the middle starts mid-line.
    let begin = if start > 0 && offset != Some(start) {
        bytes.iter().position(|byte| *byte == b'\n').map_or(bytes.len(), |at| at + 1)
    } else {
        0
    };
    let end = bytes.iter().rposition(|byte| *byte == b'\n').map_or(begin, |at| at + 1).max(begin);
    Some((String::from_utf8_lossy(&bytes[begin..end]).into_owned(), start + end as u64))
}

/// Whether `lines` record `text`, sent at `sent_ms`. Typed text lands after
/// whatever was in the input box, so the recorded turn ends with it (a turn
/// glued onto an earlier stuck message was still taken). A launch prompt may
/// be framed by Rudder's instructions, so it only has to be in the turn.
pub(crate) fn lines_take(backend: Backend, lines: &str, text: &str, sent_ms: i64, launched: bool) -> bool {
    let key = match_key(text);
    !key.is_empty()
        && lines.lines().filter_map(|line| user_text(backend, line)).any(|(at, recorded)| {
            let recorded = match_key(&recorded);
            at + CLOCK_SLACK_MS >= sent_ms && (recorded.ends_with(&key) || (launched && recorded.contains(&key)))
        })
}

/// Text the way it is compared: whitespace and controls ignored, as the app
/// matches a sent bubble to the transcript (cloud/app/pending.js `sentKey`).
pub(crate) fn match_key(text: &str) -> String {
    text.split(|ch: char| ch.is_whitespace() || ch.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What one transcript line says arrived from the user, and when.
fn user_text(backend: Backend, line: &str) -> Option<(i64, String)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let text = match backend {
        Backend::Claude => claude_user_text(&value)?,
        Backend::Codex => codex_user_text(&value)?,
        Backend::Opencode | Backend::Antigravity => return None,
    };
    let at = crate::cloudio::parse_iso8601_millis(value.get("timestamp")?.as_str()?)?;
    Some((at, text))
}

/// A Claude Code user turn, or a message queued while a turn runs.
fn claude_user_text(value: &Value) -> Option<String> {
    match value.get("type")?.as_str()? {
        "queue-operation" if value.get("operation")?.as_str()? == "enqueue" => {
            value.get("content")?.as_str().map(str::to_string)
        }
        "user" if value.get("isSidechain").and_then(Value::as_bool) != Some(true) => {
            text_of(value.get("message")?.get("content")?, "text")
        }
        _ => None,
    }
}

/// A Codex user message: the item sent to the model, or the event an older
/// Codex wrote for it.
fn codex_user_text(value: &Value) -> Option<String> {
    let payload = value.get("payload")?;
    match (value.get("type")?.as_str()?, payload.get("type")?.as_str()?) {
        ("response_item", "message") if payload.get("role")?.as_str()? == "user" => {
            text_of(payload.get("content")?, "input_text")
        }
        ("event_msg", "user_message") => payload.get("message")?.as_str().map(str::to_string),
        _ => None,
    }
}

fn text_of(content: &Value, kind: &str) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some(kind))
                .filter_map(|block| block.get("text")?.as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

pub(crate) fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENT: i64 = 1_795_000_000_000;

    /// A transcript time, to the second as Claude Code and Codex write it
    /// (to the millisecond in fact; the second is the coarser case).
    fn at(ms: i64) -> String {
        crate::usage::system_time_to_iso(UNIX_EPOCH + Duration::from_millis(ms as u64))
    }

    fn claude_user(ms: i64, text: &str) -> String {
        serde_json::json!({"type": "user", "timestamp": at(ms), "message": {"role": "user", "content": text}}).to_string()
    }

    fn codex_user(ms: i64, text: &str) -> String {
        serde_json::json!({"timestamp": at(ms), "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}).to_string()
    }

    #[test]
    fn a_claude_turn_or_queued_message_takes_the_message_sent_before_it() {
        let turn = claude_user(SENT + 900, "reply with  the word\ncherry");
        assert!(lines_take(Backend::Claude, &turn, "reply with the word cherry", SENT, false));
        // Mid-turn, Claude Code queues it.
        let queued = serde_json::json!({"type": "queue-operation", "operation": "enqueue", "timestamp": at(SENT + 40), "content": "and the tests"}).to_string();
        assert!(lines_take(Backend::Claude, &queued, "and the tests", SENT, false));
        // Recorded before it was sent: an earlier message with the same words.
        let earlier = claude_user(SENT - 5_000, "and the tests");
        assert!(!lines_take(Backend::Claude, &earlier, "and the tests", SENT, false));
        // A tool result or a subagent's turn is not the user's.
        let tool = serde_json::json!({"type": "user", "timestamp": at(SENT + 10), "message": {"content": [{"type": "tool_result", "content": "and the tests"}]}}).to_string();
        let side = serde_json::json!({"type": "user", "isSidechain": true, "timestamp": at(SENT + 10), "message": {"content": "and the tests"}}).to_string();
        assert!(!lines_take(Backend::Claude, &format!("{tool}\n{side}"), "and the tests", SENT, false));
    }

    #[test]
    fn a_message_glued_onto_a_stuck_one_was_taken_and_the_stuck_one_was_not() {
        // 11-23 08:52: the re-goal text sat in the box and the next message
        // was submitted onto it.
        let glued = claude_user(SENT + 2_000, "what ist hat link are you donehello?");
        assert!(lines_take(Backend::Claude, &glued, "hello?", SENT, false));
        assert!(!lines_take(Backend::Claude, &glued, "what ist hat link are you done", SENT - 60_000, false));
    }

    #[test]
    fn a_launch_prompt_may_be_framed_and_a_codex_user_message_counts() {
        let framed = codex_user(SENT + 3_000, "Continue your task: x. New direction: fix the parser. Your prior edits are here.");
        assert!(lines_take(Backend::Codex, &framed, "fix the parser", SENT, true));
        assert!(!lines_take(Backend::Codex, &framed, "fix the parser", SENT, false));
        let event = serde_json::json!({"timestamp": at(SENT + 5), "type": "event_msg", "payload": {"type": "user_message", "message": "- reply with the word cherry"}}).to_string();
        assert!(lines_take(Backend::Codex, &event, "- reply with the word cherry", SENT, false));
        // Codex's own context items are not the message.
        let context = codex_user(SENT + 5, "<environment_context>cherry</environment_context>");
        assert!(!lines_take(Backend::Codex, &context, "cherry", SENT, false));
    }

    #[test]
    fn reads_continue_where_the_last_one_stopped() {
        let dir = std::env::temp_dir().join(format!("rudder-delivery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, format!("{}\n{{\"partial", claude_user(SENT, "one"))).unwrap();
        let (lines, end) = read_from(&path, None).unwrap();
        assert_eq!(lines.lines().count(), 1, "the unfinished line waits");
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, b"\":1}\n").unwrap();
        let (lines, next) = read_from(&path, Some(end)).unwrap();
        assert_eq!(lines, "{\"partial\":1}\n");
        assert!(next > end);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_message_fails_when_its_agent_rests_stops_or_never_starts_the_turn() {
        let now = Instant::now();
        let pending = |launched: bool| PendingDelivery {
            id: 1,
            request_id: Some("r".into()),
            label: "n0".into(),
            backend: Backend::Claude,
            cwd: PathBuf::from("/"),
            message: SentMessage {
                run_id: "a".into(),
                instance_id: "i".into(),
                text: "hi".into(),
                sent_ms: SENT,
                launched,
                source: "user",
                resting: None,
            },
            sent: now,
            idle_since: None,
            gone_since: None,
            path: None,
            offset: None,
        };
        let busy = pending(false);
        assert_eq!(busy.not_taken(now + Duration::from_secs(600)), None, "a long turn takes it later");
        let mut resting = pending(false);
        resting.idle_since = Some(now + Duration::from_secs(1));
        assert_eq!(resting.not_taken(now + Duration::from_secs(5)), None);
        assert!(resting
            .not_taken(now + Duration::from_secs(21))
            .is_some_and(|(code, why)| code == FailCode::NotSubmitted && why.contains("input box")));
        let mut gone = pending(false);
        gone.gone_since = Some(now + Duration::from_secs(2));
        assert_eq!(gone.not_taken(now + Duration::from_secs(1)), None, "a read from before it went");
        assert_eq!(gone.not_taken(now + Duration::from_secs(2)).map(|(code, _)| code), Some(FailCode::AgentExited));
        assert!(pending(true).not_taken(now + LAUNCH_WAIT).is_some());
        assert!(pending(false).not_taken(now + MAX_WAIT).is_some());
    }
}
