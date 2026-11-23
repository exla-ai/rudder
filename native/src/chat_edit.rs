//! Edit a sent message: the Edit action on any message in a chat that started
//! a turn. The agent's conversation goes back to just before that message
//! and the edited text runs as the new prompt, so the agent no longer has the
//! turns after it in context. Files the agent changed stay as they are
//! (Rudder keeps no per-turn workspace snapshot to restore), and the old
//! conversation stays on disk untouched:
//!
//! - Claude: the conversation before the edited prompt (the session file up
//!   to it, without branches gone back from) is copied to a new session file
//!   T, and Claude relaunches with `--resume T -- <text>`. Claude carries on
//!   in T under T's id, so the chat shows the kept history the moment it
//!   relaunches.
//! - Codex: a one-shot `codex app-server` forks the thread through the turn
//!   before the edited prompt's turn (`thread/fork`), and Codex relaunches
//!   with `resume N -- <text>`. The fork's rollout refers back to the old one
//!   (`history_base`) instead of copying it, so an old rollout must never be
//!   deleted or moved.
//!
//! Nothing comes before the first message, so editing it starts the
//! conversation over in the same agent and workspace: Claude relaunches on a
//! new session id (`--session-id T`), Codex on a new, empty thread the
//! app-server makes (`thread/start`). The first prompt is the edited text in
//! the framing the agent launched with (`launch_prompt`), and it becomes the
//! agent's task.
//!
//! An edit sent while the agent is mid-turn first interrupts that turn with
//! Esc, as the app's Interrupt button does, and rewinds once the agent is
//! idle (`drain_chat_edits`).
//!
//! The steer request is `{"kind": "edit", "instruction": "<entry> <latest>\n<text>"}`:
//! `entry` is the edited prompt's id in the session file (Claude's entry
//! uuid, Codex's message id) and `latest` the id of the chat's latest prompt
//! when it was shown. A rewind is refused when a newer prompt has arrived
//! since, so a stale chat view cannot drop turns nobody saw. The file copy
//! and the app-server calls run on a thread; the App loop relaunches the PTY
//! when it reports back (`drain_chat_edits`) and writes the request's receipt
//! then.
use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

const STALE_REFUSAL: &str = "This chat has new messages since it was shown. Reload the chat and try again";
const GONE_REFUSAL: &str = "That message is no longer in this chat. Reload the chat and try again";
const SENT_MID_TURN_REFUSAL: &str =
    "A message sent while the agent was working can't be edited; edit the one that started that turn";
const CODEX_APP_SERVER_DEADLINE: Duration = Duration::from_secs(30);
/// How long an edit sent mid-turn waits for the turn it interrupted to stop.
const TURN_STOP_DEADLINE: Duration = Duration::from_secs(20);
/// The note Claude records as a user message when a turn is interrupted:
/// "[Request interrupted by user]", or "... for tool use]".
const CLAUDE_INTERRUPT_NOTE: &str = "[Request interrupted by user";

/// A checked edit request, ready to rewind.
pub(crate) struct ChatEdit {
    run_id: String,
    request_id: String,
    backend: Backend,
    cwd: PathBuf,
    model: String,
    /// The conversation it rewinds. The row must still be on it at the end.
    session: String,
    entry: String,
    latest: String,
    text: String,
}

/// An edit sent mid-turn, waiting for the turn it interrupted to stop.
pub(crate) struct WaitingChatEdit {
    edit: ChatEdit,
    until: Instant,
}

/// A finished rewind, reported back from its thread.
pub(crate) struct ChatEditOutcome {
    pub(crate) run_id: String,
    pub(crate) request_id: String,
    /// The session the rewind started from. The row must still be on it.
    pub(crate) source_session: String,
    pub(crate) text: String,
    pub(crate) result: Result<Rewound, String>,
}

pub(crate) struct Rewound {
    /// The conversation the agent relaunches on: a cut copy (Claude) or fork
    /// (Codex) of the old one, or a new one.
    session: String,
    /// The first message was edited: `session` is a new, empty conversation
    /// the edited text starts.
    over: bool,
}

// ---------------------------------------------------------------------------
// Claude: a byte-prefix copy of the session file.
// ---------------------------------------------------------------------------

fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn flag(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// Text a person typed, as the chat shows it (cloud/worker/transcripts.mjs
/// cleanUserText): harness-injected turns (command output, reminders,
/// notifications) are not the user. Neither is the note Claude records when
/// a turn is interrupted: the chat shows it, but it starts no turn, so an
/// edit that interrupts a turn still names that turn's prompt as the latest
/// (session-lineage.mjs typedText).
fn typed_text(text: &str) -> bool {
    const HARNESS: [&str; 6] = [
        "<local-command",
        "<command-",
        "<system-reminder",
        "<bash-",
        "<user-memory",
        "<task-notification",
    ];
    let text = text.trim();
    !text.is_empty()
        && !HARNESS.iter().any(|prefix| text.starts_with(prefix))
        && !text.starts_with(CLAUDE_INTERRUPT_NOTE)
}

fn claude_typed_prompt(entry: &serde_json::Value) -> bool {
    if str_field(entry, "type") != Some("user")
        || flag(entry, "isSidechain")
        || flag(entry, "isMeta")
        || flag(entry, "isCompactSummary")
    {
        return false;
    }
    match entry.pointer("/message/content") {
        Some(serde_json::Value::String(text)) => typed_text(text),
        Some(serde_json::Value::Array(blocks)) => {
            !blocks.iter().any(|block| str_field(block, "type") == Some("tool_result"))
                && blocks.iter().any(|block| {
                    str_field(block, "type") == Some("text")
                        && str_field(block, "text").is_some_and(typed_text)
                })
        }
        _ => false,
    }
}

/// A message sent while a turn runs follows a tool call or its result; one
/// that starts a turn follows the end of the previous turn.
fn claude_mid_turn(parent: &serde_json::Value) -> bool {
    let inside = match str_field(parent, "type") {
        Some("user") => "tool_result",
        Some("assistant") => "tool_use",
        _ => return false,
    };
    parent
        .pointer("/message/content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(|block| str_field(block, "type") == Some(inside)))
}

/// The lines of a Claude session file that come before the turn-starting
/// prompt `entry`, without the branches gone back from: the conversation
/// that prompt was sent in, as the chat shows it (activeClaudeLines in
/// cloud/worker/session-lineage.mjs). Off the prompt's own history, a branch
/// that starts with a prompt is one the person went back from (Claude's
/// rewind in the pane) and is left out; any other branch (a parallel tool
/// call's result) stays. The latest turn-starting prompt must still be
/// `latest`. A file-history-snapshot written for the edited prompt just
/// before it goes too. The copy must end exactly at the prompt's parent, so
/// resuming it continues that conversation; anything else is refused.
/// `None`: `entry` is the conversation's first prompt, with nothing before it
/// to keep.
pub(crate) fn claude_rewind_copy(source: &[u8], entry: &str, latest: &str) -> Result<Option<Vec<u8>>, String> {
    let mut lines = Vec::new();
    let mut values = Vec::new();
    for piece in source.split_inclusive(|byte| *byte == b'\n') {
        lines.push(piece);
        let line = piece.strip_suffix(b"\n").unwrap_or(piece);
        values.push(serde_json::from_slice::<serde_json::Value>(line).ok());
    }
    let uuid_at = |index: usize| values[index].as_ref().and_then(|value| str_field(value, "uuid"));
    let sidechain = |index: usize| values[index].as_ref().is_some_and(|value| flag(value, "isSidechain"));
    let parent_at = |index: usize| values[index].as_ref().and_then(|value| str_field(value, "parentUuid"));
    let mut by_uuid = HashMap::new();
    for index in 0..values.len() {
        if let Some(uuid) = uuid_at(index) {
            by_uuid.insert(uuid, index);
        }
    }
    let prompts: Vec<usize> = (0..values.len())
        .filter(|&index| {
            let Some(value) = values[index].as_ref().filter(|value| claude_typed_prompt(value)) else {
                return false;
            };
            let parent = str_field(value, "parentUuid")
                .and_then(|parent| by_uuid.get(parent))
                .and_then(|&parent| values[parent].as_ref());
            !parent.is_some_and(claude_mid_turn)
        })
        .collect();
    let Some(&newest) = prompts.last() else {
        return Err("This chat has no message to edit".to_string());
    };
    if uuid_at(newest) != Some(latest) {
        return Err(STALE_REFUSAL.to_string());
    }
    let Some(position) = prompts.iter().position(|&index| uuid_at(index) == Some(entry)) else {
        let typed = by_uuid
            .get(entry)
            .and_then(|&index| values[index].as_ref())
            .is_some_and(claude_typed_prompt);
        return Err(if typed { SENT_MID_TURN_REFUSAL } else { GONE_REFUSAL }.to_string());
    };
    if position == 0 {
        return Ok(None);
    }
    let edited = prompts[position];
    let mut cut = edited;
    let snapshot_for_prompt = cut > 0
        && values[cut - 1].as_ref().is_some_and(|value| {
            str_field(value, "type") == Some("file-history-snapshot")
                && str_field(value, "messageId") == Some(entry)
        });
    if snapshot_for_prompt {
        cut -= 1;
    }
    let branched = || "This chat's history branches just before that message, so it can't be rewound there".to_string();
    // No history before it: the first prompt of a conversation the person
    // went all the way back from.
    let Some(parent_uuid) = parent_at(edited) else {
        return Ok(None);
    };
    let parent = by_uuid.get(parent_uuid).copied().filter(|&parent| parent < cut).ok_or_else(branched)?;
    // The prompt's history: from its parent back to the start. A compact
    // boundary starts a new chain, so the history goes on from the entry
    // written just before it.
    let mut on_path = HashSet::new();
    let mut next = Some(parent);
    while let Some(index) = next {
        if !on_path.insert(index) {
            break;
        }
        next = match parent_at(index) {
            Some(parent) => by_uuid.get(parent).copied(),
            None if values[index].as_ref().is_some_and(|value| {
                str_field(value, "type") == Some("system") && str_field(value, "subtype") == Some("compact_boundary")
            }) =>
            {
                (0..index).rev().find(|&earlier| uuid_at(earlier).is_some() && !sidechain(earlier))
            }
            None => None,
        };
    }
    // Whether an entry off that history is on a branch that starts with a
    // prompt. Walked once per branch.
    let mut verdicts: HashMap<usize, bool> = HashMap::new();
    let mut gone_back_from = |start: usize| {
        let mut walked = Vec::new();
        let mut index = start;
        let verdict = loop {
            if let Some(&known) = verdicts.get(&index) {
                break known;
            }
            walked.push(index);
            match parent_at(index).and_then(|parent| by_uuid.get(parent).copied()) {
                Some(parent) if on_path.contains(&parent) => {
                    break values[index].as_ref().is_some_and(claude_typed_prompt);
                }
                Some(parent) if !walked.contains(&parent) => index = parent,
                // The start of the file, or a loop.
                _ => break false,
            }
        };
        for seen in walked {
            verdicts.insert(seen, verdict);
        }
        verdict
    };
    let mut copy = Vec::with_capacity(source.len());
    let mut last_kept = None;
    for index in 0..cut {
        if uuid_at(index).is_some() && !sidechain(index) {
            if !on_path.contains(&index) && gone_back_from(index) {
                continue;
            }
            last_kept = Some(index);
        }
        copy.extend_from_slice(lines[index]);
    }
    if last_kept != Some(parent) {
        return Err(branched());
    }
    Ok(Some(copy))
}

fn claude_session_file(cwd: &Path, session: &str) -> Result<PathBuf, String> {
    let file = crate::agent_life::ConversationRoots::from_env()?
        .claude_projects
        .join(encode_claude_project_dir(cwd))
        .join(format!("{session}.jsonl"));
    if file.is_file() {
        Ok(file)
    } else {
        Err(format!("This chat's conversation file is missing: {}", file.display()))
    }
}

pub(crate) fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn rewind_claude(cwd: &Path, session: &str, entry: &str, latest: &str) -> Result<Rewound, String> {
    let source = claude_session_file(cwd, session)?;
    let bytes = std::fs::read(&source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    let copy = claude_rewind_copy(&bytes, entry, latest)?;
    let session = crate::gitio::new_rudder_session_id();
    match copy {
        Some(copy) => {
            write_private_new(&source.with_file_name(format!("{session}.jsonl")), &copy)?;
            Ok(Rewound { session, over: false })
        }
        // Claude writes the new session's file when it starts on it.
        None => Ok(Rewound { session, over: true }),
    }
}

// ---------------------------------------------------------------------------
// Codex: fork the thread through the turn before the edited prompt.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CodexEvent {
    /// `task_started`: a turn begins.
    Turn(String),
    /// A user message the chat shows (transcripts.mjs parseCodex).
    Prompt(Option<String>),
}

struct CodexRollout {
    /// `history_base`: the thread this one was forked from, and how many bytes
    /// of that thread's rollout came before the fork.
    base: Option<(String, u64)>,
    events: Vec<CodexEvent>,
}

pub(crate) fn codex_home() -> Result<PathBuf, String> {
    match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => user_home_dir()
            .map(|home| home.join(".codex"))
            .ok_or_else(|| "HOME is not set".to_string()),
    }
}

/// `<sessions>/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`.
pub(crate) fn codex_rollout_file(sessions: &Path, thread: &str) -> Result<PathBuf, String> {
    let suffix = format!("-{thread}.jsonl");
    let mut dirs = vec![(sessions.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if depth < 3 {
                if path.is_dir() {
                    dirs.push((path, depth + 1));
                }
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(&suffix) {
                return Ok(path);
            }
        }
    }
    Err(format!("Codex has no saved conversation {thread} in {}", sessions.display()))
}

fn codex_typed_prompt(payload: &serde_json::Value) -> bool {
    let text = payload
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| str_field(part, "text"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let text = text.trim();
    !text.is_empty()
        && !text.starts_with("# AGENTS.md instructions")
        && !["<environment_context", "<user_instructions", "<permissions", "<turn_aborted"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
}

fn read_codex_rollout(path: &Path, limit: Option<u64>) -> Result<CodexRollout, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let end = match limit {
        Some(limit) if limit as usize <= bytes.len() => limit as usize,
        Some(limit) => {
            return Err(format!(
                "{} is shorter than the {limit} bytes a later conversation builds on",
                path.display()
            ))
        }
        None => bytes.len(),
    };
    let mut rollout = CodexRollout { base: None, events: Vec::new() };
    let mut first = true;
    for line in bytes[..end].split(|byte| *byte == b'\n') {
        let is_first = std::mem::take(&mut first);
        // Most lines are tool output; parse only the kinds that matter here.
        let relevant = is_first
            || line.windows(12).any(|window| window == b"task_started")
            || line.windows(6).any(|window| window == b"\"user\"");
        if !relevant {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let payload = value.get("payload").cloned().unwrap_or_default();
        match (str_field(&value, "type"), str_field(&payload, "type")) {
            (Some("session_meta"), _) if is_first => {
                if let Some(base) = payload.get("history_base").filter(|base| !base.is_null()) {
                    let thread = str_field(base, "thread_id").filter(|id| valid_session_id(id));
                    let offset = base.get("end_byte_offset").and_then(serde_json::Value::as_u64);
                    let (Some(thread), Some(offset)) = (thread, offset) else {
                        return Err(format!("{} has an unreadable history_base", path.display()));
                    };
                    rollout.base = Some((thread.to_string(), offset));
                }
            }
            (Some("event_msg"), Some("task_started")) => {
                if let Some(turn) = str_field(&payload, "turn_id") {
                    rollout.events.push(CodexEvent::Turn(turn.to_string()));
                }
            }
            (Some("response_item"), Some("message"))
                if str_field(&payload, "role") == Some("user") && codex_typed_prompt(&payload) =>
            {
                rollout
                    .events
                    .push(CodexEvent::Prompt(str_field(&payload, "id").map(str::to_string)));
            }
            _ => {}
        }
    }
    Ok(rollout)
}

#[derive(Debug, PartialEq)]
enum CodexForkPoint {
    /// Fork through this turn: the one before the edited prompt's turn.
    Through(String),
    /// The edited prompt is not in these events: older history, or gone.
    NotFound,
    /// No turn before the edited prompt's turn in these events.
    NoTurnBefore,
    /// No prompt the chat shows at all (in these events).
    NoPrompt,
}

/// The turn to fork through: the one before the turn that `entry`, a
/// turn-starting prompt, started. The latest turn-starting prompt must be
/// `latest`.
fn codex_fork_point(events: &[CodexEvent], entry: &str, latest: &str) -> Result<CodexForkPoint, String> {
    let mut turn_at: Option<usize> = None;
    let mut prompted = false;
    // Turn-starting prompts: where their turn began in `events`, and their id.
    let mut starts: Vec<(Option<usize>, Option<&str>)> = Vec::new();
    let mut sent_mid_turn = false;
    for (index, event) in events.iter().enumerate() {
        match event {
            CodexEvent::Turn(_) => {
                turn_at = Some(index);
                prompted = false;
            }
            CodexEvent::Prompt(id) => {
                if !prompted {
                    starts.push((turn_at, id.as_deref()));
                } else if id.as_deref() == Some(entry) {
                    sent_mid_turn = true;
                }
                prompted = true;
            }
        }
    }
    let Some(&(_, newest)) = starts.last() else {
        return Ok(CodexForkPoint::NoPrompt);
    };
    match newest {
        Some(id) if id == latest => {}
        Some(_) => return Err(STALE_REFUSAL.to_string()),
        None => return Err("This Codex conversation does not record message ids, so it can't be rewound".to_string()),
    }
    if sent_mid_turn {
        return Err(SENT_MID_TURN_REFUSAL.to_string());
    }
    let Some(&(turn_at, _)) = starts.iter().find(|(_, id)| *id == Some(entry)) else {
        return Ok(CodexForkPoint::NotFound);
    };
    let Some(turn_at) = turn_at else {
        return Ok(CodexForkPoint::NoTurnBefore);
    };
    Ok(events[..turn_at]
        .iter()
        .rev()
        .find_map(|event| match event {
            CodexEvent::Turn(turn) => Some(CodexForkPoint::Through(turn.clone())),
            CodexEvent::Prompt(_) => None,
        })
        .unwrap_or(CodexForkPoint::NoTurnBefore))
}

/// The turn to fork through, reading back through `history_base` only as
/// far as the edited prompt needs. `None`: the edited prompt started the
/// conversation's first turn, so there is nothing before it to keep.
fn codex_turn_before_edit(sessions: &Path, thread: &str, entry: &str, latest: &str) -> Result<Option<String>, String> {
    let own = read_codex_rollout(&codex_rollout_file(sessions, thread)?, None)?;
    let mut events = own.events;
    let mut base = own.base;
    loop {
        let point = codex_fork_point(&events, entry, latest)?;
        if let CodexForkPoint::Through(turn) = point {
            return Ok(Some(turn));
        }
        let Some((older, offset)) = base.take() else {
            return match point {
                CodexForkPoint::NoTurnBefore => Ok(None),
                CodexForkPoint::NoPrompt => Err("This chat has no message to edit".to_string()),
                _ => Err(GONE_REFUSAL.to_string()),
            };
        };
        let older = read_codex_rollout(&codex_rollout_file(sessions, &older)?, Some(offset))?;
        events.splice(0..0, older.events);
        base = older.base;
    }
}

fn send_rpc(stdin: &mut std::process::ChildStdin, message: serde_json::Value) -> Result<(), String> {
    let mut line = message.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("codex app-server stopped taking input: {error}"))
}

fn await_rpc(
    lines: &mpsc::Receiver<std::io::Result<String>>,
    id: u64,
    method: &str,
    deadline: Instant,
) -> Result<serde_json::Value, String> {
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        let line = match lines.recv_timeout(wait) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(format!("could not read codex app-server output: {error}")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "codex app-server did not answer within {} seconds",
                    CODEX_APP_SERVER_DEADLINE.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("codex app-server stopped before it answered".to_string())
            }
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // Notifications and the server's own requests carry a method.
        if message.get("method").is_some() || message.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            let text = str_field(error, "message").unwrap_or("unknown error");
            return Err(format!("Codex refused {method}: {text}"));
        }
        return Ok(message.get("result").cloned().unwrap_or_default());
    }
}

/// Requests to one `codex app-server`, after its handshake.
struct CodexRpc {
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<std::io::Result<String>>,
    deadline: Instant,
    last_id: u64,
}

impl CodexRpc {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.last_id += 1;
        let id = self.last_id;
        send_rpc(&mut self.stdin, serde_json::json!({ "id": id, "method": method, "params": params }))?;
        await_rpc(&self.lines, id, method, self.deadline)
    }
}

fn codex_exchange(
    child: &mut Child,
    work: impl FnOnce(&mut CodexRpc) -> Result<String, String>,
) -> Result<String, String> {
    let stdin = child.stdin.take().ok_or_else(|| "codex app-server has no stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "codex app-server has no stdout".to_string())?;
    let (tx, lines) = mpsc::channel();
    thread::Builder::new()
        .name("rudder-codex-app-server".to_string())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("could not read codex app-server: {error}"))?;
    let mut rpc = CodexRpc { stdin, lines, deadline: Instant::now() + CODEX_APP_SERVER_DEADLINE, last_id: 0 };
    rpc.call(
        "initialize",
        serde_json::json!({
            "clientInfo": { "name": "rudder", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        }),
    )?;
    send_rpc(&mut rpc.stdin, serde_json::json!({ "method": "initialized" }))?;
    work(&mut rpc)
}

/// The thread a `thread/start` or `thread/fork` answered with, once Codex
/// has saved it to disk.
fn saved_thread(result: &serde_json::Value) -> Result<String, String> {
    let session = result
        .pointer("/thread/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| valid_session_id(id))
        .ok_or_else(|| "Codex answered without a thread id".to_string())?;
    let saved = result
        .pointer("/thread/path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| Path::new(path).is_file());
    if !saved {
        return Err(format!("Codex did not save the conversation {session}"));
    }
    Ok(session.to_string())
}

/// How much of the app-server's error output a failed call reports.
const CODEX_STDERR_TAIL: usize = 1500;

/// Read `stderr` to its end on a thread and send back its last
/// `CODEX_STDERR_TAIL` bytes.
fn codex_stderr_tail(stderr: std::process::ChildStderr) -> Result<mpsc::Receiver<String>, String> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("rudder-codex-app-server-stderr".to_string())
        .spawn(move || {
            let mut tail = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > 2 * CODEX_STDERR_TAIL {
                    let cut = tail.len() - CODEX_STDERR_TAIL;
                    let cut = (cut..=tail.len()).find(|at| tail.is_char_boundary(*at)).unwrap_or(tail.len());
                    tail.drain(..cut);
                }
            }
            let _ = tx.send(tail);
        })
        .map_err(|error| format!("could not read codex app-server errors: {error}"))?;
    Ok(rx)
}

/// `work` on a one-shot `codex app-server` (stdio JSON-RPC) with the same
/// CODEX_HOME the agent uses. No turn runs. `work` returns the id of a thread
/// it saved, recorded as a conversation Rudder made. A failure carries the
/// end of the app-server's error output.
fn codex_app_server(
    cwd: &Path,
    work: impl FnOnce(&mut CodexRpc) -> Result<String, String>,
) -> Result<String, String> {
    let mut child = Command::new(codex_program())
        .arg("app-server")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start codex app-server: {error}"))?;
    let stderr = child.stderr.take().ok_or_else(|| "codex app-server has no stderr".to_string());
    let result = stderr
        .and_then(codex_stderr_tail)
        .map(|tail| (tail, codex_exchange(&mut child, work)));
    // Its stdin is closed by now (dropped with the exchange): it exits on EOF.
    let stop_by = Instant::now() + Duration::from_secs(5);
    while Instant::now() < stop_by && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(50));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
    let (tail, result) = result?;
    match result {
        Ok(session) => {
            crate::gitio::note_rudder_session(&session);
            Ok(session)
        }
        Err(error) => match tail.recv_timeout(Duration::from_secs(1)) {
            Ok(tail) if !tail.trim().is_empty() => {
                Err(format!("{error}. codex app-server said: {}", tail.trim()))
            }
            _ => Err(error),
        },
    }
}

/// One `thread/fork` (`params`). Returns the new thread's id. Branching an
/// agent (branch.rs) forks this way too.
pub(crate) fn codex_fork(cwd: &Path, params: serde_json::Value) -> Result<String, String> {
    codex_app_server(cwd, |rpc| saved_thread(&rpc.call("thread/fork", params)?))
}

/// A new, empty thread in `cwd` on `model` (empty: Codex's default), named
/// `name` and saved, so `codex resume` starts on it.
fn codex_new_thread(cwd: &Path, model: &str, name: &str) -> Result<String, String> {
    codex_app_server(cwd, |rpc| {
        let mut params = serde_json::json!({ "cwd": cwd });
        if !model.trim().is_empty() {
            params["model"] = serde_json::json!(live_codex_model(model));
        }
        let started = rpc.call("thread/start", params)?;
        let thread = started
            .pointer("/thread/id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Codex answered thread/start without a thread id".to_string())?
            .to_string();
        // Codex writes a new thread to disk only once something is recorded
        // in it. Its name is the one record that adds nothing to the
        // conversation (measured on codex 0.170).
        rpc.call("thread/name/set", serde_json::json!({ "threadId": thread, "name": name }))?;
        saved_thread(&started)
    })
}

fn rewind_codex(cwd: &Path, session: &str, entry: &str, latest: &str, model: &str, text: &str) -> Result<Rewound, String> {
    let sessions = codex_home()?.join("sessions");
    match codex_turn_before_edit(&sessions, session, entry, latest)? {
        Some(last_turn) => {
            let forked = codex_fork(
                cwd,
                serde_json::json!({ "threadId": session, "lastTurnId": last_turn, "excludeTurns": true }),
            )?;
            Ok(Rewound { session: forked, over: false })
        }
        None => {
            let name = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or(text);
            let thread = codex_new_thread(cwd, model, &truncate_chars(name, 60))?;
            Ok(Rewound { session: thread, over: true })
        }
    }
}

// ---------------------------------------------------------------------------
// The App side: checks, the thread, and the relaunch.
// ---------------------------------------------------------------------------

/// Why a backend's chats can't be edited from outside, or None when they can.
/// Antigravity keeps its conversations in its own store with no command to
/// rewind one from outside; its pane has `/rewind`.
pub(crate) fn edit_unsupported(backend: Backend) -> Option<&'static str> {
    match backend {
        Backend::Claude | Backend::Codex => None,
        Backend::Opencode => Some("Editing a message works in Claude and Codex chats"),
        Backend::Antigravity => {
            Some("Antigravity can't rewind a chat from outside; type /rewind in the agent's pane")
        }
    }
}

fn edit_refusal(run: &AgentRun) -> Option<String> {
    if is_cloud_owned_agent(run) {
        return Some("This agent runs on another machine now".to_string());
    }
    if run.is_orchestrator() || run.gam.is_some() || run.plain_process {
        return Some("Messages in this agent can't be edited".to_string());
    }
    if run.status == AgentStatus::Merged {
        return Some("Its work is already merged; start a new agent instead".to_string());
    }
    if let Some(reason) = edit_unsupported(run.backend) {
        return Some(reason.to_string());
    }
    if let Some(profile) = run.execution_profile.as_deref().filter(|profile| *profile != "native") {
        return Some(format!("This agent runs on the {profile} profile, which can't rewind a conversation"));
    }
    None
}

/// A turn is running. A row whose last turn ended on an API error (Claude
/// `StopFailure`: a rate limit, a refusal) stays Running without needing
/// input, but no turn runs: Esc changes nothing there, so an edit that
/// waited for it gave up after `TURN_STOP_DEADLINE`.
fn mid_turn(run: &mut AgentRun) -> bool {
    let live = run.terminal.as_mut().is_some_and(|terminal| terminal.is_alive());
    live
        && run.status == AgentStatus::Running
        && !run.needs_user_input
        && !matches!(run.wait_signal, Some(WaitSignal::Error(_)))
}

fn valid_entry_id(entry: &str) -> bool {
    (1..=128).contains(&entry.len())
        && entry.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

impl App {
    /// Steer kind "edit". `None`: accepted; the rewind runs on a thread and
    /// the receipt is written when it finishes (`drain_chat_edits`).
    pub(crate) fn deliver_edit_request(
        &mut self,
        task_id: &str,
        instruction: &str,
        request_id: &str,
    ) -> Option<SteerDelivery> {
        match self.start_chat_edit(task_id, instruction, request_id) {
            Ok(()) => None,
            Err(reason) => {
                self.push_activity(format!("edit: {reason}"));
                Some(SteerDelivery::Failed(reason))
            }
        }
    }

    fn start_chat_edit(&mut self, task_id: &str, instruction: &str, request_id: &str) -> Result<(), String> {
        let expected = || "expected the message id and the latest message id, then the new text".to_string();
        let (ids, text) = instruction.split_once('\n').ok_or_else(expected)?;
        let mut ids = ids.split_whitespace();
        let (Some(entry), Some(latest), None) = (ids.next(), ids.next(), ids.next()) else {
            return Err(expected());
        };
        if let Some(bad) = [entry, latest].into_iter().find(|id| !valid_entry_id(id)) {
            return Err(format!("not a message id: {bad}"));
        }
        let (entry, latest) = (entry.to_string(), latest.to_string());
        let text = sanitize_steer_instruction(text);
        if text.is_empty() {
            return Err("The message is empty".to_string());
        }
        let index = self
            .agent_index_for_token(task_id)
            .ok_or_else(|| format!("target not found: {task_id}"))?;
        let run = &mut self.agents[index];
        if let Some(refusal) = edit_refusal(run) {
            return Err(refusal);
        }
        let session = run
            .session_id
            .clone()
            .filter(|id| valid_session_id(id))
            .ok_or_else(|| "This chat has no saved conversation to rewind".to_string())?;
        if !run.cwd.is_dir() {
            return Err(format!("Its workspace is gone: {}", run.cwd.display()));
        }
        if self.chat_edits_in_flight.contains(&run.id) {
            return Err("This chat is already going back to an earlier message".to_string());
        }
        let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
        let edit = ChatEdit {
            run_id: run.id.clone(),
            request_id: request_id.to_string(),
            backend: run.backend,
            cwd: run.cwd.clone(),
            model: run.model.clone(),
            session,
            entry,
            latest,
            text,
        };
        if mid_turn(run) {
            // Esc, as the app's Interrupt button sends it (deliver_keys_request).
            let esc = remote_key_bytes("esc").ok_or_else(|| "no bytes for the Esc key".to_string())?;
            let written = run
                .terminal
                .as_mut()
                .ok_or_else(|| "it has no terminal to interrupt".to_string())
                .and_then(|terminal| terminal.write_input(esc).map_err(|error| error.to_string()));
            if let Err(error) = written {
                return Err(format!("could not interrupt its turn: {error}"));
            }
            run.last_worker_input_at = Some(Instant::now());
            self.chat_edits_in_flight.insert(edit.run_id.clone());
            self.chat_edits_waiting.push(WaitingChatEdit { edit, until: Instant::now() + TURN_STOP_DEADLINE });
            self.push_activity(format!("{label}: stopping its turn to go back to an earlier message"));
            self.dirty = true;
            return Ok(());
        }
        self.chat_edits_in_flight.insert(edit.run_id.clone());
        self.spawn_chat_rewind(edit)?;
        self.push_activity(format!("{label}: going back to an earlier message"));
        Ok(())
    }

    /// The file copy, or the app-server calls, on a thread that reports to
    /// `drain_chat_edits`. The chat is in `chat_edits_in_flight` until then.
    fn spawn_chat_rewind(&mut self, edit: ChatEdit) -> Result<(), String> {
        let run_id = edit.run_id.clone();
        let tx = self.chat_edit_tx.clone();
        let spawned = thread::Builder::new().name("rudder-chat-edit".to_string()).spawn(move || {
            let ChatEdit { run_id, request_id, backend, cwd, model, session, entry, latest, text } = edit;
            let result = match backend {
                Backend::Claude => rewind_claude(&cwd, &session, &entry, &latest),
                Backend::Codex => rewind_codex(&cwd, &session, &entry, &latest, &model, &text),
                Backend::Opencode | Backend::Antigravity => {
                    Err(edit_unsupported(backend).unwrap_or_default().to_string())
                }
            };
            let _ = tx.send(ChatEditOutcome { run_id, request_id, source_session: session, text, result });
        });
        if let Err(error) = spawned {
            self.chat_edits_in_flight.remove(&run_id);
            return Err(format!("could not start the rewind: {error}"));
        }
        Ok(())
    }

    /// Edits sent mid-turn rewind once the turn they interrupted has stopped:
    /// the agent is idle, waiting on its composer, or its process ended.
    fn start_stopped_chat_edits(&mut self) {
        let mut index = 0;
        while index < self.chat_edits_waiting.len() {
            let waiting = &self.chat_edits_waiting[index];
            let run = self.agents.iter_mut().find(|run| run.id == waiting.edit.run_id);
            let ready = match run.map(|run| (mid_turn(run), run.node_id.clone().unwrap_or_else(|| run.id.clone()))) {
                None => Err("The agent was deleted while its turn stopped".to_string()),
                Some((false, label)) => Ok(label),
                Some(_) if Instant::now() >= waiting.until => Err(format!(
                    "Its turn did not stop within {} seconds; nothing changed",
                    TURN_STOP_DEADLINE.as_secs()
                )),
                Some(_) => {
                    index += 1;
                    continue;
                }
            };
            let WaitingChatEdit { edit, .. } = self.chat_edits_waiting.remove(index);
            let (run_id, request_id) = (edit.run_id.clone(), edit.request_id.clone());
            let started = ready.and_then(|label| {
                self.spawn_chat_rewind(edit)?;
                self.push_activity(format!("{label}: going back to an earlier message"));
                Ok(())
            });
            if let Err(reason) = started {
                self.chat_edits_in_flight.remove(&run_id);
                self.push_activity(format!("edit: {reason}"));
                let _ = self.write_steer_receipt(&request_id, &SteerDelivery::Failed(reason));
            }
        }
    }

    pub(crate) fn drain_chat_edits(&mut self) {
        self.start_stopped_chat_edits();
        while let Ok(outcome) = self.chat_edit_rx.try_recv() {
            self.chat_edits_in_flight.remove(&outcome.run_id);
            let request_id = outcome.request_id.clone();
            let result = match self.finish_chat_edit(outcome) {
                Ok(()) => SteerDelivery::Delivered,
                Err(reason) => {
                    self.push_activity(format!("edit: {reason}"));
                    SteerDelivery::Failed(reason)
                }
            };
            let _ = self.write_steer_receipt(&request_id, &result);
        }
    }

    /// Relaunch the agent on the rewound conversation with the edited text as
    /// its first turn, the way a model switch relaunches it on the same one.
    /// A conversation started over gets the edited text as its task, in the
    /// framing the agent launched with.
    fn finish_chat_edit(&mut self, outcome: ChatEditOutcome) -> Result<(), String> {
        let rewound = outcome.result?;
        let index = self
            .agents
            .iter()
            .position(|run| run.id == outcome.run_id)
            .ok_or_else(|| "The agent was deleted while its chat went back".to_string())?;
        let run = &mut self.agents[index];
        if mid_turn(run) {
            return Err("It started another turn while the chat went back; nothing changed".to_string());
        }
        if run.session_id.as_deref() != Some(outcome.source_session.as_str()) {
            return Err("The chat moved to another conversation meanwhile; nothing changed".to_string());
        }
        let backend = run.backend;
        let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
        let mut command = match (backend, rewound.over) {
            (Backend::Claude, false) => claude_resume_command(run, &rewound.session),
            (Backend::Claude, true) => claude_new_session_command(run, &rewound.session),
            (Backend::Codex, _) => codex_resume_command(run, &rewound.session),
            (Backend::Opencode | Backend::Antigravity, _) => {
                return Err(edit_unsupported(backend).unwrap_or_default().to_string())
            }
        };
        // Started over: the agent's task is the edited text, framed as the
        // old one was (an "Objective:" block for a task from the task bar).
        // Orchestrators are refused, so the orchestrator flags are off.
        let new_task = if rewound.over {
            let task = if starts_goal_prompt(&run.task) {
                manual_goal_prompt(&outcome.text)
            } else {
                outcome.text.clone()
            };
            Some(task)
        } else {
            None
        };
        // A discussion (route_fresh_task) launched with its scope as the first
        // turn, and it keeps that scope when it starts over.
        let discussion = run.turns.first().is_some_and(|turn| turn.prompt.starts_with(DISCUSSION_PROMPT_PREFIX));
        let first_turn = new_task.as_ref().map(|task| if discussion { discussion_task_prompt(task) } else { task.clone() });
        let prompt = match &first_turn {
            Some(task) => launch_prompt(backend, task, run.mode, false, false)
                .ok_or_else(|| "This agent has no first prompt to start over with".to_string())?,
            None => outcome.text.clone(),
        };
        let turn_prompt = first_turn.unwrap_or_else(|| outcome.text.clone());
        let wired = if run.is_main() && run.execution_profile.is_none() {
            signals::augment_worker_command(&mut command, backend, run.mode, &run.id, run.account.as_deref())
        } else {
            signals::augment_existing_worker_command(
                &mut command,
                backend,
                run.mode,
                &run.id,
                run.execution_profile.as_deref(),
                run.account.as_deref(),
            )
        };
        // Refused before the old process is touched: it keeps running.
        if let Err(error) = wired {
            return Err(format!("could not relaunch {label}: {error}"));
        }
        // The edited text is the first turn. After `--`, text that starts
        // with "-" (a list) is the prompt, not a flag.
        command.args.push("--".to_string());
        command.args.push(prompt);
        let options = TerminalPaneOptions {
            size: run.terminal_size.unwrap_or_default(),
            cwd: Some(run.cwd.clone()),
            ..TerminalPaneOptions::default()
        };
        // Dropping the pane ends the old process before the new one starts.
        run.terminal = None;
        let mut terminal = match TerminalPane::spawn_shell_or_command(Some(command), options) {
            Ok(terminal) => terminal,
            Err(error) => {
                let run = &mut self.agents[index];
                run.status = AgentStatus::Failed;
                run.last_error = Some(error.to_string());
                let _ = save_native_run_record(&self.cwd, run);
                return Err(format!("could not relaunch {label}: {error}"));
            }
        };
        let _ = terminal.drain_output();
        Self::attach_output_waker(&self.pty_output_waker, &terminal);
        let now = now_stamp();
        let run = &mut self.agents[index];
        run.execution_profile = terminal.execution_profile().map(str::to_string);
        run.terminal = Some(terminal);
        run.session_id = Some(rewound.session.clone());
        run.status = AgentStatus::Running;
        run.completed_at = None;
        run.last_output_at = Instant::now();
        run.needs_permission = false;
        run.needs_user_input = false;
        run.wait_signal = None;
        run.last_error = None;
        run.worker_exit_note = None;
        if let Some(task) = new_task {
            // Until the backend writes the new conversation, the chat shows
            // the prompts recorded here, and none of the old ones are in it.
            run.task = task;
            run.turns.clear();
        }
        run.current_prompt = turn_prompt.clone();
        run.turns.push(AgentTurn { ts: now.clone(), prompt: turn_prompt, source: "user".to_string() });
        run.last_user_input_at = now;
        run.last_worker_input_at = Some(Instant::now());
        // The agent runs on the edited message from here, so every record of
        // that is attempted and each one that failed is reported.
        let mut unrecorded = Vec::new();
        if let Err(error) = save_native_run_record(&self.cwd, run) {
            unrecorded.push(format!("its record was not saved: {error}"));
        }
        let went = if rewound.over { "started over from its first message" } else { "went back to an earlier message" };
        self.push_activity(format!("{label}: {went} and sent \"{}\"", steer_preview(&outcome.text)));
        self.mirror_graph();
        self.dirty = true;
        if backend == Backend::Claude {
            // Claude's hooks last reported the old session id, and the row
            // follows what they report (adopt_reported_sessions).
            let written = signals::claude_session_path(&outcome.run_id)
                .ok_or_else(|| "HOME is not set".to_string())
                .and_then(|file| {
                    let temp = file.with_extension(format!("claude-session.{}.tmp", std::process::id()));
                    std::fs::write(&temp, &rewound.session)
                        .and_then(|()| std::fs::rename(&temp, &file))
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = written {
                unrecorded.push(format!("its new session id was not recorded: {error}"));
            }
        }
        if unrecorded.is_empty() {
            Ok(())
        } else {
            Err(format!("it restarted on the edited message, but {}", unrecorded.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(lines: &[serde_json::Value]) -> Vec<u8> {
        lines.iter().map(|line| format!("{line}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn chat_edit_claude_cut_keeps_history_before_the_edited_prompt() {
        let prompt = |uuid: &str, parent: Option<&str>, text: &str| {
            serde_json::json!({ "type": "user", "uuid": uuid, "parentUuid": parent, "message": { "role": "user", "content": text } })
        };
        let reply = |uuid: &str, parent: &str| {
            serde_json::json!({ "type": "assistant", "uuid": uuid, "parentUuid": parent, "message": { "content": [{ "type": "text", "text": "ok" }] } })
        };
        let first = session(&[
            serde_json::json!({ "type": "file-history-snapshot", "messageId": "u1" }),
            prompt("u1", None, "first ask"),
            reply("a1", "u1"),
            serde_json::json!({ "type": "system", "subtype": "turn_duration", "uuid": "s1", "parentUuid": "a1" }),
        ]);
        let tool_turn = session(&[
            serde_json::json!({ "type": "assistant", "uuid": "a2", "parentUuid": "s1", "message": { "content": [{ "type": "tool_use", "id": "t1" }] } }),
            serde_json::json!({ "type": "user", "uuid": "r1", "parentUuid": "a2", "message": { "content": [{ "type": "tool_result", "tool_use_id": "t1" }] } }),
            // Typed while the turn ran: not turn-starting, not editable.
            serde_json::json!({ "type": "user", "uuid": "q1", "parentUuid": "r1", "message": { "content": [{ "type": "text", "text": "also this" }] } }),
            reply("a3", "q1"),
            serde_json::json!({ "type": "system", "subtype": "turn_duration", "uuid": "s2", "parentUuid": "a3" }),
        ]);
        let snapshot = session(&[serde_json::json!({ "type": "file-history-snapshot", "messageId": "u2" })]);
        let edited = session(&[prompt("u2", Some("s2"), "second ask"), reply("a4", "u2")]);
        let later = session(&[
            serde_json::json!({ "type": "file-history-snapshot", "messageId": "u3" }),
            prompt("u3", Some("a4"), "third ask"),
            reply("a5", "u3"),
        ]);
        let file = [first.clone(), tool_turn.clone(), snapshot.clone(), edited.clone()].concat();

        // The cut drops the edited prompt, its file-history-snapshot and all after.
        let kept = claude_rewind_copy(&file, "u2", "u2").unwrap();
        assert_eq!(kept, Some([first.clone(), tool_turn.clone()].concat()));

        // Without a snapshot for it, the cut is at the prompt itself.
        let plain = [first.clone(), tool_turn.clone(), session(&[prompt("u2", Some("s2"), "second ask")])].concat();
        assert_eq!(claude_rewind_copy(&plain, "u2", "u2").unwrap(), Some([first.clone(), tool_turn.clone()].concat()));

        // An earlier prompt: the turns after it go too.
        let longer = [file.clone(), later.clone()].concat();
        assert_eq!(claude_rewind_copy(&longer, "u2", "u3").unwrap(), Some([first.clone(), tool_turn.clone()].concat()));
        assert_eq!(claude_rewind_copy(&longer, "u3", "u3").unwrap(), Some(file.clone()));

        // A newer prompt than the chat showed: nothing is cut.
        assert!(matches!(claude_rewind_copy(&longer, "u2", "u2"), Err(reason) if reason == STALE_REFUSAL));

        // A message sent mid-turn, or one not in the file, can't be edited.
        assert!(matches!(claude_rewind_copy(&longer, "q1", "u3"), Err(reason) if reason == SENT_MID_TURN_REFUSAL));
        assert!(matches!(claude_rewind_copy(&longer, "gone", "u3"), Err(reason) if reason == GONE_REFUSAL));

        // The first message has nothing before it: the conversation starts over.
        assert_eq!(claude_rewind_copy(&longer, "u1", "u3"), Ok(None));

        // The note Claude writes when a turn is interrupted starts no turn, so
        // an edit that interrupted a turn still names that turn's prompt.
        let interrupted = [
            longer.clone(),
            session(&[serde_json::json!({ "type": "user", "uuid": "i1", "parentUuid": "a5", "message": { "role": "user", "content": [{ "type": "text", "text": "[Request interrupted by user]" }] } })]),
        ]
        .concat();
        assert_eq!(claude_rewind_copy(&interrupted, "u3", "u3").unwrap(), Some(file.clone()));
        assert_eq!(claude_rewind_copy(&interrupted, "u1", "u3"), Ok(None));

        // Claude's own rewind in the pane: the prompt sent in u2's place hangs
        // off u2's parent, after the turn gone back from. That turn stays out
        // of the copy (its snapshot line is not part of any conversation).
        let again = session(&[
            serde_json::json!({ "type": "file-history-snapshot", "messageId": "u2b" }),
            prompt("u2b", Some("s2"), "second ask, again"),
            reply("a6", "u2b"),
        ]);
        let rewound = [file.clone(), again.clone(), session(&[prompt("u3b", Some("a6"), "third ask")])].concat();
        let before_u2 = [first.clone(), tool_turn.clone(), snapshot.clone()].concat();
        assert_eq!(claude_rewind_copy(&rewound, "u2b", "u3b").unwrap(), Some(before_u2.clone()));
        assert_eq!(claude_rewind_copy(&rewound, "u3b", "u3b").unwrap(), Some([before_u2, again].concat()));

        // A prompt whose parent is not the last entry kept is on a branch.
        let branched = [first, tool_turn, session(&[prompt("u2", Some("a1"), "second ask")])].concat();
        assert!(matches!(claude_rewind_copy(&branched, "u2", "u2"), Err(reason) if reason.contains("branches")));
    }

    #[test]
    fn chat_edit_codex_forks_through_the_turn_before_the_edited_prompt() {
        let turn = |id: &str| CodexEvent::Turn(id.to_string());
        let ask = |id: &str| CodexEvent::Prompt(Some(id.to_string()));
        // Turn 1 starts with the first ask; m3 was sent while turn 2 ran.
        let events = vec![turn("t1"), ask("m1"), turn("t2"), ask("m2"), ask("m3"), turn("t3"), ask("m4"), turn("t4"), ask("m5")];
        let point = |entry: &str, latest: &str| codex_fork_point(&events, entry, latest);
        assert_eq!(point("m5", "m5"), Ok(CodexForkPoint::Through("t3".to_string())));
        assert_eq!(point("m4", "m5"), Ok(CodexForkPoint::Through("t2".to_string())));
        assert_eq!(point("m2", "m5"), Ok(CodexForkPoint::Through("t1".to_string())));
        assert_eq!(point("m1", "m5"), Ok(CodexForkPoint::NoTurnBefore));
        assert_eq!(point("m2", "m4"), Err(STALE_REFUSAL.to_string()));
        assert_eq!(point("m3", "m5"), Err(SENT_MID_TURN_REFUSAL.to_string()));
        // Not in these events: older history (history_base), or gone.
        assert_eq!(point("m0", "m5"), Ok(CodexForkPoint::NotFound));
        assert_eq!(codex_fork_point(&[turn("t1")], "m1", "m1"), Ok(CodexForkPoint::NoPrompt));
    }
}
