//! Branch an agent: a new agent that starts where another one is, with a copy
//! of its conversation and of its files, while the original keeps running
//! untouched. `b` in the dashboard and the app's Branch (steer kind `branch`,
//! instruction `branch`, then the branch's first prompt on the next line when
//! there is one) both end in `branch_agent`.
//!
//! - Files: one jj command in the source's working copy snapshots it and
//!   duplicates its working-copy commit (`jj duplicate @`). The copy has the
//!   same parents and files, uncommitted edits included, but is a change of
//!   its own: the source's later edits rewrite the source's commit, never the
//!   copy or anything built on it. The branch's workspace starts on the copy
//!   (`__launch-node --base`).
//! - Claude: every complete line of the session file is copied into the new
//!   workspace's project folder under a new id, and Claude resumes the copy
//!   (`--resume <copy>`), as an edited chat does (chat_edit.rs). The copy is
//!   the branch's own session from the start, so its chat shows the history
//!   before its first message. Deleting the branch deletes it, and the
//!   source's own file is never touched.
//! - Codex: a one-shot `codex app-server` forks the whole thread
//!   (`thread/fork`) with the new workspace as its cwd, and Codex starts with
//!   `resume <new>`. The fork reads through the source's rollout
//!   (`history_base`), so the cleanup keeps a rollout another agent builds on
//!   (agent_life.rs), and keeps the source's conversation while a branch of
//!   it is still being set up.
//!
//! The row goes in next to its source at once. Its workspace and conversation
//! are made off the App loop, and `finish_branch_launch` starts it and answers
//! the app. With no prompt the branch waits, idle, for one.
use super::*;
use crate::agent_life::{ConversationRoots, AGENT_TITLE_MAX_CHARS};

pub(crate) const NOTHING_TO_BRANCH: &str = "Nothing to branch yet";
const BRANCH_SUFFIX: &str = " (branch)";
const INSTRUCTION_SHAPE: &str = "expected branch, then an optional first message";

/// What a branch copies, read on the App loop.
pub(crate) struct BranchSource {
    pub(crate) backend: Backend,
    /// Where the source runs: its workspace, or the checkout for a main agent.
    pub(crate) cwd: PathBuf,
    pub(crate) session: String,
    /// `~/.claude/projects`.
    pub(crate) claude_projects: PathBuf,
}

/// A branch whose workspace is being prepared.
pub(crate) struct PendingBranch {
    /// The source's session: the cleanup keeps it while the branch is set up.
    pub(crate) source_session: String,
    /// The source's name, for notices.
    pub(crate) source_label: String,
    pub(crate) prompt: Option<String>,
    /// The app's request, answered when the branch starts or fails.
    pub(crate) request_id: Option<String>,
}

/// A forked conversation, ready to start.
#[derive(Debug)]
pub(crate) enum Branched {
    /// Start with `--resume <session>`: the copy.
    Claude { session: String },
    /// Start with `resume <session>`: the forked thread.
    Codex { session: String },
}

impl Branched {
    fn session(&self) -> &str {
        match self {
            Branched::Claude { session } | Branched::Codex { session } => session,
        }
    }
}

// ---------------------------------------------------------------------------
// Off the App loop: the copy of the files and the fork of the conversation.
// ---------------------------------------------------------------------------

/// Run jj in `dir`, catching a stale working copy up once: another
/// workspace's operation routinely leaves an agent's working copy behind
/// (as gitio.rs run_jj_with_stale_recovery does). Returns jj's status output
/// on success or its error message on failure.
fn jj_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let run = || {
        Command::new(jj_program())
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not run jj: {error}"))
    };
    let mut output = run()?;
    if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("stale") {
        Command::new(jj_program())
            .args(["workspace", "update-stale"])
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("could not run jj: {error}"))?;
        output = run()?;
    }
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Snapshot `source_cwd`'s working copy and duplicate its working-copy
/// commit for the branch `run_id`. Returns the copy's commit id.
pub(crate) fn copy_code(source_cwd: &Path, run_id: &str) -> Result<String, String> {
    // Run ids are lowercase words and digits joined by `-`. Have duplicate
    // print the exact commit it created instead of searching history for it.
    let description = format!("rudder-branch {run_id}: the files it branched from");
    let description_config = format!(r#"templates.duplicate_description="\"{description}\"""#);
    let tag = "rudder-branch-commit:";
    let summary_config = format!(
        r#"templates.commit_summary="if(description.first_line() == \"{description}\", \"{tag}\" ++ commit_id, \"\")""#
    );
    let copied = jj_in(
        source_cwd,
        &[
            "--color=never",
            "--config",
            "ui.quiet=false",
            "--config",
            &description_config,
            "--config",
            &summary_config,
            "duplicate",
            "@",
        ],
    )
    .map_err(|error| format!("Could not copy its files: {error}"))?;
    let mut commits = copied.lines().filter_map(|line| line.split_once(tag).map(|(_, commit)| commit));
    match (commits.next(), commits.next()) {
        (Some(commit), None) if !commit.is_empty() && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(commit.to_string())
        }
        _ => Err("Could not copy its files: jj did not report one snapshot commit".to_string()),
    }
}

/// Copy the source's Claude session file into `target_cwd`'s project folder
/// under a new id. Only complete lines: Claude may be mid-write.
pub(crate) fn fork_claude(
    projects: &Path,
    source_cwd: &Path,
    session: &str,
    target_cwd: &Path,
) -> Result<Branched, String> {
    let source = projects
        .join(encode_claude_project_dir(source_cwd))
        .join(format!("{session}.jsonl"));
    let bytes = match fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(NOTHING_TO_BRANCH.to_string()),
        Err(error) => return Err(format!("could not read {}: {error}", source.display())),
    };
    let complete = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    if complete == 0 {
        return Err(NOTHING_TO_BRANCH.to_string());
    }
    let folder = projects.join(encode_claude_project_dir(target_cwd));
    fs::create_dir_all(&folder).map_err(|error| format!("could not create {}: {error}", folder.display()))?;
    let session = crate::gitio::new_rudder_session_id();
    write_private_new(&folder.join(format!("{session}.jsonl")), &bytes[..complete])?;
    Ok(Branched::Claude { session })
}

/// Fork the source's whole conversation for the branch working in `target_cwd`.
pub(crate) fn fork_conversation(source: &BranchSource, target_cwd: &Path) -> Result<Branched, String> {
    match source.backend {
        Backend::Claude => fork_claude(&source.claude_projects, &source.cwd, &source.session, target_cwd),
        Backend::Codex => {
            let session = codex_fork(
                target_cwd,
                serde_json::json!({
                    "threadId": source.session,
                    "cwd": target_cwd,
                    "excludeTurns": true,
                    // An idle branch must not pick the source's goal back up
                    // on its own; its next prompt does.
                    "deferGoalContinuation": true,
                }),
            )?;
            Ok(Branched::Codex { session })
        }
        Backend::Opencode | Backend::Antigravity => Err(branch_unsupported(source.backend)),
    }
}

// ---------------------------------------------------------------------------
// The App side.
// ---------------------------------------------------------------------------

/// Antigravity forks a conversation only from inside it (`/fork` in its
/// pane); there is no command to copy one into a new agent from outside.
fn branch_unsupported(backend: Backend) -> String {
    match backend {
        Backend::Antigravity => {
            "Antigravity can't copy a chat into a new agent; type /fork in the agent's pane".to_string()
        }
        _ => "Branching works for Claude and Codex agents".to_string(),
    }
}

fn branch_refusal(run: &AgentRun) -> Option<String> {
    if run.is_orchestrator() {
        return Some("A plan's orchestrator can't be branched".to_string());
    }
    if run.plain_process {
        return Some("Only agents can be branched".to_string());
    }
    if run.gam.is_some() || run.mode == AgentMode::GamAdversarial {
        return Some("Agents in a /gam pair can't be branched".to_string());
    }
    if is_cloud_owned_agent(run) {
        return Some("This agent runs on another machine".to_string());
    }
    if matches!(run.backend, Backend::Opencode | Backend::Antigravity) {
        return Some(branch_unsupported(run.backend));
    }
    if let Some(profile) = run.execution_profile.as_deref().filter(|profile| *profile != "native") {
        return Some(format!("This agent runs on the {profile} profile, which can't be branched"));
    }
    None
}

/// The source's name, and the branch's: "<name> (branch)".
fn branch_names(source: &AgentRun) -> (String, String) {
    let name = if source.task_summary.trim().is_empty() {
        summarize_task(&source.task)
    } else {
        source.task_summary.trim().to_string()
    };
    let room = AGENT_TITLE_MAX_CHARS - BRANCH_SUFFIX.chars().count();
    (truncate_chars(&name, 60), format!("{}{BRANCH_SUFFIX}", truncate_chars(&name, room)))
}

/// `branch`, then optionally the first prompt on the lines after it.
fn branch_prompt(instruction: &str) -> Result<Option<String>, String> {
    let rest = instruction.strip_prefix("branch").ok_or_else(|| INSTRUCTION_SHAPE.to_string())?;
    if rest.is_empty() {
        return Ok(None);
    }
    let text = rest.strip_prefix('\n').ok_or_else(|| INSTRUCTION_SHAPE.to_string())?;
    let text = sanitize_steer_instruction(text);
    Ok((!text.trim().is_empty()).then_some(text))
}

impl App {
    /// Branch the agent at `index`: its row goes in right after the source's
    /// and is started when its workspace is ready. Returns the branch's run id.
    pub(crate) fn branch_agent(
        &mut self,
        index: usize,
        prompt: Option<String>,
        request_id: Option<String>,
    ) -> Result<String, String> {
        let run = self.agents.get(index).ok_or_else(|| "no agent selected".to_string())?;
        if let Some(refusal) = branch_refusal(run) {
            return Err(refusal);
        }
        if self.pending_launches.contains_key(&run.id) {
            return Err("It is still starting; branch it in a moment".to_string());
        }
        if self.chat_edits_in_flight.contains(&run.id) {
            return Err("Its chat is going back to an earlier message; branch it when that finishes".to_string());
        }
        let session = run
            .session_id
            .clone()
            .filter(|id| valid_session_id(id))
            .ok_or_else(|| NOTHING_TO_BRANCH.to_string())?;
        if !run.cwd.is_dir() {
            return Err(format!("Its workspace is gone: {}", run.cwd.display()));
        }
        let roots = ConversationRoots::from_env()?;
        let claude_file = roots
            .claude_projects
            .join(encode_claude_project_dir(&run.cwd))
            .join(format!("{session}.jsonl"));
        if run.backend == Backend::Claude && !claude_file.is_file() {
            return Err(NOTHING_TO_BRANCH.to_string());
        }
        let (source_label, name) = branch_names(run);
        let run_id = new_run_id(&name);
        let mut row = fork_row(
            run_id.clone(),
            run.task.clone(),
            name.clone(),
            (run.backend, run.model.clone(), run.effort),
            // The branch continues the source's conversation, on its account.
            run.account.clone(),
            self.cwd.clone(),
        );
        if let Some(prompt) = &prompt {
            row.current_prompt = prompt.clone();
        }
        let job = WorkspacePrepJob {
            cwd: self.cwd.clone(),
            label: name,
            base_change: None,
            stage_claude_session: None,
            branch: Some(BranchSource {
                backend: run.backend,
                cwd: run.cwd.clone(),
                session: session.clone(),
                claude_projects: roots.claude_projects,
            }),
            files_of: None,
        };
        let at = index + 1;
        self.agents.insert(at, row);
        // Indices moved under the rendered rows; the next render rebuilds them.
        self.agent_row_map.clear();
        if self.selected_agent >= at {
            self.selected_agent += 1;
        }
        self.dirty = true;
        self.push_activity(format!("{source_label}: branching"));
        let pending = PendingBranch { source_session: session, source_label, prompt, request_id };
        self.begin_workspace_prep(run_id.clone(), PendingLaunchKind::Branch(pending), job);
        Ok(run_id)
    }

    /// Steer kind "branch". `None`: accepted; the receipt is written when the
    /// branch starts or fails (`finish_branch_launch`).
    pub(crate) fn deliver_branch_request(
        &mut self,
        task_id: &str,
        instruction: &str,
        request_id: &str,
    ) -> Option<SteerDelivery> {
        let started = branch_prompt(instruction).and_then(|prompt| {
            let index = self
                .agent_index_for_token(task_id)
                .ok_or_else(|| format!("target not found: {task_id}"))?;
            self.branch_agent(index, prompt, Some(request_id.to_string()))
        });
        match started {
            Ok(_) => None,
            Err(reason) => {
                self.push_activity(format!("branch: {reason}"));
                Some(SteerDelivery::Failed(FailCode::Refused, reason))
            }
        }
    }

    /// The branch's workspace and conversation are ready (or failed): start
    /// it, or take its row back out, and answer the app either way.
    pub(crate) fn finish_branch_launch(
        &mut self,
        index: usize,
        outcome: WorkspacePrepOutcome,
        branch: PendingBranch,
        typed: &[u8],
    ) {
        let request_id = branch.request_id.clone();
        let source_label = branch.source_label.clone();
        let result = self.start_branch(index, outcome, branch, typed);
        if let Err(reason) = &result {
            self.notice = Some(format!("could not branch \"{source_label}\": {reason}"));
            self.push_activity(format!("branch: {reason}"));
        }
        if let Some(request_id) = request_id {
            let _ = self.write_steer_receipt(&request_id, result.as_ref().map(|_| ()).map_err(|error| (FailCode::Refused, error.as_str())));
        }
        let _ = self.write_rudder_context_timed(None);
        self.dirty = true;
    }

    fn start_branch(
        &mut self,
        index: usize,
        outcome: WorkspacePrepOutcome,
        branch: PendingBranch,
        typed: &[u8],
    ) -> Result<(), String> {
        let (workspace, branched) = match (outcome.workspace, outcome.branched, outcome.error) {
            (Some(workspace), Some(branched), None) => (workspace, branched),
            (workspace, _, error) => {
                // Nothing started: the row and the workspace made for it go.
                let was_selected = self.selected_agent == index;
                self.delete_agent_now(index);
                if was_selected && self.focus == FocusPane::Worker {
                    self.focus = FocusPane::Agents;
                }
                if let Some(workspace) = workspace.filter(|workspace| workspace.path_is_workspace) {
                    self.remove_workspace_in_background(workspace.path, workspace.workspace_name);
                }
                return Err(error.unwrap_or_else(|| "its workspace was not made".to_string()));
            }
        };
        let run = &mut self.agents[index];
        run.cwd = workspace.path.clone();
        run.workspace_path = workspace.path_is_workspace.then_some(workspace.path.clone());
        run.workspace_name = workspace.workspace_name.clone();
        run.jj_change_id = workspace.jj_change_id.clone();
        run.session_id = Some(branched.session().to_string());
        run.last_output_at = Instant::now();
        let backend = run.backend;
        let run_id = run.id.clone();
        let account = run.account.clone();
        let mut command = match &branched {
            Branched::Claude { session } => claude_resume_command(run, session),
            Branched::Codex { session } => codex_resume_command(run, session),
        };
        // A resumed conversation runs in a terminal here, as an edited chat's does.
        let wired =
            signals::augment_existing_worker_command(&mut command, backend, AgentMode::Execute, &run_id, Some("native"), account.as_deref());
        if let Some(prompt) = &branch.prompt {
            // After `--`, text that starts with "-" is the prompt, not a flag.
            command.args.push("--".to_string());
            command.args.push(prompt.clone());
        }
        // It reads RUDDER.md first thing; it should find itself there.
        if let Err(error) = self.write_rudder_context_timed(None) {
            self.notice = Some(format!("context warning: {error}"));
        }
        let options = TerminalPaneOptions {
            size: TerminalSize::default(),
            cwd: Some(workspace.path.clone()),
            ..TerminalPaneOptions::default()
        };
        let spawned = self.spawn_into_row(index, wired.map(|()| command), options, typed);
        let run = &mut self.agents[index];
        if spawned.is_ok() {
            match &branch.prompt {
                Some(prompt) => {
                    let now = now_stamp();
                    run.current_prompt = prompt.clone();
                    run.turns = vec![AgentTurn { ts: now.clone(), prompt: prompt.clone(), source: "user".to_string() }];
                    run.last_user_input_at = now;
                    run.last_worker_input_at = Some(Instant::now());
                }
                // Resuming runs no turn: it sits at its prompt, like a
                // finished agent reopened.
                None => {
                    run.status = AgentStatus::Done;
                    run.completed_at = Some(Instant::now());
                }
            }
        }
        save_native_run_record(&self.cwd, run)
            .map_err(|error| format!("its record could not be saved: {error}"))?;
        if let Err(error) = spawned {
            self.release_focus_after_failed_start(index);
            return Err(format!("could not start {}: {error}", backend.as_str()));
        }
        let notice = match &branch.prompt {
            Some(prompt) => format!("branched \"{}\" and sent \"{}\"", branch.source_label, steer_preview(prompt)),
            None => format!("branched \"{}\"; it waits for your message", branch.source_label),
        };
        self.record_activity(notice.clone());
        self.notice = Some(notice);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rudder-branch-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn branch_instruction_carries_an_optional_first_prompt() {
        assert_eq!(branch_prompt("branch").unwrap(), None);
        assert_eq!(branch_prompt("branch\n  ").unwrap(), None);
        assert_eq!(branch_prompt("branch\ntry it with sqlite\n- keep the API").unwrap().as_deref(), Some("try it with sqlite\n- keep the API"));
        assert!(branch_prompt("branched").is_err());
        assert!(branch_prompt("try this").is_err());
    }

    #[test]
    fn branch_claude_copy_keeps_whole_lines_and_leaves_the_source_alone() {
        let root = temp_dir("claude");
        let projects = root.join("projects");
        let source_cwd = root.join("source");
        let target_cwd = root.join("branch");
        let session = "11111111-1111-4111-8111-111111111111";
        let source_dir = projects.join(encode_claude_project_dir(&source_cwd));
        fs::create_dir_all(&source_dir).unwrap();
        let source_file = source_dir.join(format!("{session}.jsonl"));
        let written = b"{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"assistant\",\"uuid\":\"a1\"}\n{\"type\":\"assis";
        fs::write(&source_file, written).unwrap();

        let Branched::Claude { session: forked } = fork_claude(&projects, &source_cwd, session, &target_cwd).unwrap()
        else {
            panic!("a Claude source forks as Claude");
        };
        assert!(valid_session_id(&forked));
        assert_ne!(forked, session);
        // The copy is the branch's session, where Claude looks for it from
        // the branch's workspace, without the line Claude was still writing.
        let copy = projects.join(encode_claude_project_dir(&target_cwd)).join(format!("{forked}.jsonl"));
        assert_eq!(
            fs::read(&copy).unwrap(),
            b"{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"assistant\",\"uuid\":\"a1\"}\n"
        );
        assert_eq!(fs::read(&source_file).unwrap(), written);

        // No conversation on disk yet: nothing to branch.
        assert_eq!(
            fork_claude(&projects, &source_cwd, "22222222-2222-4222-8222-222222222222", &target_cwd).unwrap_err(),
            NOTHING_TO_BRANCH
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn branch_code_copy_is_a_snapshot_the_source_cannot_move() {
        let root = temp_dir("jj");
        let jj = |dir: &Path, args: &[&str]| {
            let output = Command::new(jj_program())
                .args(args)
                .current_dir(dir)
                .env("JJ_CONFIG", root.join("no-config.toml"))
                .env("JJ_USER", "Rudder Test")
                .env("JJ_EMAIL", "test@rudder.invalid")
                .stdin(Stdio::null())
                .output()
                .expect("run jj");
            assert!(output.status.success(), "jj {args:?}: {}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        jj(&repo, &["git", "init"]);
        // Capture the commit even when the project normally hides jj status output.
        jj(&repo, &["config", "set", "--repo", "ui.quiet", "true"]);
        fs::write(repo.join("app.txt"), "base\n").unwrap();
        jj(&repo, &["commit", "-m", "base"]);
        let source = root.join("source");
        jj(&repo, &["workspace", "add", source.to_str().unwrap(), "--name", "source", "-r", "@-"]);
        // Edits jj has not seen yet come along.
        fs::write(source.join("app.txt"), "source edit\n").unwrap();
        fs::write(source.join("new.txt"), "new file\n").unwrap();

        let commit = copy_code(&source, "try-sqlite-ab12c").unwrap();
        let show = |rev: &str, file: &str| jj(&repo, &["--ignore-working-copy", "file", "show", "-r", rev, file]);
        assert_eq!(show(&commit, "app.txt"), "source edit");
        assert_eq!(show(&commit, "new.txt"), "new file");
        // A sibling of the source's working copy, not built on it.
        let parent = |rev: &str| {
            jj(&repo, &["--ignore-working-copy", "log", "--no-graph", "-r", &format!("{rev}-"), "-T", "commit_id"])
        };
        assert_eq!(parent(&commit), parent("source@"));

        // The source keeps working; the copy does not move with it.
        fs::write(source.join("app.txt"), "later edit\n").unwrap();
        jj(&source, &["status"]);
        assert_eq!(show(&commit, "app.txt"), "source edit");
        // The command identifies its own copy even when history holds the same description.
        let next_commit = copy_code(&source, "try-sqlite-ab12c").unwrap();
        assert_ne!(next_commit, commit);
        assert_eq!(show(&next_commit, "app.txt"), "later edit");
        assert_eq!(
            jj(&repo, &["--ignore-working-copy", "log", "--no-graph", "-r", &commit, "-T", "description"]),
            "rudder-branch try-sqlite-ab12c: the files it branched from"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
