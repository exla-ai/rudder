//! Renaming, pinning and deleting agents, and the cleanup that deletes agents
//! nobody touched for a day.
//!
//! Deleting has one path, `delete_agent_now`: the row, its record, signals and
//! workspace go (the workspace's jj change stays in `jj log`, see
//! `remove_workspace_in_background`), and so does its conversation: every
//! Claude session file the run recorded with the folder Claude keeps next to
//! it, or its Codex rollouts. A rollout another remaining run builds on
//! (`history_base`, written when Codex forks a thread) is kept. Files are
//! removed on a thread; a failure comes back as a notice.
//!
//! The cleanup runs here, in the dashboard, because the dashboard is the one
//! scheduling authority of its project, on a laptop and on the home machine.
//! Its clock starts at its first scan in a project (`.rudder/cleanup-since`),
//! so agents that existed before it get a full day from then.
use super::*;

/// An agent nobody touched for this long is deleted, conversation included.
pub(crate) const UNTOUCHED_AGENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How often the dashboard looks for untouched agents.
pub(crate) const AGENT_GC_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Longest agent name. The app's state carries 200 characters of a title.
pub(crate) const AGENT_TITLE_MAX_CHARS: usize = 200;

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
    for record in &job.kept_records {
        if let Some(value) = fs::read_to_string(record)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            in_use.extend(recorded_session_ids(&value));
        }
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
    /// Its jj workspace, while the directory exists.
    pub(crate) workspace: Option<String>,
    /// Where the phone's questions for it wait (`<id>.json`).
    pub(crate) asks: Option<PathBuf>,
}

fn modified(path: &Path) -> Result<SystemTime, String> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// When jj last saw the workspace's files change: its working-copy commit's
/// committer time. Read-only; it does not snapshot anything.
fn workspace_changed_at(repo_root: &Path, workspace: &str) -> Result<SystemTime, String> {
    let revset = format!("\"{}\"@", workspace.replace('\\', "\\\\").replace('"', "\\\""));
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

/// A question, permission or plan from the agent still waits for an answer.
fn has_pending_ask(asks: &Path) -> bool {
    fs::read_dir(asks).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".json") && !name.ends_with(".answer.json")
        })
    })
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
    if let Some(workspace) = probe.workspace.as_deref() {
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
        run.task_summary = title.clone();
        save_native_run_record(&self.cwd, run)
            .map_err(|error| format!("could not save the name: {error}"))?;
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

    /// Every session this run used, read before its record is removed.
    pub(crate) fn conversation_of(&self, run: &AgentRun) -> ConversationCleanup {
        let record = native_run_dir(&self.cwd, &run.id).join("run.json");
        let mut sessions: Vec<String> = fs::read_to_string(&record)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .map(|value| recorded_session_ids(&value))
            .unwrap_or_default();
        sessions.extend(run.session_id.clone());
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
            kept_records: self
                .agents
                .iter()
                .map(|other| native_run_dir(&self.cwd, &other.id).join("run.json"))
                .collect(),
        }
    }

    /// Delete a deleted agent's conversation on a thread. Call after the row
    /// left `self.agents`, so the remaining runs are what is kept.
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
        thread::Builder::new()
            .name("rudder-conversation-remove".to_string())
            .spawn(move || {
                if let Err(error) = delete_conversation(&roots, &job) {
                    let _ = tx.send(format!("could not delete all of its conversation: {error}"));
                }
            })
            .ok();
    }

    /// The plan this orchestrator drives still has work queued, waiting for
    /// approval or not yet merged.
    fn plan_is_live(&self, plan_id: Option<&str>) -> bool {
        let Some(plan_id) = plan_id else {
            // A planner from before plans had ids: it can't be told apart.
            return true;
        };
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
            || (run.is_orchestrator() && self.plan_is_live(run.plan_id.as_deref()))
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
                workspace: run
                    .workspace_name
                    .clone()
                    .filter(|_| run.workspace_path.as_deref().is_some_and(Path::exists)),
                asks: signals.as_deref().map(|dir| signals::asks_dir_in(dir, &run.id)),
            })
            .collect()
    }

    /// Every AGENT_GC_INTERVAL, find the agents nobody touched for
    /// UNTOUCHED_AGENT_TTL on a thread, then delete them here.
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
        self.agent_gc_rx = Some(rx);
        thread::Builder::new()
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
            })
            .ok();
    }

    /// Delete what the scan found, unless it was touched or became busy since.
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
            self.delete_agent_now(index);
            self.record_activity(format!("cleaned up \"{label}\" (untouched for a day)"));
            cleaned.push(label);
        }
        if cleaned.is_empty() {
            return;
        }
        self.notice = Some(if cleaned.len() == 1 {
            format!("cleaned up \"{}\": nobody touched it for a day", cleaned[0])
        } else {
            format!("cleaned up {} agents nobody touched for a day", cleaned.len())
        });
        self.dirty = true;
        let _ = self.write_rudder_context_timed(None);
    }
}
