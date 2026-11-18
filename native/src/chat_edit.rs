//! Edit a sent message: the Edit action on a chat's latest prompt. The agent's
//! conversation goes back to just before that prompt and the edited text runs
//! as the new prompt. Files the agent changed stay as they are, and the old
//! conversation stays on disk untouched:
//!
//! - Claude: the session file up to (not including) the edited prompt is
//!   copied to a new session file T, and Claude relaunches with
//!   `--resume T --fork-session --session-id N -- <text>`.
//! - Codex: a one-shot `codex app-server` forks the thread through the turn
//!   before the edited prompt (`thread/fork`), and Codex relaunches with
//!   `resume N -- <text>`. The fork's rollout refers back to the old one
//!   (`history_base`) instead of copying it, so an old rollout must never be
//!   deleted or moved.
//!
//! The steer request is `{"kind": "edit", "instruction": "<entry>\n<text>"}`:
//! `entry` is the edited prompt's id in the session file (Claude's entry
//! uuid, Codex's message id), checked against the latest prompt so a stale
//! chat view cannot rewind past newer turns. The file copy and the
//! app-server call run on a thread; the App loop relaunches the PTY when it
//! reports back (`drain_chat_edits`) and writes the request's receipt then.
use super::*;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub(crate) const FIRST_MESSAGE_REFUSAL: &str =
    "The first message can't be edited; start a new agent instead";
const STALE_REFUSAL: &str =
    "That is no longer the latest message in this chat. Reload the chat and try again";
const MID_TURN_REFUSAL: &str = "It is in the middle of a turn; edit when it finishes";
const CODEX_FORK_DEADLINE: Duration = Duration::from_secs(30);

/// A finished rewind, reported back from its thread.
pub(crate) struct ChatEditOutcome {
    pub(crate) run_id: String,
    pub(crate) request_id: String,
    /// The session the rewind started from. The row must still be on it.
    pub(crate) source_session: String,
    pub(crate) text: String,
    pub(crate) result: Result<Rewound, String>,
}

pub(crate) enum Rewound {
    /// Relaunch with `--resume <fork_from> --fork-session --session-id <session>`.
    Claude { fork_from: String, session: String },
    /// Relaunch with `resume <session>`: the forked thread.
    Codex { session: String },
}

impl Rewound {
    fn session(&self) -> &str {
        match self {
            Rewound::Claude { session, .. } | Rewound::Codex { session } => session,
        }
    }
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
/// notifications) are not the user.
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
    !text.is_empty() && !HARNESS.iter().any(|prefix| text.starts_with(prefix))
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

/// The bytes of a Claude session file that come before its latest
/// turn-starting prompt, which must be `entry`. A file-history-snapshot
/// written for that prompt just before it goes too. The kept history must
/// end exactly at the prompt's parent (a linear chain): anything else means
/// the file branches there, and resuming the copy would not be the
/// conversation the chat showed.
pub(crate) fn claude_rewind_prefix(source: &[u8], entry: &str) -> Result<Vec<u8>, String> {
    let mut starts = Vec::new();
    let mut values = Vec::new();
    let mut offset = 0;
    for piece in source.split_inclusive(|byte| *byte == b'\n') {
        starts.push(offset);
        offset += piece.len();
        let line = piece.strip_suffix(b"\n").unwrap_or(piece);
        values.push(serde_json::from_slice::<serde_json::Value>(line).ok());
    }
    let uuid_at = |index: usize| values[index].as_ref().and_then(|value| str_field(value, "uuid"));
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
    let Some(&latest) = prompts.last() else {
        return Err("This chat has no message to edit".to_string());
    };
    if uuid_at(latest) != Some(entry) {
        return Err(STALE_REFUSAL.to_string());
    }
    if prompts.len() == 1 {
        return Err(FIRST_MESSAGE_REFUSAL.to_string());
    }
    let mut cut = latest;
    let snapshot_for_prompt = cut > 0
        && values[cut - 1].as_ref().is_some_and(|value| {
            str_field(value, "type") == Some("file-history-snapshot")
                && str_field(value, "messageId") == Some(entry)
        });
    if snapshot_for_prompt {
        cut -= 1;
    }
    let parent = values[latest].as_ref().and_then(|value| str_field(value, "parentUuid"));
    let last_kept = (0..cut).rev().find_map(uuid_at);
    if parent.is_none() || last_kept != parent {
        return Err(
            "This chat's history branches just before that message, so it can't be rewound there"
                .to_string(),
        );
    }
    Ok(source[..starts[cut]].to_vec())
}

fn claude_session_file(cwd: &Path, session: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    let file = home
        .join(".claude")
        .join("projects")
        .join(encode_claude_project_dir(cwd))
        .join(format!("{session}.jsonl"));
    if file.is_file() {
        Ok(file)
    } else {
        Err(format!("This chat's conversation file is missing: {}", file.display()))
    }
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
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

fn rewind_claude(cwd: &Path, session: &str, entry: &str) -> Result<Rewound, String> {
    let source = claude_session_file(cwd, session)?;
    let bytes = std::fs::read(&source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    let prefix = claude_rewind_prefix(&bytes, entry)?;
    let fork_from = uuid::Uuid::new_v4().to_string();
    write_private_new(&source.with_file_name(format!("{fork_from}.jsonl")), &prefix)?;
    Ok(Rewound::Claude { fork_from, session: uuid::Uuid::new_v4().to_string() })
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
fn codex_rollout_file(sessions: &Path, thread: &str) -> Result<PathBuf, String> {
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

enum CodexForkPoint {
    /// Fork through this turn: the one before the edited prompt's turn.
    Through(String),
    /// The edited prompt's turn, or the one before it, is in older history.
    NeedsEarlier,
    /// No prompt the chat shows at all (in these events).
    NoPrompt,
}

/// The turn to fork through: the one before the turn whose first prompt is
/// the latest turn-starting prompt, which must be `entry`.
fn codex_fork_point(events: &[CodexEvent], entry: &str) -> Result<CodexForkPoint, String> {
    let mut turn_at: Option<usize> = None;
    let mut prompted = false;
    let mut latest: Option<(Option<usize>, Option<&str>)> = None;
    for (index, event) in events.iter().enumerate() {
        match event {
            CodexEvent::Turn(_) => {
                turn_at = Some(index);
                prompted = false;
            }
            CodexEvent::Prompt(id) => {
                if !prompted {
                    latest = Some((turn_at, id.as_deref()));
                }
                prompted = true;
            }
        }
    }
    let Some((turn_at, id)) = latest else {
        return Ok(CodexForkPoint::NoPrompt);
    };
    match id {
        Some(id) if id == entry => {}
        Some(_) => return Err(STALE_REFUSAL.to_string()),
        None => return Err("This Codex conversation does not record message ids, so it can't be rewound".to_string()),
    }
    let Some(turn_at) = turn_at else {
        return Ok(CodexForkPoint::NeedsEarlier);
    };
    Ok(events[..turn_at]
        .iter()
        .rev()
        .find_map(|event| match event {
            CodexEvent::Turn(turn) => Some(CodexForkPoint::Through(turn.clone())),
            CodexEvent::Prompt(_) => None,
        })
        .unwrap_or(CodexForkPoint::NeedsEarlier))
}

fn codex_turn_before_edit(sessions: &Path, thread: &str, entry: &str) -> Result<String, String> {
    let own = read_codex_rollout(&codex_rollout_file(sessions, thread)?, None)?;
    let mut events = own.events;
    let mut base = own.base;
    loop {
        let point = codex_fork_point(&events, entry)?;
        if let CodexForkPoint::Through(turn) = point {
            return Ok(turn);
        }
        let Some((older, offset)) = base.take() else {
            return Err(match point {
                CodexForkPoint::NoPrompt => "This chat has no message to edit".to_string(),
                _ => FIRST_MESSAGE_REFUSAL.to_string(),
            });
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
                    CODEX_FORK_DEADLINE.as_secs()
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
            return Err(format!("Codex could not rewind the conversation: {text}"));
        }
        return Ok(message.get("result").cloned().unwrap_or_default());
    }
}

fn codex_fork_exchange(child: &mut Child, thread: &str, last_turn: &str) -> Result<String, String> {
    let mut stdin = child.stdin.take().ok_or_else(|| "codex app-server has no stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "codex app-server has no stdout".to_string())?;
    let (tx, lines) = mpsc::channel();
    thread::Builder::new()
        .name("rudder-chat-edit-codex".to_string())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("could not read codex app-server: {error}"))?;
    let deadline = Instant::now() + CODEX_FORK_DEADLINE;
    send_rpc(
        &mut stdin,
        serde_json::json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "rudder", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true },
            },
        }),
    )?;
    await_rpc(&lines, 1, deadline)?;
    send_rpc(&mut stdin, serde_json::json!({ "method": "initialized" }))?;
    send_rpc(
        &mut stdin,
        serde_json::json!({
            "id": 2,
            "method": "thread/fork",
            "params": { "threadId": thread, "lastTurnId": last_turn, "excludeTurns": true },
        }),
    )?;
    let result = await_rpc(&lines, 2, deadline)?;
    let session = result
        .pointer("/thread/id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| valid_session_id(id))
        .ok_or_else(|| "Codex answered the fork without a thread id".to_string())?;
    let saved = result
        .pointer("/thread/path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|path| Path::new(path).is_file());
    if !saved {
        return Err(format!("Codex did not save the rewound conversation {session}"));
    }
    Ok(session.to_string())
}

/// One `thread/fork` on a one-shot `codex app-server` (stdio JSON-RPC) with
/// the same CODEX_HOME the agent uses. No turn runs.
fn codex_fork_through(cwd: &Path, thread: &str, last_turn: &str) -> Result<String, String> {
    let mut child = Command::new(codex_program())
        .arg("app-server")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start codex app-server: {error}"))?;
    let result = codex_fork_exchange(&mut child, thread, last_turn);
    // Its stdin is closed by now (dropped with the exchange): it exits on EOF.
    let stop_by = Instant::now() + Duration::from_secs(5);
    while Instant::now() < stop_by && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(50));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn rewind_codex(cwd: &Path, session: &str, entry: &str) -> Result<Rewound, String> {
    let sessions = codex_home()?.join("sessions");
    let last_turn = codex_turn_before_edit(&sessions, session, entry)?;
    let forked = codex_fork_through(cwd, session, &last_turn)?;
    Ok(Rewound::Codex { session: forked })
}

// ---------------------------------------------------------------------------
// The App side: checks, the thread, and the relaunch.
// ---------------------------------------------------------------------------

fn edit_refusal(run: &AgentRun) -> Option<String> {
    if run.is_orchestrator() || run.gam.is_some() || run.plain_process || is_cloud_owned_agent(run) {
        return Some("Messages in this agent can't be edited".to_string());
    }
    match run.status {
        AgentStatus::Merged => return Some("Its work is already merged; start a new agent instead".to_string()),
        AgentStatus::Migrated => return Some("This agent runs on another machine now".to_string()),
        _ => {}
    }
    if run.backend == Backend::Opencode {
        return Some("Editing a message works in Claude and Codex chats".to_string());
    }
    if let Some(profile) = run.execution_profile.as_deref().filter(|profile| *profile != "native") {
        return Some(format!("This agent runs on the {profile} profile, which can't rewind a conversation"));
    }
    None
}

fn mid_turn(run: &mut AgentRun) -> bool {
    let live = run.terminal.as_mut().is_some_and(|terminal| terminal.is_alive());
    live && run.status == AgentStatus::Running && !run.needs_user_input
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
        let (entry, text) = instruction
            .split_once('\n')
            .ok_or_else(|| "expected the message id, then the new text".to_string())?;
        let entry = entry.trim().to_string();
        if !valid_entry_id(&entry) {
            return Err(format!("not a message id: {entry}"));
        }
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
        if mid_turn(run) {
            return Err(MID_TURN_REFUSAL.to_string());
        }
        let session = run
            .session_id
            .clone()
            .filter(|id| valid_session_id(id))
            .ok_or_else(|| "This chat has no saved conversation to rewind".to_string())?;
        if !run.cwd.is_dir() {
            return Err(format!("Its workspace is gone: {}", run.cwd.display()));
        }
        let run_id = run.id.clone();
        let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
        let backend = run.backend;
        let cwd = run.cwd.clone();
        if !self.chat_edits_in_flight.insert(run_id.clone()) {
            return Err("This chat is already going back to an earlier message".to_string());
        }
        let tx = self.chat_edit_tx.clone();
        let request_id = request_id.to_string();
        let spawned = thread::Builder::new()
            .name("rudder-chat-edit".to_string())
            .spawn({
                let run_id = run_id.clone();
                move || {
                    let result = match backend {
                        Backend::Claude => rewind_claude(&cwd, &session, &entry),
                        Backend::Codex => rewind_codex(&cwd, &session, &entry),
                        Backend::Opencode => Err("Editing a message works in Claude and Codex chats".to_string()),
                    };
                    let _ = tx.send(ChatEditOutcome { run_id, request_id, source_session: session, text, result });
                }
            });
        if let Err(error) = spawned {
            self.chat_edits_in_flight.remove(&run_id);
            return Err(format!("could not start the rewind: {error}"));
        }
        self.push_activity(format!("{label}: going back to before its last message"));
        Ok(())
    }

    pub(crate) fn drain_chat_edits(&mut self) {
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
        let mut command = match &rewound {
            Rewound::Claude { fork_from, session } => {
                let mut command = claude_resume_command(run, fork_from);
                command.args.extend(["--fork-session".to_string(), "--session-id".to_string(), session.clone()]);
                command
            }
            Rewound::Codex { session } => codex_resume_command(run, session),
        };
        if run.is_main() && run.execution_profile.is_none() {
            signals::augment_worker_command(&mut command, backend, run.mode, &run.id);
        } else {
            signals::augment_existing_worker_command(
                &mut command,
                backend,
                run.mode,
                &run.id,
                run.execution_profile.as_deref(),
            );
        }
        // The edited text is the first turn. After `--`, text that starts
        // with "-" (a list) is the prompt, not a flag.
        command.args.push("--".to_string());
        command.args.push(outcome.text.clone());
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
        run.session_id = Some(rewound.session().to_string());
        run.status = AgentStatus::Running;
        run.completed_at = None;
        run.last_output_at = Instant::now();
        run.needs_permission = false;
        run.needs_user_input = false;
        run.wait_signal = None;
        run.last_error = None;
        run.worker_exit_note = None;
        run.current_prompt = outcome.text.clone();
        run.turns.push(AgentTurn { ts: now.clone(), prompt: outcome.text.clone(), source: "user".to_string() });
        run.last_user_input_at = now;
        run.last_worker_input_at = Some(Instant::now());
        let _ = save_native_run_record(&self.cwd, run);
        self.push_activity(format!(
            "{label}: went back to before its last message and sent \"{}\"",
            steer_preview(&outcome.text)
        ));
        self.mirror_graph();
        self.dirty = true;
        // The cut copy it resumed from belongs to this agent too: deleting
        // the agent deletes it with the rest of its conversation.
        if let Rewound::Claude { fork_from, .. } = &rewound {
            if let Err(error) = record_retired_session(&self.cwd, &outcome.run_id, fork_from) {
                self.push_activity(format!("{label}: could not note its old conversation file: {error}"));
            }
        }
        if backend == Backend::Claude {
            // Claude's hooks last reported the old session id, and the row
            // follows what they report (adopt_reported_claude_sessions).
            let file = signals::claude_session_path(&outcome.run_id)
                .ok_or_else(|| "HOME is not set".to_string())?;
            let temp = file.with_extension(format!("claude-session.{}.tmp", std::process::id()));
            std::fs::write(&temp, rewound.session())
                .and_then(|()| std::fs::rename(&temp, &file))
                .map_err(|error| {
                    format!("it restarted on the edited message, but its new session id was not recorded: {error}")
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(lines: &[serde_json::Value]) -> Vec<u8> {
        lines.iter().map(|line| format!("{line}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn chat_edit_claude_cut_keeps_history_before_the_latest_prompt() {
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
        let file = [first.clone(), tool_turn.clone(), snapshot, edited].concat();

        // The cut drops the edited prompt, its file-history-snapshot and all after.
        let kept = claude_rewind_prefix(&file, "u2").unwrap();
        assert_eq!(kept, [first.clone(), tool_turn.clone()].concat());

        // Without a snapshot for it, the cut is at the prompt itself.
        let plain = [first.clone(), tool_turn.clone(), session(&[prompt("u2", Some("s2"), "second ask")])].concat();
        assert_eq!(claude_rewind_prefix(&plain, "u2").unwrap(), [first.clone(), tool_turn.clone()].concat());

        // Only the latest turn-starting prompt can be edited.
        assert!(matches!(claude_rewind_prefix(&file, "u1"), Err(reason) if reason == STALE_REFUSAL));
        assert!(matches!(claude_rewind_prefix(&file, "q1"), Err(reason) if reason == STALE_REFUSAL));

        // The first message has no earlier turn to go back to.
        assert!(matches!(claude_rewind_prefix(&first, "u1"), Err(reason) if reason == FIRST_MESSAGE_REFUSAL));

        // A prompt whose parent is not the last entry kept is on a branch.
        let branched = [first, tool_turn, session(&[prompt("u2", Some("a1"), "second ask")])].concat();
        assert!(matches!(claude_rewind_prefix(&branched, "u2"), Err(reason) if reason.contains("branches")));
    }
}
