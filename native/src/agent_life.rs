//! Renaming, pinning, closing and deleting agents, and the cleanup that closes
//! agents nobody touched for a day.
//!
//! Closing has one path, `close_agent_now`: `dd`, a delete from the phone,
//! `cc` and the cleanup all end there. The row, its signals and ledger entries
//! go, and its record moves to `.rudder/closed/<id>/`, out of everything that
//! lists agents. Its conversation and workspace stay, so Show closed (a label
//! on the task pane's border, or the app's Show closed group) lists it, and a
//! click, Enter or the app's Reopen brings it back: the record returns and its
//! session resumes through the adopt path's `resume_recorded_agent`. A closed
//! chat is deleted for good (`purge_closed_agent`) CLOSED_AGENT_TTL after it
//! closed, or at once with `d d` in that list.
//!
//! Deleting for good has one path, `purge_agent`: the record and workspace go
//! (the workspace's jj change stays in `jj log`, see
//! `remove_workspace_in_background`), and so does its conversation: every
//! Claude session file the run recorded with the folder Claude keeps next to
//! it, or its Codex rollouts. A rollout another remaining or closed run builds
//! on (`history_base`, written when Codex forks a thread) is kept. Files are
//! removed on a thread; a failure comes back as a notice. Rows with nothing to
//! reopen (a launch cancelled before it started, a failed branch, an agent
//! deleted on the home machine) are deleted for good at once
//! (`delete_agent_now`).
//!
//! The cleanup runs here, in the dashboard, because the dashboard is the one
//! scheduling authority of its project, on a laptop and on the home machine.
//! Its clock starts at its first scan in a project (`.rudder/cleanup-since`),
//! so agents that existed before it get a full day from then.
use super::*;

/// An agent nobody touched for this long is closed (and deleted for good
/// CLOSED_AGENT_TTL later).
pub(crate) const UNTOUCHED_AGENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the dashboard looks for untouched agents.
pub(crate) const AGENT_GC_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Longest agent name. The app's state carries 200 characters of a title.
pub(crate) const AGENT_TITLE_MAX_CHARS: usize = 200;
/// A closed chat waits this long to be reopened, then the cleanup deletes its
/// record, conversation and workspace.
pub(crate) const CLOSED_AGENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// CLOSED_AGENT_TTL as the copy says it.
pub(crate) const CLOSED_KEEP_COPY: &str = "a day";
/// Where a deleted chat went, for every notice that deletes one.
pub(crate) const CLOSED_KEPT_COPY: &str = "kept a day under Show closed in the task pane";
/// When a closed chat closed, next to its record: `{"closedAt": <unix secs>}`.
const CLOSED_STAMP: &str = "closed.json";

/// One closed chat, as Show closed lists it.
#[derive(Clone, Debug)]
pub(crate) struct ClosedAgent {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) backend: Backend,
    pub(crate) closed_at: SystemTime,
    /// Its jj workspace, kept while it is closed. None for a main-checkout chat.
    pub(crate) workspace_name: Option<String>,
    pub(crate) workspace_path: Option<PathBuf>,
}

impl ClosedAgent {
    fn of(run: &AgentRun, closed_at: SystemTime) -> Self {
        Self {
            id: run.id.clone(),
            title: agent_label(run),
            backend: run.backend,
            closed_at,
            workspace_name: run.workspace_name.clone(),
            workspace_path: run.workspace_path.clone(),
        }
    }
}

/// The open Show closed list over the task pane (one per dashboard view).
#[derive(Clone, Debug, Default)]
pub(crate) struct ClosedList {
    pub(crate) selected: usize,
    /// The chat a first `d` asked to delete for good, and the frame it asked
    /// on: the second `d` counts only once that notice has rendered.
    pub(crate) purge_armed: Option<(String, u64)>,
}

/// A run record, its stored paths rebased onto this checkout.
fn read_run_record(repo_root: &Path, path: &Path) -> Result<serde_json::Value, String> {
    let raw = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut record = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    heal_moved_repo_paths(&mut record, repo_root);
    Ok(record)
}

fn read_recorded_agent(repo_root: &Path, path: &Path) -> Result<AgentRun, String> {
    agent_from_run_record(repo_root, read_run_record(repo_root, path)?)
        .ok_or_else(|| format!("{} is missing required fields", path.display()))
}

fn read_closed_agent(repo_root: &Path, dir: &Path) -> Result<ClosedAgent, String> {
    let stamp = dir.join(CLOSED_STAMP);
    let raw = fs::read_to_string(&stamp).map_err(|error| format!("{}: {error}", stamp.display()))?;
    let seconds = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| value.get("closedAt")?.as_u64())
        .ok_or_else(|| format!("{} has no closedAt", stamp.display()))?;
    let run = read_recorded_agent(repo_root, &dir.join("run.json"))?;
    Ok(ClosedAgent::of(&run, UNIX_EPOCH + Duration::from_secs(seconds)))
}

/// Every closed chat of this checkout, newest first.
pub(crate) fn load_closed_agents(repo_root: &Path) -> Vec<ClosedAgent> {
    let Ok(entries) = fs::read_dir(closed_runs_dir(repo_root)) else {
        return Vec::new();
    };
    let mut closed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match read_closed_agent(repo_root, &path) {
            Ok(agent) => closed.push(agent),
            Err(error) => eprintln!("rudder: skipping closed chat {}: {error}", path.display()),
        }
    }
    closed.sort_by(|a, b| b.closed_at.cmp(&a.closed_at));
    closed
}

/// Move a run's record folder to the closed list and stamp when.
fn move_record_to_closed(repo_root: &Path, run_id: &str, closed_at: SystemTime) -> Result<(), String> {
    let from = native_run_dir(repo_root, run_id);
    let to = closed_run_dir(repo_root, run_id);
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    let seconds = closed_at
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    fs::create_dir_all(closed_runs_dir(repo_root))
        .and_then(|()| fs::rename(&from, &to))
        .map_err(|error| format!("{}: {error}", from.display()))?;
    let stamp = to.join(CLOSED_STAMP);
    let temp = to.join(format!("{CLOSED_STAMP}.{}.tmp", std::process::id()));
    fs::write(&temp, format!("{}\n", serde_json::json!({ "closedAt": seconds })))
        .and_then(|()| fs::rename(&temp, &stamp))
        .map_err(|error| format!("{}: {error}", stamp.display()))
}

/// Move a closed chat's record folder back to `.rudder/runs`.
fn move_record_back(repo_root: &Path, run_id: &str) -> Result<(), String> {
    let from = closed_run_dir(repo_root, run_id);
    let to = native_run_dir(repo_root, run_id);
    if to.exists() {
        return Err(format!("{} already exists", to.display()));
    }
    fs::create_dir_all(native_runs_dir(repo_root))
        .and_then(|()| fs::rename(&from, &to))
        .map_err(|error| format!("{}: {error}", from.display()))?;
    fs::remove_file(to.join(CLOSED_STAMP)).map_err(|error| format!("{}: {error}", to.join(CLOSED_STAMP).display()))
}

/// How long ago, as the closed list says it: `5m`, `3h`, `2d`.
pub(crate) fn closed_age_label(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 48 * 60 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / (24 * 60))
    }
}

/// Where conversations live on this machine.
#[derive(Clone, Debug)]
pub(crate) struct ConversationRoots {
    /// `~/.claude/projects`: one folder per working directory.
    pub(crate) claude_projects: PathBuf,
    /// `$CODEX_HOME/sessions`: `YYYY/MM/DD/rollout-<time>-<thread>.jsonl`.
    pub(crate) codex_sessions: PathBuf,
}

impl ConversationRoots {
    pub(crate) fn from_env() -> Result<Self, String> {
        let home = user_home_dir().ok_or_else(|| "HOME is not set".to_string())?;
        Ok(Self {
            claude_projects: home.join(".claude").join("projects"),
            codex_sessions: crate::chat_edit::codex_home()?.join("sessions"),
        })
    }
}

/// One deleted agent's conversation, and what must survive it.
#[derive(Debug)]
pub(crate) struct ConversationCleanup {
    pub(crate) backend: Backend,
    /// Claude names its project folder after the agent's working directory.
    pub(crate) cwd: PathBuf,
    pub(crate) sessions: Vec<String>,
    /// Sessions the remaining runs are on (their records may lag).
    pub(crate) kept_sessions: Vec<String>,
    /// The remaining runs' records, for the sessions they left.
    pub(crate) kept_records: Vec<PathBuf>,
}

/// Codex rollouts by thread id.
type Rollouts = HashMap<String, Vec<PathBuf>>;

/// Index `<sessions>/YYYY/MM/DD/rollout-<time>-<thread>.jsonl` by thread id
/// (a UUID: the last 36 characters of the name).
fn index_rollouts(sessions: &Path) -> Rollouts {
    let mut rollouts = Rollouts::new();
    let mut dirs = vec![(sessions.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
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
            let Some(stem) = name
                .to_str()
                .filter(|name| name.starts_with("rollout-"))
                .and_then(|name| name.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if stem.len() > 36 && stem.is_char_boundary(stem.len() - 36) {
                let thread = &stem[stem.len() - 36..];
                rollouts.entry(thread.to_string()).or_default().push(path);
            }
        }
    }
    rollouts
}

/// The thread a forked rollout builds on (`session_meta.history_base`).
fn history_base_thread(rollout: &Path) -> Option<String> {
    let line = read_first_line(rollout, CODEX_SESSION_META_MAX_BYTES)?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    value
        .get("payload")?
        .get("history_base")?
        .get("thread_id")?
        .as_str()
        .filter(|id| valid_session_id(id))
        .map(ToOwned::to_owned)
}

/// The sessions in use and every thread their rollouts build on, all the way down.
fn codex_threads_in_use(rollouts: &Rollouts, in_use: &HashSet<String>) -> HashSet<String> {
    let mut kept = in_use.clone();
    let mut queue: Vec<String> = kept.iter().cloned().collect();
    while let Some(thread) = queue.pop() {
        for rollout in rollouts.get(&thread).into_iter().flatten() {
            if let Some(base) = history_base_thread(rollout) {
                if kept.insert(base.clone()) {
                    queue.push(base);
                }
            }
        }
    }
    kept
}

fn remove_file_if_present(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn remove_dir_if_present(path: &Path) -> Result<bool, String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Delete one agent's conversation files. Returns how many were removed.
pub(crate) fn delete_conversation(
    roots: &ConversationRoots,
    job: &ConversationCleanup,
) -> Result<usize, String> {
    let mut in_use: HashSet<String> = job.kept_sessions.iter().cloned().collect();
    // A remaining run that has no record yet (still starting) left no
    // sessions. One whose record can't be read might name any of these, so
    // nothing is deleted.
    for record in &job.kept_records {
        let raw = match fs::read_to_string(record) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", record.display())),
        };
        let value = serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|error| format!("{}: {error}", record.display()))?;
        in_use.extend(recorded_session_ids(&value));
    }
    let doomed: Vec<&String> = job
        .sessions
        .iter()
        .filter(|session| valid_session_id(session) && !in_use.contains(*session))
        .collect();
    let mut removed = 0;
    let mut errors = Vec::new();
    let mut count = |result: Result<bool, String>| match result {
        Ok(true) => removed += 1,
        Ok(false) => {}
        Err(error) => errors.push(error),
    };
    match job.backend {
        Backend::Claude => {
            let folder = roots.claude_projects.join(encode_claude_project_dir(&job.cwd));
            for session in doomed {
                count(remove_file_if_present(&folder.join(format!("{session}.jsonl"))));
                // Subagent transcripts and tool output Claude keeps per session.
                count(remove_dir_if_present(&folder.join(session)));
            }
        }
        Backend::Codex => {
            let rollouts = index_rollouts(&roots.codex_sessions);
            let kept = codex_threads_in_use(&rollouts, &in_use);
            for session in doomed {
                if kept.contains(session) {
                    continue;
                }
                for rollout in rollouts.get(session.as_str()).into_iter().flatten() {
                    count(remove_file_if_present(rollout));
                }
            }
        }
        _ => {}
    }
    if errors.is_empty() {
        Ok(removed)
    } else {
        Err(errors.join("; "))
    }
}

/// What the cleanup reads about one agent off the App loop.
#[derive(Clone, Debug)]
pub(crate) struct GcProbe {
    pub(crate) run_id: String,
    pub(crate) record: PathBuf,
    pub(crate) backend: Backend,
    pub(crate) cwd: PathBuf,
    pub(crate) session: Option<String>,
    /// Its jj workspace name and directory, while the directory exists.
    pub(crate) workspace: Option<(String, PathBuf)>,
    /// The jj change it started as, for finding its work once the workspace
    /// is gone.
    pub(crate) change: Option<String>,
    /// Where the phone's questions for it wait (`<id>.json`).
    pub(crate) asks: Option<PathBuf>,
}

fn modified(path: &Path) -> Result<SystemTime, String> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// `<name>@` as a revset, the name quoted.
fn workspace_revset(workspace: &str) -> String {
    format!("\"{}\"@", workspace.replace('\\', "\\\\").replace('"', "\\\""))
}

/// When jj last saw the workspace's files change: its working-copy commit's
/// committer time. Read-only; it does not snapshot anything.
fn workspace_changed_at(repo_root: &Path, workspace: &str) -> Result<SystemTime, String> {
    let revset = workspace_revset(workspace);
    let output = Command::new(jj_program())
        .args(["--ignore-working-copy", "log", "--no-graph", "-r", &revset, "-T"])
        .arg("committer.timestamp().utc().format(\"%s\")")
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("jj: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let seconds: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| format!("jj gave no commit time for {workspace}"))?;
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// The agent has work main does not: a non-empty commit in its workspace's
/// history (snapshotted first, so files it never committed count) or, once
/// the workspace is gone, in its change or anything built on it, that is
/// neither in trunk nor under the main checkout. Untouched means no such
/// work, whatever the agent's status. An error counts as unmerged work: the
/// agent is left alone.
fn has_unmerged_work(repo_root: &Path, probe: &GcProbe) -> bool {
    let revset = match (&probe.workspace, &probe.change) {
        (Some((workspace, path)), _) => {
            if crate::gitio::snapshot_jj_workspace(path).is_err() {
                return true;
            }
            format!("::{}", workspace_revset(workspace))
        }
        (None, Some(change)) if !change.is_empty() && change.chars().all(|c| c.is_ascii_alphanumeric()) => {
            format!("present({change})::")
        }
        (None, Some(_)) => return true,
        (None, None) => return false,
    };
    let output = Command::new(jj_program())
        .args(["--ignore-working-copy", "log", "--no-graph", "--limit", "1", "-r"])
        .arg(format!("({revset}) ~ ::(trunk() | @) ~ empty()"))
        .args(["-T", "commit_id.short() ++ \"\\n\""])
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => !output.stdout.trim_ascii().is_empty(),
        _ => true,
    }
}

/// A question, permission or plan from the agent still waits for an answer.
/// A folder that can't be read counts as waiting: the agent is left alone.
fn has_pending_ask(asks: &Path) -> bool {
    match fs::read_dir(asks) {
        Ok(mut entries) => entries.any(|entry| match entry {
            Ok(entry) => {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".json") && !name.ends_with(".answer.json")
            }
            Err(_) => true,
        }),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

/// The newest change of anything the agent owns: its record, its current
/// conversation and its workspace. An error means it could not be told, and
/// the agent is left alone.
fn last_touched(
    repo_root: &Path,
    roots: &ConversationRoots,
    rollouts: &Rollouts,
    probe: &GcProbe,
) -> Result<SystemTime, String> {
    let mut newest = modified(&probe.record)?;
    if let Some(session) = probe.session.as_deref() {
        let transcripts: Vec<PathBuf> = match probe.backend {
            Backend::Claude => vec![roots
                .claude_projects
                .join(encode_claude_project_dir(&probe.cwd))
                .join(format!("{session}.jsonl"))],
            Backend::Codex => rollouts.get(session).cloned().unwrap_or_default(),
            _ => Vec::new(),
        };
        for transcript in transcripts.iter().filter(|path| path.is_file()) {
            newest = newest.max(modified(transcript)?);
        }
    }
    if let Some((workspace, _)) = &probe.workspace {
        newest = newest.max(workspace_changed_at(repo_root, workspace)?);
    }
    Ok(newest)
}

/// When this project's cleanup clock started. The first scan writes `now`;
/// no agent counts as untouched from before then.
pub(crate) fn cleanup_since(repo_root: &Path, now: SystemTime) -> Result<SystemTime, String> {
    let path = repo_root.join(".rudder").join("cleanup-since");
    let seconds = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|value| value.get("since")?.as_u64())
            .ok_or_else(|| format!("{} has no start time", path.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let seconds = now
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_secs();
            let temp = path.with_extension(format!("{}.tmp", std::process::id()));
            fs::create_dir_all(repo_root.join(".rudder"))
                .and_then(|()| fs::write(&temp, format!("{}\n", serde_json::json!({ "since": seconds }))))
                .and_then(|()| fs::rename(&temp, &path))
                .map_err(|error| format!("{}: {error}", path.display()))?;
            seconds
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    Ok(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// The probed agents nobody touched for `ttl` before `now`, counting from
/// `since` at the earliest.
pub(crate) fn untouched_agents(
    repo_root: &Path,
    roots: &ConversationRoots,
    probes: &[GcProbe],
    since: SystemTime,
    now: SystemTime,
    ttl: Duration,
) -> Vec<String> {
    let rollouts = if probes.iter().any(|probe| probe.backend == Backend::Codex) {
        index_rollouts(&roots.codex_sessions)
    } else {
        Rollouts::new()
    };
    probes
        .iter()
        .filter(|probe| !probe.asks.as_deref().is_some_and(has_pending_ask))
        .filter(|probe| !has_unmerged_work(repo_root, probe))
        .filter(|probe| {
            last_touched(repo_root, roots, &rollouts, probe)
                .is_ok_and(|at| now.duration_since(at.max(since)).is_ok_and(|age| age >= ttl))
        })
        .map(|probe| probe.run_id.clone())
        .collect()
}

fn agent_label(run: &AgentRun) -> String {
    if run.task_summary.trim().is_empty() {
        short_task(&run.task)
    } else {
        truncate_chars(run.task_summary.trim(), 60)
    }
}

impl App {
    /// Name an agent. The dashboard's `r` and the app's rename both end here.
    pub(crate) fn rename_agent_at(&mut self, index: usize, title: &str) -> Result<String, String> {
        let title: String = title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(AGENT_TITLE_MAX_CHARS)
            .collect();
        if title.is_empty() {
            return Err("a name can't be empty".to_string());
        }
        let run = self
            .agents
            .get_mut(index)
            .ok_or_else(|| "no agent selected".to_string())?;
        let previous = std::mem::replace(&mut run.task_summary, title.clone());
        if let Err(error) = save_native_run_record(&self.cwd, run) {
            run.task_summary = previous;
            return Err(format!("could not save the name: {error}"));
        }
        self.renamed_runs.insert(run.id.clone());
        self.dirty = true;
        Ok(format!("renamed to {title}"))
    }

    /// The app renamed an agent (steer kind `rename`, the name as instruction).
    pub(crate) fn deliver_rename_request(&mut self, task_id: &str, title: &str) -> SteerDelivery {
        let Some(index) = self.agent_index_for_token(task_id) else {
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        };
        if self.pending_launches.contains_key(&self.agents[index].id) {
            return SteerDelivery::Failed("it is still starting; rename it in a moment".to_string());
        }
        match self.rename_agent_at(index, title) {
            Ok(notice) => {
                self.notice = Some(notice);
                let _ = self.write_rudder_context_timed(None);
                SteerDelivery::Delivered
            }
            Err(reason) => SteerDelivery::Failed(reason),
        }
    }

    /// The app pinned or unpinned an agent (steer kind `pin`, instruction
    /// `pin` or `unpin`), as `p` does here. The cleanup never deletes a pin.
    pub(crate) fn deliver_pin_request(&mut self, task_id: &str, instruction: &str) -> SteerDelivery {
        let pinned = match instruction.trim() {
            "pin" => true,
            "unpin" => false,
            other => return SteerDelivery::Failed(format!("expected pin or unpin, got {other}")),
        };
        let Some(index) = self.agent_index_for_token(task_id) else {
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        };
        let run = &mut self.agents[index];
        if run.is_orchestrator() {
            return SteerDelivery::Failed("a plan's orchestrator is always at the top".to_string());
        }
        if run.pinned != pinned {
            run.pinned = pinned;
            if let Err(error) = save_native_run_record(&self.cwd, run) {
                run.pinned = !pinned;
                return SteerDelivery::Failed(format!("could not save the pin: {error}"));
            }
        }
        self.dirty = true;
        let _ = self.write_rudder_context_timed(None);
        SteerDelivery::Delivered
    }

    /// Every session Rudder made for this run, read from its `record` before
    /// that is removed. A conversation it was switched to (`/resume`, a
    /// restored session) is the person's and stays.
    pub(crate) fn conversation_of(&self, run: &AgentRun, record: &Path) -> ConversationCleanup {
        let record = fs::read_to_string(record)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        let mut sessions: Vec<String> = record.iter().flat_map(rudder_session_ids).collect();
        sessions.extend(
            record
                .iter()
                .flat_map(recorded_session_ids)
                .chain(run.session_id.clone())
                .filter(|session| is_rudder_session(session)),
        );
        sessions.sort();
        sessions.dedup();
        // A branch still being set up has no session of its own yet, but its
        // fork reads the source's conversation (and a Codex fork keeps
        // reading through it afterwards).
        let branching = self.pending_launches.values().filter_map(|launch| match &launch.kind {
            PendingLaunchKind::Branch(branch) => Some(branch.source_session.clone()),
            _ => None,
        });
        ConversationCleanup {
            backend: run.backend,
            cwd: run.cwd.clone(),
            sessions,
            kept_sessions: self
                .agents
                .iter()
                .filter_map(|other| other.session_id.clone())
                .chain(branching)
                .collect(),
            // Closed chats can still be reopened: their sessions stay too.
            kept_records: self
                .agents
                .iter()
                .map(|other| native_run_dir(&self.cwd, &other.id))
                .chain(self.closed_agents.iter().map(|closed| closed_run_dir(&self.cwd, &closed.id)))
                .map(|dir| dir.join("run.json"))
                .collect(),
        }
    }

    /// Delete a deleted agent's conversation on a thread. Call after the row
    /// left `self.agents` (and `self.closed_agents`), so the remaining runs
    /// are what is kept.
    pub(crate) fn delete_conversation_in_background(&self, job: ConversationCleanup) {
        if job.sessions.is_empty() {
            return;
        }
        let roots = match ConversationRoots::from_env() {
            Ok(roots) => roots,
            Err(error) => {
                let _ = self.bg_notice_tx.send(format!("kept its conversation: {error}"));
                return;
            }
        };
        let tx = self.bg_notice_tx.clone();
        let spawned = thread::Builder::new()
            .name("rudder-conversation-remove".to_string())
            .spawn(move || {
                if let Err(error) = delete_conversation(&roots, &job) {
                    let _ = tx.send(format!("could not delete all of its conversation: {error}"));
                }
            });
        if let Err(error) = spawned {
            let _ = self.bg_notice_tx.send(format!("kept its conversation: {error}"));
        }
    }

    /// The plan this orchestrator drives still has work queued, waiting for
    /// approval or not yet merged.
    fn plan_is_live(&self, plan_id: &str) -> bool {
        self.plans.iter().any(|plan| {
            plan.id == plan_id && (!plan.planned_nodes.is_empty() || plan.awaiting_approval)
        }) || self.agents.iter().any(|run| {
            run.plan_id.as_deref() == Some(plan_id)
                && run.node_id.is_some()
                && run.status != AgentStatus::Merged
        })
    }

    /// Rows the cleanup never deletes, however long nobody touched them.
    pub(crate) fn agent_gc_keeps(&self, run: &AgentRun) -> bool {
        run.pinned
            || is_cloud_owned_agent(run)
            // Working, or waiting on a person.
            || run.status == AgentStatus::Running
            || run.needs_permission
            || run.needs_user_input
            || run.wait_signal.is_some()
            // Mid-integration.
            || run.merge_resolver
            || run.merge_conflict
            || run.merge_requested
            || self.conflict_merge_inflight.as_deref() == Some(run.id.as_str())
            || self.pending_launches.contains_key(&run.id)
            || self.chat_edits_in_flight.contains(&run.id)
            // A plan's unmerged node: deleting it would let its dependents
            // start without its work.
            || (run.node_id.is_some() && run.status != AgentStatus::Merged)
            || (run.is_orchestrator() && run.plan_id.as_deref().is_some_and(|plan| self.plan_is_live(plan)))
            // Unmerged work: finished and waiting for review or a merge, or
            // merged but no longer in main.
            || agent_awaits_merge(run)
            || (run.status == AgentStatus::Merged && run.integration.in_trunk == Some(false))
    }

    pub(crate) fn agent_gc_probes(&self) -> Vec<GcProbe> {
        let signals = signals::signals_dir();
        self.agents
            .iter()
            .filter(|run| !self.agent_gc_keeps(run))
            .map(|run| GcProbe {
                run_id: run.id.clone(),
                record: native_run_dir(&self.cwd, &run.id).join("run.json"),
                backend: run.backend,
                cwd: run.cwd.clone(),
                session: run.session_id.clone(),
                workspace: run.workspace_name.clone().zip(
                    run.workspace_path.clone().filter(|path| path.exists()),
                ),
                change: run.jj_change_id.clone(),
                asks: signals.as_deref().map(|dir| signals::asks_dir_in(dir, &run.id)),
            })
            .collect()
    }

    /// Every AGENT_GC_INTERVAL, delete the chats closed CLOSED_AGENT_TTL ago,
    /// and find the agents nobody touched for UNTOUCHED_AGENT_TTL on a thread,
    /// then close them here.
    pub(crate) fn collect_untouched_agents(&mut self) {
        if let Some(rx) = self.agent_gc_rx.take() {
            match rx.try_recv() {
                Ok(untouched) => self.delete_untouched_agents(untouched),
                Err(TryRecvError::Empty) => {
                    self.agent_gc_rx = Some(rx);
                    return;
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        if self.last_agent_gc.elapsed() < AGENT_GC_INTERVAL {
            return;
        }
        self.last_agent_gc = Instant::now();
        self.purge_expired_closed_agents(SystemTime::now());
        let since = match cleanup_since(&self.cwd, SystemTime::now()) {
            Ok(since) => since,
            Err(error) => {
                self.push_activity(format!("cleanup: {error}"));
                return;
            }
        };
        let probes = self.agent_gc_probes();
        if probes.is_empty() {
            return;
        }
        let roots = match ConversationRoots::from_env() {
            Ok(roots) => roots,
            Err(error) => {
                self.push_activity(format!("cleanup: {error}"));
                return;
            }
        };
        let repo = self.cwd.clone();
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("rudder-agent-gc".to_string())
            .spawn(move || {
                let _ = tx.send(untouched_agents(
                    &repo,
                    &roots,
                    &probes,
                    since,
                    SystemTime::now(),
                    UNTOUCHED_AGENT_TTL,
                ));
            });
        match spawned {
            Ok(_) => self.agent_gc_rx = Some(rx),
            Err(error) => self.push_activity(format!("cleanup: could not start its scan: {error}")),
        }
    }

    /// Close what the scan found, unless it was touched or became busy since.
    pub(crate) fn delete_untouched_agents(&mut self, untouched: Vec<String>) {
        let mut cleaned = Vec::new();
        for id in untouched {
            let Some(index) = self.agents.iter().position(|run| run.id == id) else {
                continue;
            };
            let record = native_run_dir(&self.cwd, &id).join("run.json");
            let touched_since = modified(&record)
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_none_or(|age| age < UNTOUCHED_AGENT_TTL);
            if touched_since || self.agent_gc_keeps(&self.agents[index]) {
                continue;
            }
            let label = agent_label(&self.agents[index]);
            if self.delete_pending.as_deref() == Some(id.as_str()) {
                self.delete_pending = None;
            }
            if let Err(error) = self.close_agent_now(index) {
                self.push_activity(format!("cleanup: could not close \"{label}\": {error}"));
                continue;
            }
            self.record_activity(format!("cleaned up \"{label}\" (untouched for a day)"));
            cleaned.push(label);
        }
        if cleaned.is_empty() {
            return;
        }
        self.notice = Some(if cleaned.len() == 1 {
            format!("closed \"{}\": nobody touched it for a day · {CLOSED_KEPT_COPY}", cleaned[0])
        } else {
            format!("closed {} agents nobody touched for a day · {CLOSED_KEPT_COPY}", cleaned.len())
        });
        self.dirty = true;
        let _ = self.write_rudder_context_timed(None);
    }
}

impl App {
    /// Close a row (module docs). Its record, with the row's latest state,
    /// moves to the closed list first; when that fails the row stays and
    /// nothing changed. A launch still being prepared has nothing to reopen
    /// and is deleted for good. Returns the notice to show.
    pub(crate) fn close_agent_now(&mut self, index: usize) -> Result<String, String> {
        let Some(run) = self.agents.get(index) else {
            return Err("no agent selected".to_string());
        };
        if self.launch_preparing(&run.id) {
            return Ok(self.delete_agent_now(index));
        }
        let closed_at = SystemTime::now();
        save_native_run_record(&self.cwd, run).map_err(|error| format!("could not save its record: {error}"))?;
        move_record_to_closed(&self.cwd, &run.id, closed_at)?;
        let run = self.take_agent_row(index);
        let closed = ClosedAgent::of(&run, closed_at);
        let notice = format!(
            "deleted \"{}\" · {CLOSED_KEPT_COPY}",
            truncate_chars(&closed.title, 40)
        );
        self.closed_agents.insert(0, closed);
        Ok(notice)
    }

    /// Delete one run for good from its record folder `dir` (module docs).
    /// Call once the run is in neither `self.agents` nor `self.closed_agents`.
    pub(crate) fn purge_agent(&self, run: &AgentRun, dir: &Path) {
        // Read before the record goes: it names every session the run left.
        let conversation = self.conversation_of(run, &dir.join("run.json"));
        self.delete_conversation_in_background(conversation);
        if let Err(error) = remove_dir_if_present(dir) {
            let _ = self.bg_notice_tx.send(format!("could not remove its record: {error}"));
        }
        // In the background: removing a workspace on the UI thread froze it.
        if let Some(path) = run.workspace_path.clone() {
            self.remove_workspace_in_background(path, run.workspace_name.clone());
        }
    }

    /// Delete a closed chat for good: `d d` in Show closed, or the cleanup
    /// CLOSED_AGENT_TTL after it closed. Returns its title.
    pub(crate) fn purge_closed_agent(&mut self, run_id: &str) -> Result<String, String> {
        let position = self
            .closed_agents
            .iter()
            .position(|closed| closed.id == run_id)
            .ok_or_else(|| format!("no closed chat {run_id}"))?;
        let dir = closed_run_dir(&self.cwd, run_id);
        let run = read_recorded_agent(&self.cwd, &dir.join("run.json"))?;
        let closed = self.closed_agents.remove(position);
        self.purge_agent(&run, &dir);
        Ok(closed.title)
    }

    pub(crate) fn purge_expired_closed_agents(&mut self, now: SystemTime) {
        let expired: Vec<String> = self
            .closed_agents
            .iter()
            .filter(|closed| now.duration_since(closed.closed_at).is_ok_and(|age| age >= CLOSED_AGENT_TTL))
            .map(|closed| closed.id.clone())
            .collect();
        for id in expired {
            match self.purge_closed_agent(&id) {
                Ok(title) => self.record_activity(format!(
                    "deleted \"{title}\" for good (closed {CLOSED_KEEP_COPY} ago)"
                )),
                Err(error) => self.push_activity(format!("cleanup: {error}")),
            }
        }
    }

    /// Enter in Show closed: put the record back and continue its conversation
    /// with no new turn, through the adopt path. Returns the notice.
    /// Reopen a closed chat on its own record and session, the same resume the
    /// adopt path runs. Returns its row and title.
    pub(crate) fn reopen_closed_agent(&mut self, run_id: &str) -> Result<(usize, String), String> {
        let position = self
            .closed_agents
            .iter()
            .position(|closed| closed.id == run_id)
            .ok_or_else(|| format!("no closed chat {run_id}"))?;
        move_record_back(&self.cwd, run_id)?;
        let closed = self.closed_agents.remove(position);
        let run = read_recorded_agent(&self.cwd, &native_run_dir(&self.cwd, run_id).join("run.json"))?;
        let session = run.session_id.clone().unwrap_or_default();
        let index = self.resume_recorded_agent(run, session, None)?;
        self.record_activity(format!("reopened \"{}\"", closed.title));
        Ok((index, closed.title))
    }

    /// Enter or a click on a row of Show closed: reopen it and show it.
    fn reopen_from_closed_list(&mut self, run_id: &str) {
        self.closed_list = None;
        self.dirty = true;
        match self.reopen_closed_agent(run_id) {
            Ok((index, title)) => {
                self.selected_agent = index;
                self.focus = FocusPane::Worker;
                self.worker_view = WorkerView::Terminal;
                self.notice = Some(format!("reopened \"{}\"", truncate_chars(&title, 40)));
            }
            Err(error) => self.notice = Some(format!("could not reopen it: {error}")),
        }
        let _ = self.write_rudder_context_timed(None);
    }

    /// The app's Reopen on a row of its Show closed group (home.mjs action
    /// `reopen`). The dashboard's own focus stays where it is.
    pub(crate) fn deliver_reopen_request(&mut self, run_id: &str) -> SteerDelivery {
        match self.reopen_closed_agent(run_id) {
            Ok((_, title)) => {
                self.notice = Some(format!("reopened \"{}\" (from the phone)", truncate_chars(&title, 40)));
                self.dirty = true;
                let _ = self.write_rudder_context_timed(None);
                SteerDelivery::Delivered
            }
            Err(error) => SteerDelivery::Failed(format!("could not reopen it: {error}")),
        }
    }

    pub(crate) fn open_closed_list(&mut self) {
        self.closed_list = Some(ClosedList::default());
        self.focus = FocusPane::Task;
        self.dirty = true;
    }

    /// A left click: the Show closed label on the task pane's border shows or
    /// hides the list, and a row of the list reopens that chat. A click
    /// anywhere else hides the list and goes on to the pane under it.
    pub(crate) fn handle_closed_click(&mut self, column: u16, row: u16) -> bool {
        let Some(task_area) = self.task_area else {
            return false;
        };
        if let Some((label, _)) = closed_toggle(self, task_area) {
            if rect_contains(label, column, row) {
                if closed_list_shown(self) {
                    self.closed_list = None;
                    self.dirty = true;
                } else {
                    self.open_closed_list();
                }
                return true;
            }
        }
        let Some(layout) = closed_list_layout(self, task_area) else {
            if self.closed_list.take().is_some() {
                self.dirty = true;
            }
            return false;
        };
        if !rect_contains(layout.area, column, row) {
            self.closed_list = None;
            self.dirty = true;
            return false;
        }
        let picked = (row as usize)
            .checked_sub(layout.area.y as usize + 1)
            .filter(|offset| *offset < layout.rows)
            .and_then(|offset| self.closed_agents.get(layout.first + offset))
            .map(|closed| closed.id.clone());
        if let Some(id) = picked {
            self.reopen_from_closed_list(&id);
        }
        true
    }

    /// A key while Show closed is open: arrows or j/k pick, Enter reopens, `d`
    /// twice deletes for good, Esc hides the list. Any other key hides it and
    /// returns false, so the task pane handles that key as usual.
    pub(crate) fn handle_closed_list_key(&mut self, key: KeyEvent) -> bool {
        let Some(list) = self.closed_list.as_mut() else {
            return false;
        };
        self.dirty = true;
        let armed = list.purge_armed.take();
        if armed.is_some() {
            // The confirm notice goes with the key that answers it.
            self.notice = None;
        }
        let last = self.closed_agents.len().saturating_sub(1);
        list.selected = list.selected.min(last);
        let selected = list.selected;
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Up => list.selected = selected.saturating_sub(1),
            KeyCode::Char('k') if plain => list.selected = selected.saturating_sub(1),
            KeyCode::Down => list.selected = (selected + 1).min(last),
            KeyCode::Char('j') if plain => list.selected = (selected + 1).min(last),
            KeyCode::Esc => self.closed_list = None,
            KeyCode::Enter => {
                let Some(id) = self.closed_agents.get(selected).map(|closed| closed.id.clone()) else {
                    return true;
                };
                self.reopen_from_closed_list(&id);
            }
            KeyCode::Char('d') | KeyCode::Char('D') if plain => self.arm_or_purge_closed(selected, armed),
            _ => {
                self.closed_list = None;
                return false;
            }
        }
        true
    }

    /// `d` in Show closed: the first press asks, a second one after the
    /// question rendered deletes the selected chat for good.
    fn arm_or_purge_closed(&mut self, selected: usize, armed: Option<(String, u64)>) {
        let Some(closed) = self.closed_agents.get(selected) else {
            return;
        };
        let confirmed = armed
            .as_ref()
            .is_some_and(|(id, frame)| *id == closed.id && self.frames_drawn > *frame);
        if !confirmed {
            self.notice = Some(format!(
                "delete \"{}\" forever? its conversation and workspace go too · press d again to confirm · any other key cancels",
                truncate_chars(&closed.title, 40)
            ));
            let armed = Some((closed.id.clone(), self.frames_drawn));
            if let Some(list) = self.closed_list.as_mut() {
                list.purge_armed = armed;
            }
            return;
        }
        let id = closed.id.clone();
        self.notice = Some(match self.purge_closed_agent(&id) {
            Ok(title) => format!("deleted \"{}\" forever", truncate_chars(&title, 40)),
            Err(error) => format!("could not delete it: {error}"),
        });
    }
}
