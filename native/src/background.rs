//! Work a Claude Code agent still has running in the background: shells and
//! Monitor watchers started with `run_in_background`, async subagents, and
//! agent-team teammates. Claude's `Stop` hook ends the TURN while these keep
//! going, and each one restarts the agent with a `<task-notification>` (or a
//! teammate's message) when it finishes. So a row whose turn ended with
//! background work outstanding is waiting on it, not ready for review
//! (`WaitSignal::Background`, main.rs).
//!
//! Sources, measured on Claude Code 2.1.330:
//! - **The session transcript** (`~/.claude/projects/<cwd>/<session>.jsonl`).
//!   A launch is the tool result's `toolUseResult`: `backgroundTaskId` (a
//!   background or timed-out Bash), `status: "async_launched"` with `agentId`
//!   (a subagent), or `taskId` with `persistent: false` (a Monitor; a
//!   persistent one watches for the whole session and is not waited on). A
//!   `<task-notification>` carrying a `<status>` closes every `<task-id>` it
//!   names; it is written as a `queue-operation` enqueue the moment the task
//!   ends, then as the prompt or a `queued_command` attachment. A Monitor's
//!   event notifications carry no status and close nothing. `TaskStop`'s
//!   result (`task_id` + `task_type`) closes its task.
//! - **The team config** (`~/.claude/teams/session-<first 8 of the lead's
//!   session>/config.json`): each teammate's `isActive`, which Claude keeps
//!   itself. Teammates run in their own processes and outlive the lead's, so
//!   the config counts only while its `leadProcess.pid` is this run's process.
//!
//! Background tasks die with the process that started them, so only entries
//! written since this process spawned count (`ScanRequest::since_ms`).
//!
//! Codex is not tracked: its rollout records a long-running exec only as a
//! `session_id` in a tool output, never its end, and nothing restarts a Codex
//! turn when one finishes.
//!
//! The scan runs on its own thread and returns typed results; the App loop
//! only applies them (AGENTS.md: no file IO in the loop).
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// How often the dashboard rescans live Claude agents.
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// A session whose transcript is not on disk yet is looked for again after this.
const MISS_RECHECK: Duration = Duration::from_secs(5);

/// What an agent still has running in the background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackgroundWork {
    /// Background Bash commands and Monitor watchers.
    pub(crate) shells: usize,
    /// Async subagents.
    pub(crate) agents: usize,
    /// Agent-team teammates that are working.
    pub(crate) teammates: usize,
    /// Helpers it started with agents_start that are still working
    /// (helpers.rs). Each one's reply starts its next turn.
    pub(crate) helpers: usize,
}

impl BackgroundWork {
    pub(crate) fn any(self) -> bool {
        self.shells + self.agents + self.teammates + self.helpers > 0
    }

    /// The row's status: "waiting on 2 helpers and 1 command".
    pub(crate) fn label(self) -> String {
        let helpers = self.agents + self.teammates + self.helpers;
        let plural = |count: usize, word: &str| {
            format!("{count} {word}{}", if count == 1 { "" } else { "s" })
        };
        match (helpers, self.shells) {
            (0, 0) => "waiting on background work".to_string(),
            (helpers, 0) => format!("waiting on {}", plural(helpers, "helper")),
            (0, shells) => format!("waiting on {}", plural(shells, "background command")),
            (helpers, shells) => {
                format!("waiting on {} and {}", plural(helpers, "helper"), plural(shells, "command"))
            }
        }
    }

    /// run.json `background`, read by cloud/worker/agent-status.mjs.
    pub(crate) fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "shells": self.shells,
            "agents": self.agents + self.helpers,
            "teammates": self.teammates,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskKind {
    Shell,
    Agent,
}

/// The background tasks a Claude transcript leaves open, built line by line.
#[derive(Debug, Default)]
pub(crate) struct TranscriptTasks {
    open: HashMap<String, TaskKind>,
}

/// Every needle a line that launches or closes a task contains. A line with
/// none of them is skipped before it is parsed.
const NEEDLES: [&str; 5] = [
    "\"backgroundTaskId\"",
    "\"async_launched\"",
    "\"taskId\"",
    "\"task_id\"",
    "<task-notification>",
];

fn str_at<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// The ids each `<task-notification>` with a `<status>` in `text` closes.
fn closed_task_ids(text: &str) -> Vec<String> {
    const OPEN: &str = "<task-notification>";
    const CLOSE: &str = "</task-notification>";
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let body = &rest[start + OPEN.len()..];
        let end = body.find(CLOSE).unwrap_or(body.len());
        let block = &body[..end];
        rest = &body[end..];
        if !block.contains("<status>") {
            continue;
        }
        let mut tags = block;
        while let Some(at) = tags.find("<task-id>") {
            let after = &tags[at + "<task-id>".len()..];
            let Some(stop) = after.find("</task-id>") else {
                break;
            };
            let id = after[..stop].trim();
            if !id.is_empty() {
                ids.push(id.to_string());
            }
            tags = &after[stop..];
        }
    }
    ids
}

/// A user message's text: Claude writes a prompt as a string, tool results and
/// some prompts as a list of parts.
fn message_text(message: Option<&serde_json::Value>) -> String {
    match message.and_then(|message| message.get("content")) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter(|part| str_at(part, "type") == Some("text"))
            .filter_map(|part| str_at(part, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl TranscriptTasks {
    /// Apply one transcript line. Lines written before `since_ms` (Unix ms)
    /// belong to an earlier process, whose tasks died with it.
    pub(crate) fn ingest(&mut self, line: &str, since_ms: u64) {
        // A request body repeats the whole conversation, notifications included.
        if line.starts_with("{\"type\":\"api-request") || !NEEDLES.iter().any(|needle| line.contains(needle)) {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let written = str_at(&value, "timestamp").and_then(crate::timeline::iso_millis);
        if written.is_none_or(|at| at < since_ms) {
            return;
        }
        let closed = match str_at(&value, "type") {
            Some("user") => {
                if let Some(result) = value.get("toolUseResult").filter(|result| result.is_object()) {
                    self.apply_tool_result(result);
                }
                closed_task_ids(&message_text(value.get("message")))
            }
            Some("queue-operation") if str_at(&value, "operation") == Some("enqueue") => {
                closed_task_ids(str_at(&value, "content").unwrap_or_default())
            }
            Some("attachment") => value
                .get("attachment")
                .filter(|attachment| str_at(attachment, "type") == Some("queued_command"))
                .and_then(|attachment| str_at(attachment, "prompt"))
                .map(closed_task_ids)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        for id in closed {
            self.open.remove(&id);
        }
    }

    fn apply_tool_result(&mut self, result: &serde_json::Value) {
        if let Some(id) = str_at(result, "backgroundTaskId") {
            self.open.insert(id.to_string(), TaskKind::Shell);
        }
        if str_at(result, "status") == Some("async_launched") {
            if let Some(id) = str_at(result, "agentId") {
                self.open.insert(id.to_string(), TaskKind::Agent);
            }
        }
        if let Some(id) = str_at(result, "taskId") {
            if result.get("persistent").and_then(serde_json::Value::as_bool) == Some(false) {
                self.open.insert(id.to_string(), TaskKind::Shell);
            }
        }
        if str_at(result, "task_type").is_some() {
            if let Some(id) = str_at(result, "task_id") {
                self.open.remove(id);
            }
        }
    }

    pub(crate) fn work(&self) -> BackgroundWork {
        let count = |kind| self.open.values().filter(|open| **open == kind).count();
        BackgroundWork {
            shells: count(TaskKind::Shell),
            agents: count(TaskKind::Agent),
            teammates: 0,
            helpers: 0,
        }
    }
}

/// Teammates working for the lead `session` running as process `pid`, from
/// its team config. A config another process claimed counts nothing.
pub(crate) fn busy_teammates(config: &str, session: &str, pid: u32) -> usize {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(config) else {
        return 0;
    };
    let lead_pid = value
        .get("leadProcess")
        .and_then(|lead| lead.get("pid"))
        .and_then(serde_json::Value::as_u64);
    if str_at(&value, "leadSessionId") != Some(session) || lead_pid != Some(u64::from(pid)) {
        return 0;
    }
    value
        .get("members")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|member| str_at(member, "agentType") != Some("team-lead"))
        .filter(|member| member.get("isActive").and_then(serde_json::Value::as_bool) == Some(true))
        .count()
}

/// `~/.claude/teams/session-<first 8 of the session>/config.json`.
fn team_config_path(home: &Path, session: &str) -> PathBuf {
    let short: String = session.chars().take(8).collect();
    home.join(".claude").join("teams").join(format!("session-{short}")).join("config.json")
}

/// The time the Stop hook wrote the run's signal file.
pub(crate) fn signal_written_at(run_id: &str) -> Option<SystemTime> {
    std::fs::metadata(crate::signals::signal_path(run_id)?).ok()?.modified().ok()
}

/// One live Claude agent to scan.
pub(crate) struct ScanRequest {
    pub(crate) run_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: String,
    /// When its process spawned (Unix ms).
    pub(crate) since_ms: u64,
    pub(crate) pid: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct ScanResult {
    pub(crate) run_id: String,
    /// Taken before anything was read. A Stop signal written after this may
    /// end a turn whose last launch the scan did not see yet.
    pub(crate) read_at: SystemTime,
    pub(crate) work: BackgroundWork,
}

/// Where one run's transcript has been read to.
struct Tracker {
    cwd: PathBuf,
    session_id: String,
    since_ms: u64,
    path: Option<PathBuf>,
    looked_at: Instant,
    offset: u64,
    tasks: TranscriptTasks,
}

impl Tracker {
    fn new(request: &ScanRequest) -> Self {
        Tracker {
            cwd: request.cwd.clone(),
            session_id: request.session_id.clone(),
            since_ms: request.since_ms,
            path: crate::claude_transcript_path(&request.cwd, &request.session_id),
            looked_at: Instant::now(),
            offset: 0,
            tasks: TranscriptTasks::default(),
        }
    }

    fn same_process(&self, request: &ScanRequest) -> bool {
        self.cwd == request.cwd && self.session_id == request.session_id && self.since_ms == request.since_ms
    }

    /// Read the lines appended since the last scan. A line still being
    /// written is left for the next one.
    fn read_new_lines(&mut self) {
        if self.path.is_none() && self.looked_at.elapsed() >= MISS_RECHECK {
            self.path = crate::claude_transcript_path(&self.cwd, &self.session_id);
            self.looked_at = Instant::now();
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if len < self.offset {
            // Rewritten from the start.
            self.offset = 0;
            self.tasks = TranscriptTasks::default();
        }
        if len == self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::with_capacity(256 * 1024, file);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(read) if read > 0 && line.ends_with(b"\n") => {
                    self.offset += read as u64;
                    if let Ok(text) = std::str::from_utf8(&line) {
                        self.tasks.ingest(text, self.since_ms);
                    }
                }
                _ => break,
            }
        }
    }
}

fn trackers() -> std::sync::MutexGuard<'static, HashMap<String, Tracker>> {
    static TRACKERS: OnceLock<Mutex<HashMap<String, Tracker>>> = OnceLock::new();
    TRACKERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reads each agent's transcript and team config. Runs on the scan thread.
pub(crate) fn scan(requests: &[ScanRequest]) -> Vec<ScanResult> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut trackers = trackers();
    trackers.retain(|run_id, _| requests.iter().any(|request| &request.run_id == run_id));
    requests
        .iter()
        .map(|request| {
            let read_at = SystemTime::now();
            let tracker = trackers
                .entry(request.run_id.clone())
                .or_insert_with(|| Tracker::new(request));
            if !tracker.same_process(request) {
                *tracker = Tracker::new(request);
            }
            tracker.read_new_lines();
            let mut work = tracker.tasks.work();
            if let (Some(home), Some(pid)) = (home.as_ref(), request.pid) {
                work.teammates = std::fs::read_to_string(team_config_path(home, &request.session_id))
                    .map(|config| busy_teammates(&config, &request.session_id, pid))
                    .unwrap_or(0);
            }
            ScanResult { run_id: request.run_id.clone(), read_at, work }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Claude Code 2.1.330 lines from a lead session, trimmed to the
    /// fields that matter.
    const TRANSCRIPT: &str = include_str!("../../tests/fixtures/claude-2.1.330-background.jsonl");
    const TEAM: &str = include_str!("../../tests/fixtures/claude-2.1.330-team-config.json");

    fn ms(iso: &str) -> u64 {
        crate::timeline::iso_millis(iso).unwrap()
    }

    /// The open work after every line up to (and including) the first one
    /// whose text contains `marker`.
    fn work_through(marker: &str, since: &str) -> BackgroundWork {
        let mut tasks = TranscriptTasks::default();
        for line in TRANSCRIPT.lines() {
            tasks.ingest(line, ms(since));
            if line.contains(marker) {
                return tasks.work();
            }
        }
        panic!("no line contains {marker}");
    }

    #[test]
    fn transcript_counts_what_is_still_running_at_each_turn_end() {
        let since = "2026-11-23T01:00:00.000Z";
        // A background Bash and an async subagent are out when the first turn ends.
        assert_eq!(
            work_through("stop-1", since),
            BackgroundWork { shells: 1, agents: 1, teammates: 0, helpers: 0 }
        );
        // The shell's notification closes it as soon as it is queued; the
        // subagent and a new Monitor are still out. A persistent Monitor
        // watches for the whole session and is not waited on.
        assert_eq!(
            work_through("stop-2", since),
            BackgroundWork { shells: 1, agents: 1, teammates: 0, helpers: 0 }
        );
        // A Monitor event (no status) closes nothing; the subagent's
        // notification (a queued_command attachment) closes it; TaskStop
        // closes the Monitor.
        assert_eq!(work_through("stop-3", since), BackgroundWork::default());
        // One notification can close several tasks; a timed-out foreground
        // Bash that moved to the background counts like any other.
        assert_eq!(
            work_through("stop-4", since),
            BackgroundWork { shells: 1, agents: 0, teammates: 0, helpers: 0 }
        );
        assert_eq!(work_through("stop-5", since), BackgroundWork::default());
    }

    #[test]
    fn tasks_from_an_earlier_process_do_not_count() {
        // This process started after the first turn's launches: they died
        // with the process before it, and their notifications never came.
        assert_eq!(
            work_through("stop-1", "2026-11-23T01:50:30.000Z"),
            BackgroundWork::default()
        );
    }

    #[test]
    fn request_bodies_are_not_read_as_the_conversation() {
        // The fixture's api-request-blob repeats a notification for the
        // subagent; reading it would close the subagent before stop-2.
        assert!(TRANSCRIPT
            .lines()
            .any(|line| line.starts_with("{\"type\":\"api-request-blob\"") && line.contains("a65b7e9d3c4728f6f")));
        assert_eq!(work_through("stop-2", "2026-11-23T01:00:00.000Z").agents, 1);
    }

    #[test]
    fn team_config_counts_working_teammates_of_this_lead_process_only() {
        let session = "9ae11dea-ffef-4096-93e6-aa173a0ec95d";
        assert_eq!(busy_teammates(TEAM, session, 1085718), 2);
        // A lead process that has since been replaced owns none of them.
        assert_eq!(busy_teammates(TEAM, session, 4242), 0);
        assert_eq!(busy_teammates(TEAM, "another-session", 1085718), 0);
        assert_eq!(
            team_config_path(Path::new("/home/u"), session),
            PathBuf::from("/home/u/.claude/teams/session-9ae11dea/config.json")
        );
    }

    #[test]
    fn scan_reads_only_new_complete_lines() {
        let root = std::env::temp_dir().join(format!("rudder-background-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let lines: Vec<&str> = TRANSCRIPT.lines().collect();
        let stop1 = lines.iter().position(|line| line.contains("stop-1")).unwrap();
        let head = lines[..=stop1].join("\n");
        // The last launch line is still being written.
        let partial = &lines[stop1 + 1][..20];
        std::fs::write(&path, format!("{head}\n{partial}")).unwrap();
        let mut tracker = Tracker {
            cwd: root.clone(),
            session_id: "session".into(),
            since_ms: ms("2026-11-23T01:00:00.000Z"),
            path: Some(path.clone()),
            looked_at: Instant::now(),
            offset: 0,
            tasks: TranscriptTasks::default(),
        };
        tracker.read_new_lines();
        assert_eq!(tracker.tasks.work(), BackgroundWork { shells: 1, agents: 1, teammates: 0, helpers: 0 });
        let read = tracker.offset;
        assert_eq!(read as usize, head.len() + 1, "the partial line is left");
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        tracker.read_new_lines();
        assert_eq!(tracker.tasks.work(), BackgroundWork::default());
        assert!(tracker.offset > read);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn labels_name_helpers_and_commands() {
        let work = |shells, agents, teammates| BackgroundWork { shells, agents, teammates, helpers: 0 };
        assert_eq!(work(0, 1, 2).label(), "waiting on 3 helpers");
        assert_eq!(work(1, 0, 0).label(), "waiting on 1 background command");
        assert_eq!(work(2, 1, 0).label(), "waiting on 1 helper and 2 commands");
        assert_eq!(work(0, 0, 0).label(), "waiting on background work");
        assert_eq!(BackgroundWork { helpers: 1, ..work(0, 1, 0) }.label(), "waiting on 2 helpers");
    }
}
