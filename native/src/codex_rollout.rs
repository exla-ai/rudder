//! A Codex worker's turn and waiting prompts, read from its rollout
//! (`<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`).
//!
//! Two facts the dashboard row needs and no hook gives (measured on 0.170):
//! - **The turn.** `task_started` opens one and `task_complete` or
//!   `turn_aborted` closes it. A keystroke that starts nothing (Esc, F2, `/`,
//!   Backspace) writes nothing here, so a Done row reopens only for a real turn,
//!   including the ones no prompt starts (a goal continuing, a plan approved).
//! - **What waits on the user.** `request_user_input`, an accepted
//!   `request_user_input_async` with unanswered questions, and a finished plan
//!   on "Implement this plan?". These rules mirror `pendingCodexAsks` and
//!   `newerCodexThread` in cloud/worker/asks.mjs (the phone's cards). Both are
//!   pinned to the same real rollouts and expectations,
//!   tests/fixtures/codex-0.170-rollout*.{jsonl,json}: a rule changed on one
//!   side fails the other side's test until it is mirrored.
//! - **When a plan stops waiting.** That menu belongs to Plan mode, and leaving
//!   Plan mode (Shift+Tab, a `/model` pick) writes `thread_settings_applied`
//!   with the new mode. Esc on the menu, "No, stay in Plan mode" and a resumed
//!   session (it does not show the menu again) write nothing at all, so the
//!   dashboard also asks the pane's screen (`PLAN_MENU`, see
//!   `App::poll_codex_rollouts`).
//!
//! The scan runs on its own thread and returns typed results; the App loop
//! only applies them (AGENTS.md: no file IO in the loop).
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How often the dashboard rescans live Codex workers' rollouts.
pub(crate) const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// asks.mjs `CODEX_TAIL_BYTES`: the turn and its prompts are in the tail.
const TAIL_BYTES: u64 = 512 * 1024;
/// asks.mjs `CODEX_HEAD_BYTES`: a rollout's first line is its `session_meta`.
const HEAD_BYTES: u64 = 16 * 1024;
/// A thread whose rollout is not on disk yet is looked for again after this.
const MISS_RECHECK: Duration = Duration::from_secs(5);
/// The title of Codex's plan menu, on the pane's live screen while it is up.
pub(crate) const PLAN_MENU: &str = "Implement this plan?";

/// A prompt the rollout leaves waiting, named as asks.mjs names the card's `tool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodexWait {
    /// `request_user_input`: Plan mode, the pane waits on it.
    Question,
    /// `request_user_input_async`: the turn goes on, the questions wait in the
    /// pane's "Queued follow-up inputs".
    AsyncQuestion,
    /// A finished plan on "Implement this plan?".
    Plan,
}

#[cfg(test)]
impl CodexWait {
    fn tool(self) -> &'static str {
        match self {
            CodexWait::Question => "request_user_input",
            CodexWait::AsyncQuestion => "request_user_input_async",
            CodexWait::Plan => "Plan",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RolloutFacts {
    /// The running turn: a `task_started` with no `task_complete` or
    /// `turn_aborted` after it.
    pub(crate) open_turn: Option<String>,
    /// Oldest first, in asks.mjs's order: question, async question, plan.
    pub(crate) waits: Vec<CodexWait>,
    /// When the waiting plan finished (epoch ms), for the newer-thread rule.
    pub(crate) plan_at_ms: Option<i64>,
}

struct AsyncCall {
    call_id: String,
    questions: usize,
    answered: Vec<usize>,
    accepted: bool,
}

struct Plan {
    turn_id: Option<String>,
    at_ms: i64,
    has_text: bool,
    done: bool,
}

fn str_at<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

/// The questions of a `request_user_input(_async)` call, or None when Codex
/// would refuse it (no `questions`, an empty list, no call id).
fn call_questions(payload: &serde_json::Value) -> Option<(String, usize)> {
    let arguments: serde_json::Value = serde_json::from_str(str_at(payload, "arguments")?).ok()?;
    let questions = arguments.get("questions")?.as_array()?.len();
    let call_id = str_at(payload, "call_id").filter(|id| !id.is_empty())?;
    (questions > 0).then(|| (call_id.to_string(), questions))
}

/// The (call, question index) pairs a `request_user_input_async` reply answers.
/// Codex sends the reply as a user message:
/// `<send_user_message_question_reply>[{answer, question, questionItemId:
/// "[\"request_user_input_async\",\"<call>\",<index>]"}]</…>`.
fn async_replies(content: Option<&serde_json::Value>) -> Vec<(String, usize)> {
    const OPEN: &str = "<send_user_message_question_reply>";
    const CLOSE: &str = "</send_user_message_question_reply>";
    let mut replies = Vec::new();
    for part in content.and_then(serde_json::Value::as_array).into_iter().flatten() {
        let text = str_at(part, "text").unwrap_or_default();
        let Some(body) = text
            .find(OPEN)
            .map(|start| &text[start + OPEN.len()..])
            .and_then(|rest| rest.find(CLOSE).map(|end| &rest[..end]))
        else {
            continue;
        };
        let Ok(serde_json::Value::Array(items)) = serde_json::from_str(body) else {
            continue;
        };
        for item in items {
            let id = str_at(&item, "questionItemId")
                .and_then(|id| serde_json::from_str::<serde_json::Value>(id).ok());
            let Some([kind, call, index]) = id.as_ref().and_then(|id| id.as_array()).map(Vec::as_slice)
            else {
                continue;
            };
            if kind.as_str() == Some("request_user_input_async") {
                if let (Some(call), Some(index)) = (call.as_str(), index.as_u64()) {
                    replies.push((call.to_string(), index as usize));
                }
            }
        }
    }
    replies
}

/// The turn and the waiting prompts that a rollout's lines leave, by the rules
/// of asks.mjs `pendingCodexAsks` (plus the turn, which asks.mjs does not need).
pub(crate) fn rollout_facts<'a>(lines: impl IntoIterator<Item = &'a str>) -> RolloutFacts {
    let mut open_turn: Option<String> = None;
    let mut question: Option<String> = None;
    let mut plan: Option<Plan> = None;
    // This turn's async calls in call order. Their pane goes when the turn
    // ends, answered or not.
    let mut later: Vec<AsyncCall> = Vec::new();
    for line in lines {
        if !line.contains("\"request_user_input")
            && !line.contains("\"event_msg\"")
            && !line.contains("\"function_call_output\"")
            && !line.contains("send_user_message_question_reply")
        {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = entry.get("payload").cloned().unwrap_or(serde_json::Value::Null);
        let kind = str_at(&payload, "type");
        let at_ms = str_at(&entry, "timestamp")
            .and_then(crate::cloudio::parse_iso8601_millis)
            .unwrap_or(0);
        match str_at(&entry, "type") {
            Some("response_item") => match kind {
                Some("function_call") => {
                    let name = str_at(&payload, "name");
                    if name == Some("request_user_input") {
                        question = call_questions(&payload).map(|(call_id, _)| call_id);
                    } else if name == Some("request_user_input_async") {
                        if let Some((call_id, questions)) = call_questions(&payload) {
                            // A Map.set in asks.mjs: a repeated id keeps its place.
                            let call = AsyncCall { call_id, questions, answered: Vec::new(), accepted: false };
                            match later.iter_mut().find(|known| known.call_id == call.call_id) {
                                Some(known) => *known = call,
                                None => later.push(call),
                            }
                        }
                    }
                }
                Some("function_call_output") => {
                    let call_id = str_at(&payload, "call_id");
                    if call_id.is_some() && call_id == question.as_deref() {
                        question = None;
                    }
                    // `{"accepted":true}` once the pane shows it; anything else
                    // is an error the model gets back, and nothing was asked.
                    if let Some(at) = later.iter().position(|call| Some(call.call_id.as_str()) == call_id) {
                        let accepted = str_at(&payload, "output")
                            .and_then(|output| serde_json::from_str::<serde_json::Value>(output).ok())
                            .is_some_and(|output| output.get("accepted") == Some(&serde_json::Value::Bool(true)));
                        if accepted {
                            later[at].accepted = true;
                        } else {
                            later.remove(at);
                        }
                    }
                }
                Some("message") if str_at(&payload, "role") == Some("user") => {
                    for (call_id, index) in async_replies(payload.get("content")) {
                        if let Some(call) = later.iter_mut().find(|call| call.call_id == call_id) {
                            call.answered.push(index);
                        }
                    }
                }
                _ => {}
            },
            Some("event_msg") => match kind {
                Some("task_started") => {
                    open_turn = str_at(&payload, "turn_id").map(str::to_string);
                    question = None;
                    plan = None;
                    later.clear();
                }
                Some("turn_aborted") => {
                    open_turn = None;
                    question = None;
                    plan = None;
                    later.clear();
                }
                Some("task_complete") => {
                    open_turn = None;
                    question = None;
                    later.clear();
                    let turn_id = str_at(&payload, "turn_id");
                    plan = plan
                        .filter(|plan| plan.turn_id.as_deref() == turn_id)
                        .map(|plan| Plan { done: true, ..plan });
                }
                // A Plan mode menu: applying another mode closes it.
                Some("thread_settings_applied") => {
                    let mode = payload
                        .pointer("/thread_settings/collaboration_mode/mode")
                        .and_then(serde_json::Value::as_str);
                    if mode.is_some_and(|mode| mode != "plan") {
                        plan = None;
                    }
                }
                Some("item_completed") => {
                    let item = payload.get("item");
                    let text = item.and_then(|item| str_at(item, "text"));
                    if item.and_then(|item| str_at(item, "type")) == Some("Plan") {
                        if let Some(text) = text {
                            plan = Some(Plan {
                                turn_id: str_at(&payload, "turn_id").map(str::to_string),
                                at_ms,
                                has_text: !text.trim().is_empty(),
                                done: false,
                            });
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    let mut waits = Vec::new();
    if question.is_some() {
        waits.push(CodexWait::Question);
    }
    if later
        .iter()
        .any(|call| call.accepted && (0..call.questions).any(|index| !call.answered.contains(&index)))
    {
        waits.push(CodexWait::AsyncQuestion);
    }
    let plan_at_ms = plan.filter(|plan| plan.done && plan.has_text).map(|plan| plan.at_ms);
    if plan_at_ms.is_some() {
        waits.push(CodexWait::Plan);
    }
    RolloutFacts { open_turn, waits, plan_at_ms }
}

/// One live Codex worker to scan.
pub(crate) struct ScanRequest {
    pub(crate) run_id: String,
    /// The thread the dashboard recorded for the run.
    pub(crate) session_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ScanResult {
    pub(crate) run_id: String,
    /// Taken before the rollout was read. A result read before the row last
    /// completed cannot reopen it: that read may predate the turn's end.
    pub(crate) read_at: Instant,
    /// A turn is running that is not the one the worker last reported done
    /// (its signal script's `<signal>.turn`, written before every `done`).
    pub(crate) new_turn: bool,
    pub(crate) waits: Vec<CodexWait>,
    /// The worker moved to a newer thread (`/new`, `/clear`, "clear context
    /// and implement"): `(pinned, newer)`. The App adopts `newer` only while
    /// the row is still on `pinned`.
    pub(crate) moved: Option<(String, String)>,
}

/// Reads each worker's rollout. Runs on the scan thread.
pub(crate) fn scan(requests: &[ScanRequest]) -> Vec<ScanResult> {
    let sessions = crate::chat_edit::codex_home().ok().map(|home| home.join("sessions"));
    requests
        .iter()
        .map(|request| {
            let read_at = Instant::now();
            let signal = crate::signals::signal_path(&request.run_id);
            let sidecar = |suffix: &str| {
                signal
                    .as_ref()
                    .and_then(|path| std::fs::read_to_string(format!("{}.{suffix}", path.display())).ok())
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
            };
            let rollout = |id: &str| sessions.as_ref().and_then(|sessions| rollout_file(sessions, id));
            let pinned = request.session_id.as_deref().and_then(rollout);
            // The thread this worker's own SessionStart hook last named (the
            // signal script is per run, so no other process writes it).
            let reported = sidecar("session")
                .filter(|id| request.session_id.as_ref() != Some(id))
                .and_then(|id| rollout(&id).map(|path| (id, path)));
            let (read, moved) = match (&request.session_id, pinned, reported) {
                // Not pinned yet (`ensure_session_ids_recorded` pins it).
                (None, _, Some((_, path))) => (Some(path), None),
                // A thread begun after the pinned one, in its folder, is where
                // the conversation went. An older one is a report the row has
                // moved past (an edit relaunch pins its fork before the new
                // process reports it).
                (Some(was), Some(pinned), Some((id, path))) if started_after(&pinned, &path) => {
                    (Some(path), Some((was.clone(), id)))
                }
                (_, pinned, _) => (pinned, None),
            };
            let facts = read
                .map(|rollout| {
                    let mut facts = read_facts(&rollout);
                    if let Some(since) = facts.plan_at_ms {
                        if newer_thread(&rollout, since) {
                            facts.waits.retain(|wait| *wait != CodexWait::Plan);
                        }
                    }
                    facts
                })
                .unwrap_or_default();
            // Read after the rollout, so a turn that ended in between is seen
            // ended here.
            let done_turn = sidecar("turn");
            ScanResult {
                run_id: request.run_id.clone(),
                read_at,
                new_turn: facts.open_turn.is_some() && facts.open_turn != done_turn,
                waits: facts.waits,
                moved,
            }
        })
        .collect()
}

/// Whether `later` began after `earlier` in the same folder.
fn started_after(earlier: &Path, later: &Path) -> bool {
    match (rollout_head(earlier), rollout_head(later)) {
        (Some((then, folder)), Some((now, cwd))) => now > then && cwd == folder,
        _ => false,
    }
}

fn cache<K, V>(cell: &'static OnceLock<Mutex<HashMap<K, V>>>) -> std::sync::MutexGuard<'static, HashMap<K, V>> {
    cell.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `<sessions>/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`. Rollouts never move,
/// so a found path is kept; a miss is looked for again after `MISS_RECHECK`
/// (Codex writes the file at its first turn).
fn rollout_file(sessions: &Path, thread: &str) -> Option<PathBuf> {
    static FOUND: OnceLock<Mutex<HashMap<String, (Option<PathBuf>, Instant)>>> = OnceLock::new();
    if let Some((path, checked)) = cache(&FOUND).get(thread) {
        if path.as_ref().is_some_and(|path| path.exists()) {
            return path.clone();
        }
        if path.is_none() && checked.elapsed() < MISS_RECHECK {
            return None;
        }
    }
    let suffix = format!("-{thread}.jsonl");
    let mut found = None;
    let mut dirs = vec![(sessions.to_path_buf(), 0)];
    'walk: while let Some((dir, depth)) = dirs.pop() {
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
                found = Some(path);
                break 'walk;
            }
        }
    }
    cache(&FOUND).insert(thread.to_string(), (found.clone(), Instant::now()));
    found
}

/// The facts of a rollout's tail, reread only when the file changed.
fn read_facts(rollout: &Path) -> RolloutFacts {
    static READ: OnceLock<Mutex<HashMap<PathBuf, (u64, SystemTime, RolloutFacts)>>> = OnceLock::new();
    let Ok(meta) = std::fs::metadata(rollout) else {
        return RolloutFacts::default();
    };
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    if let Some((size, at, facts)) = cache(&READ).get(rollout) {
        if *size == meta.len() && *at == modified {
            return facts.clone();
        }
    }
    let Some(text) = read_tail(rollout, TAIL_BYTES) else {
        return RolloutFacts::default();
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    if meta.len() > TAIL_BYTES && !lines.is_empty() {
        lines.remove(0); // cut mid-line
    }
    let facts = rollout_facts(lines);
    cache(&READ).insert(rollout.to_path_buf(), (meta.len(), modified, facts.clone()));
    facts
}

fn read_tail(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(bytes))).ok()?;
    let mut buffer = Vec::new();
    file.take(bytes).read_to_end(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// When a rollout's session began and in which folder, read as asks.mjs
/// `codexHead` reads it: the first line's `{"timestamp":"…"` and the first
/// `"cwd"` after `"type":"session_meta","payload":{`. Not a JSON parse: since
/// 0.166 that line carries the base instructions and runs past 20 KiB (26 KiB
/// on 0.170), while `cwd` comes before them. Never changes once written, so it
/// is kept.
fn rollout_head(path: &Path) -> Option<(i64, String)> {
    static HEADS: OnceLock<Mutex<HashMap<PathBuf, (i64, String)>>> = OnceLock::new();
    if let Some(head) = cache(&HEADS).get(path) {
        return Some(head.clone());
    }
    let mut buffer = Vec::new();
    std::fs::File::open(path).ok()?.take(HEAD_BYTES).read_to_end(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer);
    let head = parse_head(text.lines().next()?)?;
    cache(&HEADS).insert(path.to_path_buf(), head.clone());
    Some(head)
}

fn parse_head(first_line: &str) -> Option<(i64, String)> {
    let stamp = first_line.strip_prefix(r#"{"timestamp":""#)?;
    let started = crate::cloudio::parse_iso8601_millis(&stamp[..stamp.find('"')?])?;
    const META: &str = r#""type":"session_meta","payload":{"#;
    let payload = &first_line[first_line.find(META)? + META.len()..];
    let cwd = &payload[payload.find(r#""cwd":"#)? + r#""cwd":"#.len()..];
    // One JSON string, whatever follows it.
    let cwd = serde_json::Deserializer::from_str(cwd).into_iter::<String>().next()?.ok()?;
    Some((started, cwd))
}

/// asks.mjs `newerCodexThread`: whether Codex moved on from `rollout` to a
/// newer thread in the same folder after `since_ms`. "Yes, clear context and
/// implement" (and `/new`) does that and writes nothing more to the old
/// thread, so a plan waiting there is over. Looks in the rollout's own day and
/// today's (local time, as Codex files them).
fn newer_thread(rollout: &Path, since_ms: i64) -> bool {
    let Some((_, own_cwd)) = rollout_head(rollout) else {
        return false;
    };
    let mut dirs: Vec<PathBuf> = rollout.parent().map(Path::to_path_buf).into_iter().collect();
    if let Some(today) = rollout
        .ancestors()
        .nth(4)
        .and_then(local_day_dir)
        .filter(|today| !dirs.contains(today))
    {
        dirs.push(today);
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path == rollout || !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let modified_ms = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |at| at.as_millis() as i64);
            if modified_ms <= since_ms {
                continue;
            }
            if let Some((started, cwd)) = rollout_head(&path) {
                if started > since_ms && cwd == own_cwd {
                    return true;
                }
            }
        }
    }
    false
}

/// `<sessions>/YYYY/MM/DD` for today in local time.
#[cfg(unix)]
fn local_day_dir(sessions: &Path) -> Option<PathBuf> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return None;
    }
    Some(
        sessions
            .join(format!("{:04}", tm.tm_year + 1900))
            .join(format!("{:02}", tm.tm_mon + 1))
            .join(format!("{:02}", tm.tm_mday)),
    )
}

#[cfg(not(unix))]
fn local_day_dir(_sessions: &Path) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLLOUT: &str = include_str!("../../tests/fixtures/codex-0.170-rollout.jsonl");
    const CLEARED: &str = include_str!("../../tests/fixtures/codex-0.170-rollout-cleared.jsonl");
    const EXPECT: &str = include_str!("../../tests/fixtures/codex-0.170-rollout.expect.json");
    const PLAN_LEFT: &str = include_str!("../../tests/fixtures/codex-0.170-rollout-plan-left.jsonl");
    const PLAN_LEFT_EXPECT: &str =
        include_str!("../../tests/fixtures/codex-0.170-rollout-plan-left.expect.json");

    /// Every prefix of two real 0.170 sessions gives the turn and the waits
    /// that asks.mjs gives (its own test reads the same expectations). The
    /// first: a reply, an async question, the same in Plan mode answered in
    /// the pane, a blocking question, two plans. The second: three plans, left
    /// by Shift+Tab out of Plan mode and by a `/model` pick.
    #[test]
    fn a_real_codex_rollout_reads_like_the_phone_cards_at_every_line() {
        for (rollout, expect) in [(ROLLOUT, EXPECT), (PLAN_LEFT, PLAN_LEFT_EXPECT)] {
            let lines: Vec<&str> = rollout.lines().collect();
            let expected: Vec<serde_json::Value> = serde_json::from_str(expect).unwrap();
            assert_eq!(expected.len(), lines.len());
            for step in expected {
                let n = step["lines"].as_u64().unwrap() as usize;
                let facts = rollout_facts(lines[..n].iter().copied());
                assert_eq!(facts.open_turn.as_deref(), step["openTurn"].as_str(), "turn after {n} lines");
                let waits: Vec<&str> = facts.waits.iter().map(|wait| wait.tool()).collect();
                let want: Vec<&str> = step["waits"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
                assert_eq!(waits, want, "waits after {n} lines");
            }
        }
    }

    #[test]
    fn a_waiting_plan_is_over_once_a_newer_thread_starts_in_the_same_folder() {
        // Real `session_meta` lines carry the base instructions (26 KiB on
        // 0.170), past the head that is read; the fixtures were trimmed.
        let full_size = |rollout: &str| {
            let (first, rest) = rollout.split_once('\n').unwrap();
            let pad = "x".repeat(26 * 1024);
            format!("{},\"base_instructions\":{{\"text\":\"{pad}\"}}}}}}\n{rest}", first.strip_suffix("}}").unwrap())
        };
        let root = std::env::temp_dir().join(format!("rudder-codex-rollout-{}", std::process::id()));
        let day = root.join("2026").join("11").join("18");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-11-18T11-26-51-01a1ee68-da13-7822-a7de-75c3368eeed4.jsonl");
        std::fs::write(&rollout, full_size(ROLLOUT)).unwrap();
        let facts = read_facts(&rollout);
        assert_eq!(facts.waits, vec![CodexWait::Plan]);
        let since = facts.plan_at_ms.unwrap();
        assert!(!newer_thread(&rollout, since));
        // "Yes, clear context and implement": the new thread's rollout.
        let cleared = day.join("rollout-2026-11-18T11-31-28-01a1ee6d-12ce-7b62-adf4-508197eeb930.jsonl");
        std::fs::write(&cleared, full_size(CLEARED)).unwrap();
        assert!(newer_thread(&rollout, since));
        // The row follows it there, and never back.
        assert!(started_after(&rollout, &cleared));
        assert!(!started_after(&cleared, &rollout));
        let _ = std::fs::remove_dir_all(&root);
    }
}
