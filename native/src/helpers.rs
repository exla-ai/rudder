//! Helper agents: one agent asks for another on a model it names, for a review
//! or a second opinion (the `agents_start` tool, agent_mcp.rs).
//!
//! - The tool queues steer kind "helper" for this dashboard: the first line is
//!   `<harness> <model> [effort]`, the task follows. `deliver_helper_request`
//!   checks them against the model catalog and starts an isolated worker on
//!   exactly that model and effort. The user's model for new agents (the
//!   dashboard's selection and ~/.rudder/config.json) is not touched. The
//!   receipt carries the new run id.
//! - The row records the agent that asked (`parent_run`, run.json
//!   `parentRunId`); the agents list and the app show it as that agent's helper.
//! - The helper's workspace starts from a copy of the asking agent's files,
//!   uncommitted edits included (`branch::copy_code`, as a branch's does): an
//!   overlay fork's files exist only inside its own agent's mount namespace,
//!   so another agent cannot read them at their path. The helper reads the
//!   copy itself, with its own tools. Nothing is pasted into its prompt, so no
//!   message limit cuts what it sees (`agy -p` keeps about 200 KB of one
//!   message), and its edits never reach the asking agent's files.
//! - When a helper's turn ends (detect.rs `finish_turn`), its final message is
//!   read off the App loop from its backend's own session record and appended
//!   to `.rudder/helpers/<run id>.json`, where `agents_result` reads it. This
//!   works for every harness, including Antigravity, which has no peer tools.

use super::*;
use std::sync::Mutex;

const SHAPE: &str = "expected <harness> <model> [effort], then the task on the next lines";
/// Live helpers one agent may have at a time.
const MAX_LIVE_HELPERS: usize = 4;
/// How long a finished turn's final message may take to reach its session record.
const REPLY_WAIT: Duration = Duration::from_secs(30);

/// `.rudder/helpers/<run id>.json`: who asked, on what, and every reply so far.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HelperRecord {
    pub(crate) run_id: String,
    pub(crate) parent_run_id: String,
    pub(crate) harness: String,
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) replies: Vec<HelperReply>,
}

/// One finished turn: its final message in full, or why it could not be read.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HelperReply {
    pub(crate) at: String,
    pub(crate) text: Option<String>,
    pub(crate) error: Option<String>,
}

/// A run id that is safe as a file name.
pub(crate) fn valid_helper_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub(crate) fn helper_record_path(repo: &Path, run_id: &str) -> PathBuf {
    repo.join(".rudder").join("helpers").join(format!("{run_id}.json"))
}

pub(crate) fn read_helper_record(repo: &Path, run_id: &str) -> Result<HelperRecord, String> {
    if !valid_helper_id(run_id) {
        return Err(format!("{run_id:?} is not a run id"));
    }
    let path = helper_record_path(repo, run_id);
    let raw = fs::read_to_string(&path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => format!("no helper {run_id} in this project"),
        _ => format!("could not read {}: {error}", path.display()),
    })?;
    serde_json::from_str(&raw).map_err(|error| format!("{} is not a helper record: {error}", path.display()))
}

fn write_helper_record(repo: &Path, record: &HelperRecord) -> Result<(), String> {
    let path = helper_record_path(repo, &record.run_id);
    let dir = path.parent().expect("a helper record has a directory");
    fs::create_dir_all(dir).map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    fs::write(&temp, bytes)
        .and_then(|()| fs::rename(&temp, &path))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Serializes read-modify-write of helper records within this process.
static RECORD_LOCK: Mutex<()> = Mutex::new(());

/// Append one finished turn. The same message twice in a row is one turn
/// reported twice, so it is kept once.
pub(crate) fn append_helper_reply(repo: &Path, run_id: &str, reply: HelperReply) -> Result<(), String> {
    let _guard = RECORD_LOCK.lock().map_err(|_| "helper record lock failed".to_string())?;
    let mut record = read_helper_record(repo, run_id)?;
    let repeated = record
        .replies
        .last()
        .is_some_and(|last| last.text == reply.text && last.error == reply.error);
    if repeated {
        return Ok(());
    }
    record.replies.push(reply);
    write_helper_record(repo, &record)
}

#[derive(Debug, PartialEq)]
pub(crate) struct HelperSpec {
    pub(crate) backend: Backend,
    pub(crate) model: String,
    pub(crate) effort: Option<EffortLevel>,
    pub(crate) task: String,
}

/// Steer kind "helper"'s instruction. The model must be one the catalog lists
/// for that harness, and the effort one that model takes; no effort means the
/// model's default, as `/model` without one.
pub(crate) fn parse_helper_instruction(instruction: &str) -> Result<HelperSpec, String> {
    let (head, task) = instruction.split_once('\n').ok_or(SHAPE)?;
    let parts: Vec<&str> = head.split_whitespace().collect();
    let (harness, model, effort) = match parts[..] {
        [harness, model] => (harness, model, None),
        [harness, model, effort] => (harness, model, Some(effort)),
        _ => return Err(SHAPE.to_string()),
    };
    let backend = Backend::parse(harness)
        .ok_or_else(|| format!("unknown harness: {harness}; use claude, codex, antigravity or opencode"))?;
    let mut args = vec![backend.as_str(), model];
    args.extend(effort);
    let (backend, model, effort) = parse_model_args(&args).map_err(|refusal| {
        // The refusal names `/model`, which an agent cannot open: list the
        // models it may name instead.
        let refusal = refusal.split("; pick one").next().unwrap_or(&refusal).to_string();
        format!("{refusal}. {}", known_models_note(backend))
    })?;
    if let Some(refusal) = openrouter_key_refusal(backend, &model) {
        return Err(refusal.to_string());
    }
    let task = sanitize_steer_instruction(task);
    if task.trim().is_empty() {
        return Err("the helper's task is empty".to_string());
    }
    Ok(HelperSpec { backend, model, effort, task })
}

fn known_models_note(backend: Backend) -> String {
    let models: Vec<String> = model_suggestions_for(backend, "")
        .into_iter()
        .filter_map(|suggestion| match suggestion.action {
            SuggestionAction::ChooseModel { model, .. } => Some(model),
            _ => None,
        })
        .collect();
    let effort = if backend == Backend::Antigravity {
        "; their ids carry the effort, so give no effort"
    } else {
        ""
    };
    if models.is_empty() {
        format!("Rudder lists no {} models on this machine", backend.as_str())
    } else {
        format!("Known {} models{effort}: {}", backend.as_str(), models.join(", "))
    }
}

/// The helper's task as it starts: the asker's words, then what a helper is.
pub(crate) fn helper_prompt(task: &str, parent: &str) -> String {
    let objective = task
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_chars(line, 200))
        .unwrap_or_default();
    format!(
        "Objective: {objective}\nDone when: your final message answers the request in full.\n\n{task}\n\n\
         Rudder: you are a helper agent. The agent \"{parent}\" started you and reads only your final message of each turn, so put your whole answer in it. \
         Your working directory starts as a copy of that agent's files, its uncommitted edits included. Read them there with your own tools; \
         relative paths in this request are taken from there. Your changes stay in your copy and never reach that agent's files."
    )
}

impl App {
    /// Steer kind "helper", from `parent_id`'s `agents_start`. `None`: started,
    /// and the receipt (with the new run id) is written.
    pub(crate) fn deliver_helper_request(
        &mut self,
        parent_id: &str,
        instruction: &str,
        request_id: &str,
    ) -> Option<SteerDelivery> {
        let started = self.start_helper(parent_id, instruction);
        match started {
            Ok(run_id) => match self.write_helper_receipt(request_id, &run_id) {
                Ok(()) => None,
                Err(error) => Some(SteerDelivery::Failed(format!(
                    "helper {run_id} started, but its receipt was not written: {error}"
                ))),
            },
            Err(reason) => {
                self.push_activity(format!("helper not started: {reason}"));
                Some(SteerDelivery::Failed(reason))
            }
        }
    }

    fn start_helper(&mut self, parent_id: &str, instruction: &str) -> Result<String, String> {
        let spec = parse_helper_instruction(instruction)?;
        let parent_index = self
            .agent_index_for_token(parent_id)
            .ok_or_else(|| format!("the agent that asked ({parent_id}) is not in this dashboard"))?;
        let parent = &self.agents[parent_index];
        if parent.parent_run.is_some() {
            return Err("a helper cannot start helpers of its own".to_string());
        }
        let parent_id = parent.id.clone();
        let live = self
            .agents
            .iter()
            .filter(|run| run.parent_run.as_deref() == Some(parent_id.as_str()))
            .filter(|run| run.status == AgentStatus::Running)
            .count();
        if live >= MAX_LIVE_HELPERS {
            return Err(format!(
                "this agent already has {live} helpers working; wait for one with agents_result first"
            ));
        }
        let parent = &self.agents[parent_index];
        let parent_label = if parent.task_summary.trim().is_empty() {
            summarize_task(&parent.task)
        } else {
            parent.task_summary.clone()
        };
        let parent_cwd = parent.cwd.clone();
        let prompt = helper_prompt(&spec.task, &parent_label);
        let title = summarize_task(&spec.task);
        // Start on exactly this model the way a routed task does
        // (finish_task_routing): the selection for new agents comes back
        // after, and the user's view stays where it was.
        let previous = (self.backend, self.model.clone(), self.effort, self.selected_agent, self.focus);
        self.backend = spec.backend;
        self.model = spec.model.clone();
        self.effort = spec.effort;
        let before = self.agents.len();
        self.start_execute_task_node(&prompt, Some(&title), None, Some(parent_cwd));
        (self.backend, self.model, self.effort, self.selected_agent, self.focus) = previous;
        if self.agents.len() != before + 1 {
            return Err("the helper's row was not created".to_string());
        }
        let index = self.agents.len() - 1;
        let run = &mut self.agents[index];
        run.parent_run = Some(parent_id.clone());
        let run_id = run.id.clone();
        let record = HelperRecord {
            run_id: run_id.clone(),
            parent_run_id: parent_id,
            harness: spec.backend.as_str().to_string(),
            model: spec.model.clone(),
            effort: spec.effort.map(|effort| effort.as_str().to_string()),
            replies: Vec::new(),
        };
        if let Err(error) = write_helper_record(&self.cwd, &record) {
            self.delete_agent_now(index);
            return Err(error);
        }
        // A row still making its workspace has no record yet; finish_task_launch
        // writes it with the link.
        if !self.launch_preparing(&run_id) {
            let _ = save_native_run_record(&self.cwd, &self.agents[index]);
        }
        let label = agent_model_label(spec.backend, &spec.model, spec.effort);
        self.push_activity(format!("{parent_label}: started helper {run_id} on {label}"));
        self.dirty = true;
        Ok(run_id)
    }

    fn write_helper_receipt(&self, request_id: &str, run_id: &str) -> io::Result<()> {
        let valid = request_id.len() >= 8
            && request_id.len() <= 128
            && request_id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-');
        if !valid {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a helper request needs a request id"));
        }
        let dir = self.cwd.join(".rudder").join("steer-receipts");
        fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{request_id}.json"));
        let temp = dir.join(format!("{request_id}.{}.tmp", std::process::id()));
        let value = serde_json::json!({"requestId": request_id, "status": "delivered", "runId": run_id});
        fs::write(&temp, serde_json::to_vec(&value)?)?;
        fs::rename(temp, file)
    }
}

/// A helper's turn ended: append its final message to its record, off the
/// App loop. Called once per turn by detect.rs `finish_turn`.
pub(crate) fn spawn_helper_reply(run: &AgentRun) {
    if run.parent_run.is_none() {
        return;
    }
    let job = HelperReplyJob {
        repo: dashboard_root(&run.cwd),
        run_id: run.id.clone(),
        backend: run.backend,
        session: run.session_id.clone(),
        cwd: run.cwd.clone(),
        account: run.account.clone(),
    };
    let _ = thread::Builder::new()
        .name("rudder-helper-reply".to_string())
        .spawn(move || job.run());
}

/// A helper's turn failed (a usage limit, an API error; the App loop's
/// `SignalState::Failed`). Its process waits at its prompt and ends no turn,
/// so without this its parent would wait for a reply that is not coming.
pub(crate) fn spawn_helper_failure(run: &AgentRun, detail: &str) {
    if run.parent_run.is_none() {
        return;
    }
    let repo = dashboard_root(&run.cwd);
    let run_id = run.id.clone();
    let reply = HelperReply { at: now_stamp(), text: None, error: Some(format!("the helper's turn failed: {detail}")) };
    let _ = thread::Builder::new()
        .name("rudder-helper-reply".to_string())
        .spawn(move || {
            let _ = append_helper_reply(&repo, &run_id, reply);
        });
}

pub(crate) struct HelperReplyJob {
    pub(crate) repo: PathBuf,
    pub(crate) run_id: String,
    pub(crate) backend: Backend,
    pub(crate) session: Option<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) account: Option<String>,
}

impl HelperReplyJob {
    pub(crate) fn run(self) {
        let deadline = Instant::now() + REPLY_WAIT;
        let reply = loop {
            match self.final_text() {
                Ok(text) => break HelperReply { at: now_stamp(), text: Some(text), error: None },
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_secs(1)),
                Err(reason) => break HelperReply {
                    at: now_stamp(),
                    text: None,
                    error: Some(format!("the helper's turn ended, but its final message could not be read: {reason}")),
                },
            }
        };
        let _ = append_helper_reply(&self.repo, &self.run_id, reply);
    }

    /// The turn's final message from the backend's own session record.
    pub(crate) fn final_text(&self) -> Result<String, String> {
        let session = match (&self.session, self.backend) {
            (Some(session), _) => session.clone(),
            // Antigravity names its conversation through its SessionStart hook.
            (None, Backend::Antigravity) => signals::read_antigravity_session(&self.run_id)
                .ok_or("Antigravity has not reported its conversation")?,
            (None, backend) => return Err(format!("{} has not reported its session", backend.as_str())),
        };
        let account_dir = signals::account_env(self.backend, self.account.as_deref())
            .map_err(|error| error.to_string())?
            .map(|(_, dir)| dir);
        let text = match self.backend {
            Backend::Claude => {
                let root = match account_dir {
                    Some(dir) => dir,
                    None => user_home_dir().ok_or("HOME is not set")?.join(".claude"),
                };
                let path = root
                    .join("projects")
                    .join(encode_claude_project_dir(&self.cwd))
                    .join(format!("{session}.jsonl"));
                let raw = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                parse_transcript_final_text(&raw)
            }
            Backend::Codex => {
                let sessions = match account_dir {
                    Some(dir) => dir,
                    None => codex_home()?,
                }
                .join("sessions");
                let path = crate::chat_edit::codex_rollout_file(&sessions, &session)?;
                let raw = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                codex_final_text(&raw)
            }
            Backend::Opencode => crate::opencode::final_assistant_text(&session),
            Backend::Antigravity => crate::antigravity::final_assistant_text(&session),
        };
        text.ok_or_else(|| format!("{} session {session} has no final message yet", self.backend.as_str()))
    }
}

/// The last finished turn's message in a Codex rollout (`task_complete`).
pub(crate) fn codex_final_text(raw: &str) -> Option<String> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg"))
        .filter_map(|value| {
            let payload = value.get("payload")?;
            (payload.get("type")?.as_str()? == "task_complete")
                .then(|| payload.get("last_agent_message")?.as_str().map(str::to_string))
                .flatten()
        })
        .filter(|text| !text.trim().is_empty())
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rudder-helpers-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_helper_request_names_harness_model_and_task_on_its_own_lines() {
        assert_eq!(parse_helper_instruction("antigravity gemini-4-neon-xhigh").unwrap_err(), SHAPE);
        assert_eq!(parse_helper_instruction("antigravity\nreview lib/").unwrap_err(), SHAPE);
        assert_eq!(parse_helper_instruction("claude opus high extra\nreview").unwrap_err(), SHAPE);
        assert!(parse_helper_instruction("gemini-cli neon\nreview")
            .unwrap_err()
            .starts_with("unknown harness: gemini-cli"));
    }

    #[test]
    fn the_helper_prompt_names_its_copy_of_the_parents_files() {
        let prompt = helper_prompt("\n  Review lib/ and test/.\nTop five only.", "ship the parser");
        assert!(prompt.starts_with("Objective: Review lib/ and test/.\nDone when: "), "{prompt}");
        assert!(starts_goal_prompt(&prompt), "manual_goal_prompt keeps it as written");
        assert!(prompt.contains("Top five only."));
        assert!(prompt.contains("\"ship the parser\" started you"));
        assert!(prompt.contains("starts as a copy of that agent's files, its uncommitted edits included"));
        assert!(prompt.contains("never reach that agent's files"));
    }

    #[test]
    fn replies_append_once_per_turn_and_records_refuse_unsafe_ids() {
        let repo = temp_repo("record");
        let record = HelperRecord {
            run_id: "review-1a2b".into(),
            parent_run_id: "main".into(),
            harness: "antigravity".into(),
            model: "gemini-4-neon-xhigh".into(),
            effort: None,
            replies: Vec::new(),
        };
        write_helper_record(&repo, &record).unwrap();
        let reply = |text: &str| HelperReply { at: "1".into(), text: Some(text.into()), error: None };
        append_helper_reply(&repo, "review-1a2b", reply("first")).unwrap();
        append_helper_reply(&repo, "review-1a2b", reply("first")).unwrap();
        append_helper_reply(&repo, "review-1a2b", reply("second")).unwrap();
        let texts: Vec<_> = read_helper_record(&repo, "review-1a2b").unwrap().replies.into_iter().filter_map(|r| r.text).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(read_helper_record(&repo, "other").unwrap_err(), "no helper other in this project");
        for bad in ["", "../x", ".hidden", "a/b"] {
            assert!(read_helper_record(&repo, bad).unwrap_err().contains("is not a run id"), "{bad:?}");
        }
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn a_codex_reply_is_the_last_finished_turns_message() {
        let raw = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/codex-0.170-rollout.jsonl")).unwrap();
        // The fixture's later turn ended with no message; the reply is the one before.
        assert_eq!(codex_final_text(&raw).as_deref(), Some("hello"));
        let turns = [
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"one"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"draft"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"two"}}"#,
        ];
        assert_eq!(codex_final_text(&turns.join("\n")).as_deref(), Some("two"));
        assert_eq!(codex_final_text(""), None);
    }
}
