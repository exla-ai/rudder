//! Per-agent timeline: where an agent's wall time and tokens went.
//!
//! Sources, all cheap and local:
//! - `.rudder/runs/<id>/timeline.jsonl` (append-only JSON lines). Rudder appends
//!   a `workspace` event when it creates the run's workspace and a `summary`
//!   event whenever a recomputed summary differs from the last one.
//! - The backend's own session log (Claude project jsonl / Codex rollout),
//!   which carries timestamps for every model response, tool call and prompt.
//!
//! Computation does file IO, so it runs off the App loop (`spawn_timeline_job`)
//! and returns a typed `TimelineJob` the diff panel drains.

use super::*;
use std::collections::HashMap;

pub(crate) const TIMELINE_FILE: &str = "timeline.jsonl";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineSummary {
    /// Latest observed request, separate from cumulative billing totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<ContextUsage>,
    /// Workspace startup (fork/checkout), when Rudder created one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) start_ms: Option<u64>,
    #[serde(default)]
    pub(crate) install_ms: u64,
    #[serde(default)]
    pub(crate) build_ms: u64,
    #[serde(default)]
    pub(crate) test_ms: u64,
    /// Other tool calls (reads, edits, searches, misc commands).
    #[serde(default)]
    pub(crate) tool_ms: u64,
    /// Blocked on the user: from the end of a turn to the next prompt.
    #[serde(default)]
    pub(crate) wait_ms: u64,
    #[serde(default)]
    pub(crate) model_ms: u64,
    /// Fresh (uncached) input tokens, cache writes included.
    #[serde(default)]
    pub(crate) input: u64,
    #[serde(default)]
    pub(crate) cached: u64,
    #[serde(default)]
    pub(crate) output: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextUsage {
    pub(crate) timestamp_ms: u64,
    pub(crate) input: u64,
    pub(crate) cached: Option<u64>,
    pub(crate) cache_write: Option<u64>,
    pub(crate) capacity: Option<u64>,
    /// Input of the conversation's first request, or of the first one after
    /// its last compaction: system prompt, tools, memory files and the opening
    /// prompt. The context panel shows it as the estimated fixed base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base: Option<u64>,
    /// Context size when the backend last compacted on its own (Claude Code's
    /// `compact_boundary` with trigger `auto`). Observed, never assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_compact_at: Option<u64>,
    /// Context size per request in this conversation, oldest first, at most
    /// `CONTEXT_HISTORY`. Memory only: timeline.jsonl gets a summary line on
    /// every change, so the history stays out of the file.
    #[serde(skip)]
    pub(crate) history: Vec<u64>,
}

/// Requests the context growth sparkline keeps.
pub(crate) const CONTEXT_HISTORY: usize = 60;

/// One conversation's context over time, folded request by request while a
/// session log is parsed (off the App loop). It is stamped onto that
/// conversation's latest request, so the newest conversation's track wins.
#[derive(Default)]
struct ContextTrack {
    base: Option<u64>,
    auto_compact_at: Option<u64>,
    history: Vec<u64>,
}

impl ContextTrack {
    fn request(&mut self, input: u64) {
        if input == 0 {
            return;
        }
        self.base.get_or_insert(input);
        self.history.push(input);
        if self.history.len() > CONTEXT_HISTORY {
            self.history.remove(0);
        }
    }

    fn compacted(&mut self, auto_at: Option<u64>) {
        self.base = None;
        if auto_at.is_some() {
            self.auto_compact_at = auto_at;
        }
    }

    fn stamp(self, context: Option<&mut ContextUsage>) {
        if let Some(c) = context {
            c.base = self.base;
            c.auto_compact_at = self.auto_compact_at;
            c.history = self.history;
        }
    }
}

fn latest_context(summary: &mut TimelineSummary, context: ContextUsage) {
    if summary.context.as_ref().is_none_or(|old| context.timestamp_ms >= old.timestamp_ms) {
        summary.context = Some(context);
    }
}

pub(crate) fn context_lines(summary: &TimelineSummary) -> Vec<String> {
    let Some(c) = &summary.context else {
        return vec!["Context: waiting for backend usage".into()];
    };
    let mut title = format!("Context: {} input tokens", format_token_count(c.input));
    if let Some(capacity) = c.capacity.filter(|n| *n > 0) {
        title.push_str(&format!(" / {} ({:.0}%)", format_token_count(capacity), c.input as f64 / capacity as f64 * 100.0));
    }
    let mut detail = Vec::new();
    if let Some(cached) = c.cached {
        detail.push(format!("{} cached", format_token_count(cached)));
        let fresh = c.input.saturating_sub(cached).saturating_sub(c.cache_write.unwrap_or(0));
        detail.push(format!("{} fresh", format_token_count(fresh)));
    }
    if let Some(write) = c.cache_write {
        detail.push(format!("{} cache write", format_token_count(write)));
    }
    let mut lines = vec![title];
    if !detail.is_empty() { lines.push(detail.join(" · ")); }
    lines.push("Latest request; excludes later messages and tool results".into());
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandKind {
    Install,
    Build,
    Test,
    Other,
}

pub(crate) fn classify_command(command: &str) -> CommandKind {
    let c = format!(" {} ", command.to_ascii_lowercase());
    let any = |needles: &[&str]| needles.iter().any(|n| c.contains(n));
    if any(&[
        " npm install", " npm i ", " npm ci", " pnpm install", " pnpm i ", " yarn install",
        " bun install", " pip install", " uv sync", " uv pip install", " cargo fetch",
        " go mod download", " bundle install", " poetry install",
    ]) {
        return CommandKind::Install;
    }
    if any(&[
        " test", "pytest", "vitest", "jest", "test:", " go test", " cargo nextest",
    ]) {
        return CommandKind::Test;
    }
    if any(&[
        " build", " tsc", " cargo check", " rcargo", " run check", " make", " clippy",
        " lint", " typecheck", " go vet",
    ]) {
        return CommandKind::Build;
    }
    CommandKind::Other
}

/// Milliseconds since the epoch for `YYYY-MM-DDTHH:MM:SS[.fff]Z` (offset `Z`
/// or `+00:00`). Backend logs always write UTC.
pub(crate) fn iso_millis(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut frac = 0i64;
    if b.get(19) == Some(&b'.') {
        let digits: String = ts[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        let padded = format!("{:0<3}", &digits[..digits.len().min(3)]);
        frac = padded.parse().ok()?;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let ms = ((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000) + frac;
    u64::try_from(ms).ok()
}

fn add_tool(summary: &mut TimelineSummary, command: Option<&str>, ms: u64) {
    match command.map(classify_command).unwrap_or(CommandKind::Other) {
        CommandKind::Install => summary.install_ms += ms,
        CommandKind::Build => summary.build_ms += ms,
        CommandKind::Test => summary.test_ms += ms,
        CommandKind::Other => summary.tool_ms += ms,
    }
}

/// Fold one Claude Code session transcript into `summary`.
pub(crate) fn analyze_claude_transcript(content: &str, since_ms: u64, summary: &mut TimelineSummary) {
    let mut prev_ts: Option<u64> = None;
    // tool_use id -> (start, bash command)
    let mut pending: HashMap<String, (u64, Option<String>)> = HashMap::new();
    // message id -> (input, cached, output); streamed blocks repeat usage.
    let mut usage: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut anon = 0usize;
    // The whole conversation's context, from before the run too: its base is
    // the first request, and a resumed session's first request here is not.
    let mut track = ContextTrack::default();
    let mut last_request: Option<String> = None;
    let mut latest: Option<ContextUsage> = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "system" && value.get("subtype").and_then(|v| v.as_str()) == Some("compact_boundary") {
            let meta = value.get("compactMetadata");
            let auto = meta.and_then(|m| m.get("trigger")).and_then(|v| v.as_str()) == Some("auto");
            track.compacted(meta.and_then(|m| m.get("preTokens")).and_then(|v| v.as_u64()).filter(|_| auto));
            continue;
        }
        if kind != "assistant" && kind != "user" {
            continue;
        }
        if value.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()).and_then(iso_millis) else {
            continue;
        };
        let message = value.get("message");
        let request = message.and_then(|m| m.get("usage")).filter(|_| kind == "assistant").and_then(|u| {
            let field = |k: &str| u.get(k).and_then(|v| v.as_u64());
            let (input, cached, write) =
                (field("input_tokens")?, field("cache_read_input_tokens")?, field("cache_creation_input_tokens")?);
            Some(ContextUsage {
                timestamp_ms: ts, input: input.saturating_add(cached).saturating_add(write),
                cached: Some(cached), cache_write: Some(write), ..Default::default()
            })
        });
        if let Some(request) = &request {
            // Streamed blocks of one response repeat its usage: one request.
            let id = message.and_then(|m| m.get("id")).and_then(|v| v.as_str()).map(ToString::to_string);
            if id.is_none() || id != last_request {
                track.request(request.input);
            }
            last_request = id;
        }
        if ts < since_ms {
            continue;
        }
        let gap = prev_ts.map(|p| ts.saturating_sub(p)).unwrap_or(0);
        let content_items = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        if kind == "assistant" {
            summary.model_ms += gap;
            if let Some(u) = message.and_then(|m| m.get("usage")) {
                let get = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(request) = request {
                    if latest.as_ref().is_none_or(|old| request.timestamp_ms >= old.timestamp_ms) {
                        latest = Some(request);
                    }
                }
                let id = message
                    .and_then(|m| m.get("id"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| {
                        anon += 1;
                        format!("anon-{anon}")
                    });
                usage.insert(
                    id,
                    (
                        get("input_tokens") + get("cache_creation_input_tokens"),
                        get("cache_read_input_tokens"),
                        get("output_tokens"),
                    ),
                );
            }
            for item in &content_items {
                if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let command = item
                        .get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    pending.insert(id, (ts, command));
                }
            }
        } else {
            let results: Vec<&serde_json::Value> = content_items
                .iter()
                .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
                .collect();
            if results.is_empty() {
                // A real prompt: the time since the last event was the agent
                // waiting on its user (or idle).
                summary.wait_ms += gap;
            } else {
                let mut counted = false;
                for item in results {
                    let id = item.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some((start, command)) = pending.remove(id) {
                        add_tool(summary, command.as_deref(), ts.saturating_sub(start));
                        counted = true;
                    }
                }
                if !counted {
                    summary.tool_ms += gap;
                }
            }
        }
        prev_ts = Some(ts.max(prev_ts.unwrap_or(0)));
    }
    for (input, cached, output) in usage.values() {
        summary.input += input;
        summary.cached += cached;
        summary.output += output;
    }
    track.stamp(latest.as_mut());
    if let Some(latest) = latest {
        latest_context(summary, latest);
    }
}

/// Fold one Codex rollout into `summary` when it belongs to `cwd`.
pub(crate) fn analyze_codex_rollout(
    content: &str,
    cwd: &str,
    since_ms: u64,
    summary: &mut TimelineSummary,
) -> bool {
    let mut local = TimelineSummary::default();
    let mut matched = false;
    let mut prev_ts: Option<u64> = None;
    let mut pending: HashMap<String, (u64, Option<String>)> = HashMap::new();
    let mut last_total: Option<(u64, u64, u64)> = None;
    let mut track = ContextTrack::default();
    // Codex can report one request twice; an unchanged running total says so.
    let mut last_request: Option<String> = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = value.get("payload");
        let ptype = payload.and_then(|p| p.get("type")).and_then(|v| v.as_str()).unwrap_or("");
        let ts = value.get("timestamp").and_then(|v| v.as_str()).and_then(iso_millis);
        if kind == "session_meta" {
            matched = payload.and_then(|p| p.get("cwd")).and_then(|v| v.as_str()) == Some(cwd)
                && ts.is_some_and(|t| t >= since_ms);
            if !matched {
                return false;
            }
            prev_ts = ts;
            continue;
        }
        if kind == "compacted" {
            track.compacted(None);
            continue;
        }
        if kind == "event_msg" && ptype == "token_count" {
            if let Some(info) = payload.and_then(|p| p.get("info")) {
                let input = info.get("last_token_usage").and_then(|u| u.get("input_tokens")).and_then(|v| v.as_u64());
                if let Some(input) = input {
                    let total = info.get("total_token_usage").map(ToString::to_string);
                    if total.is_none() || total != last_request {
                        track.request(input);
                    }
                    last_request = total;
                }
                if let (Some(ts), Some(input)) = (ts, input) {
                    latest_context(&mut local, ContextUsage {
                        timestamp_ms: ts, input,
                        cached: info.get("last_token_usage").and_then(|u| u.get("cached_input_tokens")).and_then(|v| v.as_u64()),
                        cache_write: None,
                        capacity: info.get("model_context_window").and_then(|v| v.as_u64()),
                        ..Default::default()
                    });
                }
            }
            if let Some(total) = payload.and_then(|p| p.get("info")).and_then(|i| i.get("total_token_usage")) {
                let get = |k: &str| total.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let cached = get("cached_input_tokens");
                last_total = Some((
                    get("input_tokens").saturating_sub(cached),
                    cached,
                    get("output_tokens") + get("reasoning_output_tokens"),
                ));
            }
            continue;
        }
        if kind != "response_item" {
            continue;
        }
        let Some(ts) = ts else { continue };
        let gap = prev_ts.map(|p| ts.saturating_sub(p)).unwrap_or(0);
        match ptype {
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                local.model_ms += gap;
                let id = payload
                    .and_then(|p| p.get("call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let command = payload
                    .and_then(|p| p.get("arguments"))
                    .and_then(|v| v.as_str())
                    .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                    .and_then(|a| {
                        let c = a.get("cmd").or_else(|| a.get("command"))?;
                        Some(match c {
                            serde_json::Value::Array(parts) => parts
                                .iter()
                                .filter_map(|p| p.as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                            other => other.as_str().unwrap_or("").to_string(),
                        })
                    });
                pending.insert(id, (ts, command));
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = payload.and_then(|p| p.get("call_id")).and_then(|v| v.as_str()).unwrap_or("");
                match pending.remove(id) {
                    Some((start, command)) => add_tool(&mut local, command.as_deref(), ts.saturating_sub(start)),
                    None => local.tool_ms += gap,
                }
            }
            "message" => {
                if payload.and_then(|p| p.get("role")).and_then(|v| v.as_str()) == Some("user") {
                    local.wait_ms += gap;
                } else {
                    local.model_ms += gap;
                }
            }
            _ => local.model_ms += gap,
        }
        prev_ts = Some(ts);
    }
    if !matched {
        return false;
    }
    if let Some((input, cached, output)) = last_total {
        local.input = input;
        local.cached = cached;
        local.output = output;
    }
    track.stamp(local.context.as_mut());
    merge(summary, &local);
    true
}

fn merge(into: &mut TimelineSummary, from: &TimelineSummary) {
    if let Some(context) = &from.context { latest_context(into, context.clone()); }
    into.install_ms += from.install_ms;
    into.build_ms += from.build_ms;
    into.test_ms += from.test_ms;
    into.tool_ms += from.tool_ms;
    into.wait_ms += from.wait_ms;
    into.model_ms += from.model_ms;
    into.input += from.input;
    into.cached += from.cached;
    into.output += from.output;
}

/// Everything a background job needs; copied out of the App's `AgentRun`.
#[derive(Debug, Clone)]
pub(crate) struct TimelineRequest {
    pub(crate) repo_root: PathBuf,
    pub(crate) run_id: String,
    pub(crate) backend: Backend,
    pub(crate) cwd: PathBuf,
    pub(crate) created_at: String,
}

impl TimelineRequest {
    pub(crate) fn for_run(repo_root: &Path, run: &AgentRun) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            run_id: run.id.clone(),
            backend: run.backend,
            cwd: run.cwd.clone(),
            created_at: run.created_at.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TimelineJob {
    pub(crate) run_id: String,
    pub(crate) summary: TimelineSummary,
}

fn timeline_path(repo_root: &Path, run_id: &str) -> PathBuf {
    native_run_dir(repo_root, run_id).join(TIMELINE_FILE)
}

/// Append one JSON event (a `ts` field is added). Best effort: telemetry must
/// never fail a launch.
pub(crate) fn append_timeline_event(repo_root: &Path, run_id: &str, mut event: serde_json::Value) {
    if run_id.trim().is_empty() {
        return;
    }
    if let Some(map) = event.as_object_mut() {
        map.insert("ts".into(), serde_json::json!(system_time_to_iso(SystemTime::now())));
    }
    let path = timeline_path(repo_root, run_id);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{event}");
    }
}

/// Workspace start time and the last recorded summary from the timeline file.
pub(crate) fn read_timeline_file(repo_root: &Path, run_id: &str) -> (Option<u64>, Option<TimelineSummary>) {
    let Ok(content) = fs::read_to_string(timeline_path(repo_root, run_id)) else {
        return (None, None);
    };
    let mut start = None;
    let mut last = None;
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("kind").and_then(|v| v.as_str()) {
            Some("workspace") => start = value.get("ms").and_then(|v| v.as_u64()).or(start),
            Some("summary") => last = serde_json::from_value(value).ok().or(last),
            _ => {}
        }
    }
    (start, last)
}

/// Recompute the run's summary from its session log, and append it to the
/// timeline file when it changed. Runs off the App loop.
pub(crate) fn compute_timeline(request: &TimelineRequest) -> TimelineSummary {
    let home = user_home_dir();
    compute_timeline_in(
        request,
        home.as_ref().map(|h| h.join(".claude/projects")).as_deref(),
        home.as_ref().map(|h| h.join(".codex/sessions")).as_deref(),
    )
}

pub(crate) fn compute_timeline_in(
    request: &TimelineRequest,
    claude_projects: Option<&Path>,
    codex_sessions: Option<&Path>,
) -> TimelineSummary {
    let since_iso = created_at_to_iso(&request.created_at);
    let since_ms = iso_millis(&since_iso).unwrap_or(0);
    let mut summary = TimelineSummary::default();
    match request.backend {
        Backend::Claude => {
            if let Some(root) = claude_projects {
                let dir = root.join(encode_claude_projects_cwd(&request.cwd));
                for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "jsonl")
                        && !file_modified_before(&path, &since_iso)
                    {
                        if let Ok(content) = fs::read_to_string(&path) {
                            analyze_claude_transcript(&content, since_ms, &mut summary);
                        }
                    }
                }
            }
        }
        Backend::Codex => {
            if let Some(root) = codex_sessions {
                let mut files = Vec::new();
                collect_jsonl_files(root, &mut files, 4);
                let cwd = request.cwd.display().to_string();
                for file in files {
                    if file_modified_before(&file, &since_iso) {
                        continue;
                    }
                    if let Ok(content) = fs::read_to_string(&file) {
                        analyze_codex_rollout(&content, &cwd, since_ms, &mut summary);
                    }
                }
            }
        }
        Backend::Opencode => {}
    }
    let (start, last) = read_timeline_file(&request.repo_root, &request.run_id);
    summary.start_ms = start;
    let has_data = summary.context.is_some() || summary.model_ms + summary.input + summary.output > 0;
    // The file never holds the growth history, so compare without it.
    let mut recorded = summary.clone();
    if let Some(c) = recorded.context.as_mut() {
        c.history.clear();
    }
    if has_data && last.as_ref() != Some(&recorded) {
        if let Ok(mut value) = serde_json::to_value(&recorded) {
            if let Some(map) = value.as_object_mut() {
                map.insert("kind".into(), serde_json::json!("summary"));
            }
            append_timeline_event(&request.repo_root, &request.run_id, value);
        }
    } else if !has_data {
        if let Some(mut last) = last {
            last.start_ms = start;
            return last;
        }
    }
    summary
}

pub(crate) fn spawn_timeline_job(tx: mpsc::Sender<TimelineJob>, request: TimelineRequest) {
    let _ = std::thread::Builder::new()
        .name("rudder-timeline".into())
        .spawn(move || {
            let summary = compute_timeline(&request);
            let _ = tx.send(TimelineJob { run_id: request.run_id, summary });
        });
}

pub(crate) fn format_duration_ms(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 10.0 {
        let s = format!("{secs:.1}s");
        if s == "0.0s" { "0s".into() } else { s }
    } else if secs < 60.0 {
        format!("{}s", secs.round() as u64)
    } else if secs < 3600.0 {
        let m = (secs / 60.0).floor() as u64;
        let s = (secs - m as f64 * 60.0).round() as u64;
        if s == 0 { format!("{m}m") } else { format!("{m}m{s:02}s") }
    } else {
        let h = (secs / 3600.0).floor() as u64;
        let m = ((secs - h as f64 * 3600.0) / 60.0).round() as u64;
        format!("{h}h{m:02}m")
    }
}

/// `start 0.8s · install 0s · build 12s · test 40s · model 3m · wait 1m · 1.2M tok, 92% cached`
pub(crate) fn format_timeline(summary: &TimelineSummary) -> String {
    let mut parts = Vec::new();
    if let Some(start) = summary.start_ms {
        parts.push(format!("start {}", format_duration_ms(start)));
    }
    parts.push(format!("install {}", format_duration_ms(summary.install_ms)));
    parts.push(format!("build {}", format_duration_ms(summary.build_ms)));
    parts.push(format!("test {}", format_duration_ms(summary.test_ms)));
    if summary.tool_ms > 0 {
        parts.push(format!("tools {}", format_duration_ms(summary.tool_ms)));
    }
    parts.push(format!("model {}", format_duration_ms(summary.model_ms)));
    if summary.wait_ms > 0 {
        parts.push(format!("wait {}", format_duration_ms(summary.wait_ms)));
    }
    let total_in = summary.input + summary.cached;
    let total = total_in + summary.output;
    if total > 0 {
        let pct = if total_in > 0 { summary.cached * 100 / total_in } else { 0 };
        parts.push(format!("{} tok, {pct}% cached", format_token_count(total)));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_uses_latest_request_and_can_shrink() {
        let mut summary = TimelineSummary::default();
        for (ts, input) in [(2, 1000), (3, 10), (1, 5000)] {
            let row = serde_json::json!({"type":"assistant", "timestamp":format!("2026-11-04T00:00:0{ts}Z"),
                "message":{"usage":{"input_tokens":input,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}}});
            analyze_claude_transcript(&row.to_string(), 0, &mut summary);
        }
        let c = summary.context.as_ref().unwrap();
        assert_eq!((c.input, c.cached, c.cache_write, c.capacity), (60, Some(20), Some(30), None));
        assert!(context_lines(&summary)[1].contains("10 fresh"));
    }

    #[test]
    fn codex_context_uses_last_usage_not_session_total_or_capacity() {
        let content = [
            serde_json::json!({"type":"session_meta","timestamp":"2026-11-04T00:00:00Z","payload":{"cwd":"/repo"}}),
            serde_json::json!({"type":"event_msg","timestamp":"2026-11-04T00:00:01Z","payload":{"type":"token_count","info":{
                "model_context_window":1000,"last_token_usage":{"input_tokens":250,"cached_input_tokens":200},
                "total_token_usage":{"input_tokens":9000}}}}),
        ].iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
        let mut summary = TimelineSummary::default();
        assert!(analyze_codex_rollout(&content, "/repo", 0, &mut summary));
        assert_eq!(summary.context.as_ref().unwrap().input, 250);
        assert!(context_lines(&summary)[0].contains("25%"));
        assert!(context_lines(&summary)[1].contains("50 fresh"));
        let mut other = TimelineSummary::default();
        assert!(!analyze_codex_rollout(&content, "/other", 0, &mut other));
        assert!(other.context.is_none());
    }

    #[test]
    fn first_request_base_history_and_auto_compaction_come_from_the_transcript() {
        let t = |s: u64| format!("2026-11-04T00:00:{s:02}.000Z");
        let request = |s: u64, id: &str, fresh: u64, read: u64, write: u64| {
            serde_json::json!({"type":"assistant","timestamp":t(s),"message":{"id":id,"usage":{
                "input_tokens":fresh,"cache_read_input_tokens":read,"cache_creation_input_tokens":write}}}).to_string()
        };
        let lines = [
            request(1, "m1", 10, 0, 30_000),
            // A second streamed block of the same response: not a new request.
            request(1, "m1", 10, 0, 30_000),
            request(2, "m2", 5, 30_010, 2_000),
            serde_json::json!({"type":"system","subtype":"compact_boundary","timestamp":t(3),
                "compactMetadata":{"trigger":"auto","preTokens":160_000}}).to_string(),
            request(4, "m3", 3, 0, 9_000),
            serde_json::json!({"type":"assistant","isSidechain":true,"timestamp":t(5),"message":{"id":"s1","usage":{
                "input_tokens":1,"cache_read_input_tokens":90_000,"cache_creation_input_tokens":0}}}).to_string(),
        ];
        let mut summary = TimelineSummary::default();
        // The run started after the first two requests; they still set the
        // conversation's history, and the compaction reset its base.
        analyze_claude_transcript(&lines.join("\n"), iso_millis(&t(3)).unwrap(), &mut summary);
        let c = summary.context.as_ref().unwrap();
        assert_eq!(c.input, 9_003);
        assert_eq!(c.base, Some(9_003));
        assert_eq!(c.history, vec![30_010, 32_015, 9_003]);
        assert_eq!(c.auto_compact_at, Some(160_000));
        // timeline.jsonl keeps base and compaction but not the history.
        let row = serde_json::to_value(c).unwrap();
        assert_eq!(row["base"], 9_003);
        assert!(row.get("history").is_none());
    }

    #[test]
    fn missing_context_is_not_reported_as_zero() {
        let mut summary = TimelineSummary::default();
        analyze_claude_transcript(r#"{"type":"assistant","timestamp":"2026-11-04T00:00:00Z","message":{"usage":{"input_tokens":5}}}"#, 0, &mut summary);
        assert!(summary.context.is_none());
        assert_eq!(context_lines(&summary), vec!["Context: waiting for backend usage"]);
    }

    #[test]
    fn iso_millis_parses_utc_timestamps() {
        assert_eq!(iso_millis("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(iso_millis("2026-11-04T00:00:00.000Z"), Some(1_793_750_400_000));
        assert_eq!(iso_millis("nope"), None);
    }

    #[test]
    fn classifies_commands() {
        assert_eq!(classify_command("npm ci"), CommandKind::Install);
        assert_eq!(classify_command("cd x && npm test -- foo"), CommandKind::Test);
        assert_eq!(classify_command("cargo test -p rudder"), CommandKind::Test);
        assert_eq!(classify_command("npm run build"), CommandKind::Build);
        assert_eq!(classify_command("ls -la"), CommandKind::Other);
    }

    #[test]
    fn claude_transcript_splits_model_tools_and_wait() {
        let t = |s: u64| format!("2026-11-04T00:00:{s:02}.000Z");
        let lines = [
            format!(r#"{{"type":"user","timestamp":"{}","message":{{"content":"hi"}}}}"#, t(0)),
            format!(r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"m1","usage":{{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":5}},"content":[{{"type":"tool_use","id":"t1","input":{{"command":"npm test"}}}}]}}}}"#, t(3)),
            format!(r#"{{"type":"user","timestamp":"{}","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1"}}]}}}}"#, t(13)),
            format!(r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"m2","usage":{{"input_tokens":1,"cache_read_input_tokens":99,"output_tokens":5}},"content":[{{"type":"text"}}]}}}}"#, t(15)),
            format!(r#"{{"type":"user","timestamp":"{}","message":{{"content":"more"}}}}"#, t(45)),
        ];
        let mut summary = TimelineSummary::default();
        analyze_claude_transcript(&lines.join("\n"), 0, &mut summary);
        assert_eq!(summary.model_ms, 5000);
        assert_eq!(summary.test_ms, 10_000);
        assert_eq!(summary.wait_ms, 30_000);
        assert_eq!((summary.input, summary.cached, summary.output), (11, 189, 10));
        let line = format_timeline(&summary);
        assert!(line.contains("test 10s"), "{line}");
        assert!(line.contains("model 5.0s"), "{line}");
        assert!(line.contains("94% cached"), "{line}");
    }

    #[test]
    fn timeline_file_round_trips_workspace_and_summary() {
        let dir = std::env::temp_dir().join(format!("rudder-timeline-{}", std::process::id()));
        append_timeline_event(&dir, "r1", serde_json::json!({"kind":"workspace","ms":800}));
        let request = TimelineRequest {
            repo_root: dir.clone(),
            run_id: "r1".into(),
            backend: Backend::Opencode,
            cwd: dir.clone(),
            created_at: "0".into(),
        };
        let summary = compute_timeline_in(&request, None, None);
        assert_eq!(summary.start_ms, Some(800));
        assert!(format_timeline(&summary).starts_with("start 0.8s"));
        let _ = fs::remove_dir_all(&dir);
    }
}
