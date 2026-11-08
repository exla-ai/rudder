//! Subagents a Claude Code or Codex agent spawns, shown as a small tree under
//! its row in the agent list (render.rs) and opened with Tab (main.rs).
//!
//! View state only. A subagent is never a run: it is not persisted, never
//! merges, and never counts as an agent for `/sync`, scheduling or capacity.
//!
//! Detection is the backends' own hooks, wired next to the lifecycle signals in
//! signals.rs. Each hook drops its JSON payload as one file in
//! `<rudder_home>/signals/<run_id>.subagents/`. A background scan reads and
//! deletes those files and hands the App loop typed `SubagentEvent`s.
//!
//! - **Claude Code** (measured on 2.1.319): `SubagentStart` (`agent_id`,
//!   `agent_type`), `SubagentStop` (`agent_id`, `last_assistant_message`), and
//!   `PostToolUse` on the `Agent` tool, whose `tool_response` pairs `agentId`
//!   with the `description` the model gave. Every subagent is launched async, so
//!   that response arrives right after the start. `UserPromptSubmit` matters
//!   too: Claude reports a finished background subagent by injecting a
//!   `<task-notification>` prompt with its final `<status>` (completed, failed,
//!   killed), and any other prompt is the human's next turn.
//! - **Codex** (0.166, `multi_agent`): `SubagentStart` / `SubagentStop` hooks;
//!   `agent_id` is the subagent's thread id. They carry no name (the spawn
//!   message itself is encrypted in the hook payload), so the scan reads the
//!   head of the subagent's rollout (`transcript_path`) for its `agent_path`
//!   (`/root/<task name>`) and nickname.

use super::*;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Running,
    Done,
    Failed,
    /// Killed, or its parent's process ended before it reported.
    Stopped,
}

impl SubagentStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Done => "done",
            SubagentStatus::Failed => "failed",
            SubagentStatus::Stopped => "stopped",
        }
    }

    pub(crate) fn finished(self) -> bool {
        self != SubagentStatus::Running
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Subagent {
    /// The backend's id: Claude's `agent_id`, Codex's subagent thread id.
    pub(crate) id: String,
    /// `agent_type` (Claude's `general-purpose`, `Explore`, ...; Codex's role).
    pub(crate) kind: String,
    /// What the parent asked it to do, when the backend says.
    pub(crate) description: Option<String>,
    pub(crate) status: SubagentStatus,
    pub(crate) started: Instant,
    pub(crate) finished: Option<Instant>,
    pub(crate) last_message: Option<String>,
}

impl Subagent {
    pub(crate) fn label(&self) -> &str {
        match self.description.as_deref() {
            Some(text) if !text.is_empty() => text,
            _ if !self.kind.is_empty() => &self.kind,
            _ => "subagent",
        }
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        self.finished
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SubagentEvent {
    Started { id: String, kind: String },
    Named { id: String, description: String },
    Stopped { id: String, kind: String, last_message: Option<String> },
    /// Claude's `<task-notification>` for a background task. Ignored unless the
    /// task is a subagent we know (background shells notify the same way).
    Notified { id: String, status: SubagentStatus, result: Option<String> },
    /// The human sent the parent its next prompt.
    HumanPrompt,
}

/// Finished subagents stay listed (dimmed) until the parent's next human
/// prompt, and at most this many at a time; running ones are always listed.
pub(crate) const FINISHED_KEEP: usize = 8;

/// How often the background scan looks for new hook payloads.
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_millis(500);

/// Most payload files one scan takes per run, so a flood cannot stall it.
const SCAN_BATCH: usize = 64;

/// The directory a run's subagent hooks write into, next to its signal file.
pub(crate) fn events_dir_for_signal(signal: &Path) -> PathBuf {
    signal.with_extension("subagents")
}

pub(crate) fn events_dir(run_id: &str) -> Option<PathBuf> {
    Some(events_dir_for_signal(&signals::signal_path(run_id)?))
}

fn text_field<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// One line, at most `max` characters, for a sidebar row or detail line.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}\u{2026}", cut.trim_end())
}

/// The value between `<tag>` and `</tag>` in a task notification.
fn xml_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&close)?;
    Some(text[start..end].trim())
}

/// Turn one hook payload into the events it means. Tolerant: an unknown or
/// malformed payload means nothing.
pub(crate) fn parse_hook_payload(body: &str) -> Vec<SubagentEvent> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
        return Vec::new();
    };
    let kind = || text_field(&value, "agent_type").unwrap_or("").to_string();
    match text_field(&value, "hook_event_name") {
        Some("SubagentStart") => text_field(&value, "agent_id")
            .map(|id| vec![SubagentEvent::Started { id: id.to_string(), kind: kind() }])
            .unwrap_or_default(),
        Some("SubagentStop") => text_field(&value, "agent_id")
            .map(|id| {
                vec![SubagentEvent::Stopped {
                    id: id.to_string(),
                    kind: kind(),
                    last_message: text_field(&value, "last_assistant_message")
                        .map(|text| one_line(text, 400)),
                }]
            })
            .unwrap_or_default(),
        Some("PostToolUse") => {
            let response = value.get("tool_response");
            let id = response.and_then(|r| text_field(r, "agentId"));
            let description = response
                .and_then(|r| text_field(r, "description"))
                .or_else(|| value.get("tool_input").and_then(|i| text_field(i, "description")));
            match (id, description) {
                (Some(id), Some(description)) => vec![SubagentEvent::Named {
                    id: id.to_string(),
                    description: one_line(description, 120),
                }],
                _ => Vec::new(),
            }
        }
        Some("UserPromptSubmit") => {
            let prompt = text_field(&value, "prompt").unwrap_or("");
            if !prompt.starts_with("<task-notification>") {
                return vec![SubagentEvent::HumanPrompt];
            }
            let Some(id) = xml_tag(prompt, "task-id").filter(|id| !id.is_empty()) else {
                return Vec::new();
            };
            let status = match xml_tag(prompt, "status").unwrap_or("") {
                "completed" => SubagentStatus::Done,
                "failed" => SubagentStatus::Failed,
                "killed" | "stopped" | "cancelled" => SubagentStatus::Stopped,
                _ => return Vec::new(),
            };
            vec![SubagentEvent::Notified {
                id: id.to_string(),
                status,
                result: xml_tag(prompt, "result").map(|text| one_line(text, 400)),
            }]
        }
        _ => Vec::new(),
    }
}

/// Apply one event to a parent's subagent list (display order = start order).
pub(crate) fn apply_event(list: &mut Vec<Subagent>, event: SubagentEvent, now: Instant) {
    let fresh = |id: &str, kind: &str| Subagent {
        id: id.to_string(),
        kind: kind.to_string(),
        description: None,
        status: SubagentStatus::Running,
        started: now,
        finished: None,
        last_message: None,
    };
    match event {
        SubagentEvent::Started { id, kind } => match list.iter_mut().find(|s| s.id == id) {
            // A finished subagent the parent messaged again runs again.
            Some(sub) => {
                if sub.status.finished() {
                    sub.status = SubagentStatus::Running;
                    sub.started = now;
                    sub.finished = None;
                }
                if sub.kind.is_empty() {
                    sub.kind = kind;
                }
            }
            None => list.push(fresh(&id, &kind)),
        },
        SubagentEvent::Named { id, description } => {
            match list.iter_mut().find(|s| s.id == id) {
                Some(sub) => sub.description = Some(description),
                None => {
                    let mut sub = fresh(&id, "");
                    sub.description = Some(description);
                    list.push(sub);
                }
            }
        }
        SubagentEvent::Stopped { id, kind, last_message } => {
            if !list.iter().any(|s| s.id == id) {
                list.push(fresh(&id, &kind));
            }
            if let Some(sub) = list.iter_mut().find(|s| s.id == id) {
                if sub.status == SubagentStatus::Running {
                    sub.status = SubagentStatus::Done;
                }
                sub.finished.get_or_insert(now);
                if last_message.is_some() {
                    sub.last_message = last_message;
                }
            }
        }
        SubagentEvent::Notified { id, status, result } => {
            if let Some(sub) = list.iter_mut().find(|s| s.id == id) {
                if status != SubagentStatus::Done || sub.status == SubagentStatus::Running {
                    sub.status = status;
                }
                sub.finished.get_or_insert(now);
                if sub.last_message.is_none() {
                    sub.last_message = result;
                }
            }
        }
        SubagentEvent::HumanPrompt => list.retain(|s| !s.status.finished()),
    }
    // Keep only the newest finished ones.
    let over = list
        .iter()
        .filter(|s| s.status.finished())
        .count()
        .saturating_sub(FINISHED_KEEP);
    if over > 0 {
        let mut finished: Vec<(Instant, String)> = list
            .iter()
            .filter(|s| s.status.finished())
            .map(|s| (s.finished.unwrap_or(s.started), s.id.clone()))
            .collect();
        finished.sort();
        let drop: HashSet<String> = finished.into_iter().take(over).map(|(_, id)| id).collect();
        list.retain(|s| !drop.contains(&s.id));
    }
}

/// The task name (`/root/<name>`) or nickname Codex gave a subagent, read off
/// the `session_meta` line at the top of its rollout. The line carries the
/// whole base prompt after these fields, so only the head is read.
pub(crate) fn codex_subagent_name(rollout: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = vec![0_u8; 16 * 1024];
    let read = std::fs::File::open(rollout).ok()?.read(&mut head).ok()?;
    let head = String::from_utf8_lossy(&head[..read]);
    let field = |key: &str| -> Option<String> {
        let marker = format!("\"{key}\":\"");
        let start = head.find(&marker)? + marker.len();
        let end = start + head[start..].find('"')?;
        Some(head[start..end].trim().to_string()).filter(|text| !text.is_empty())
    };
    field("agent_path")
        .and_then(|path| path.rsplit('/').next().map(str::to_string))
        .filter(|name| !name.is_empty() && name != "root")
        .or_else(|| field("agent_nickname"))
}

/// Read and delete the payload files a run's hooks wrote, oldest first.
pub(crate) fn take_events_in(dir: &Path, backend: Backend) -> Vec<SubagentEvent> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    files.sort();
    let mut events = Vec::new();
    for (_, path) in files.into_iter().take(SCAN_BATCH) {
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        let parsed = parse_hook_payload(&body);
        // Codex names its subagents only in their rollout: SubagentStart's
        // `transcript_path` is the subagent's own, SubagentStop adds
        // `agent_transcript_path`.
        let codex_name = match parsed.first() {
            Some(SubagentEvent::Started { id, .. } | SubagentEvent::Stopped { id, .. })
                if backend == Backend::Codex =>
            {
                let value: serde_json::Value = serde_json::from_str(body.trim()).unwrap_or_default();
                text_field(&value, "agent_transcript_path")
                    .or_else(|| text_field(&value, "transcript_path"))
                    .and_then(|rollout| codex_subagent_name(Path::new(rollout)))
                    .map(|description| SubagentEvent::Named { id: id.clone(), description })
            }
            _ => None,
        };
        events.extend(parsed);
        events.extend(codex_name);
    }
    events
}

/// One background scan over the live runs: every run's new events.
pub(crate) fn scan(runs: &[(String, Backend)]) -> Vec<(String, Vec<SubagentEvent>)> {
    runs.iter()
        .filter_map(|(run_id, backend)| {
            let events = take_events_in(&events_dir(run_id)?, *backend);
            (!events.is_empty()).then(|| (run_id.clone(), events))
        })
        .collect()
}

/// `42s`, `3m`, `2h`: how long a subagent has run (or ran).
pub(crate) fn elapsed_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// `1m 12s`: the same, to the second, for the detail line.
pub(crate) fn elapsed_detail(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_payloads_become_subagent_rows_start_then_stop() {
        // The hook command Rudder installs, run for real on payloads shaped
        // like the ones Claude Code 2.1.319 sent in a live run.
        let root = std::env::temp_dir().join(format!("rudder-subagents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let signal = root.join("run-sub.json");
        let settings: serde_json::Value =
            serde_json::from_str(&signals::claude_settings_json(&signal, false)).unwrap();
        let command = |event: &str, index: usize| -> String {
            settings["hooks"][event][0]["hooks"][index]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} hook"))
                .to_string()
        };
        assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Agent|Task");
        let run_hook = |command: &str, payload: &str| {
            use std::io::Write;
            let mut child = std::process::Command::new("sh")
                .args(["-c", command])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
            // mtime orders the files; keep each one distinct.
            std::thread::sleep(Duration::from_millis(15));
        };
        let dir = events_dir_for_signal(&signal);
        let mut list: Vec<Subagent> = Vec::new();
        let start = Instant::now();

        run_hook(
            &command("SubagentStart", 0),
            r#"{"session_id":"s1","hook_event_name":"SubagentStart","agent_id":"a6263","agent_type":"general-purpose"}"#,
        );
        run_hook(
            &command("PostToolUse", 0),
            r#"{"hook_event_name":"PostToolUse","tool_name":"Agent","tool_input":{"description":"Say hi","prompt":"Reply hi"},"tool_response":{"isAsync":true,"status":"async_launched","agentId":"a6263","description":"Say hi"},"tool_use_id":"toolu_1"}"#,
        );
        for event in take_events_in(&dir, Backend::Claude) {
            apply_event(&mut list, event, start);
        }
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].label(), "Say hi");
        assert_eq!(list[0].status, SubagentStatus::Running);

        run_hook(
            &command("SubagentStop", 0),
            r#"{"hook_event_name":"SubagentStop","agent_id":"a6263","agent_type":"general-purpose","last_assistant_message":"hi"}"#,
        );
        // A notification for a background shell names no subagent we know.
        run_hook(
            &command("UserPromptSubmit", 2),
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>\n<task-id>b77</task-id>\n<status>failed</status>\n</task-notification>"}"#,
        );
        for event in take_events_in(&dir, Backend::Claude) {
            apply_event(&mut list, event, start + Duration::from_secs(3));
        }
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, SubagentStatus::Done);
        assert_eq!(list[0].last_message.as_deref(), Some("hi"));
        assert_eq!(elapsed_label(list[0].elapsed(Instant::now())), "3s");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "payloads are consumed");

        // The human's next prompt clears finished rows.
        run_hook(&command("UserPromptSubmit", 2), r#"{"hook_event_name":"UserPromptSubmit","prompt":"next"}"#);
        for event in take_events_in(&dir, Backend::Claude) {
            apply_event(&mut list, event, start + Duration::from_secs(4));
        }
        assert!(list.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
