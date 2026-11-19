//! `P`: switch the selected agent's model and effort, as the model control in
//! the app's chat does (steer kind "model", `deliver_model_request`). The task
//! pane's `/model` picker is the chooser, opened at the agent's own backend
//! with its current model marked; a pick relaunches the agent's conversation
//! on the new flags when it is at its prompt. `/model` typed into the task
//! pane on its own still sets the model new agents start on.
//!
//! A model of another harness, picked in the app's chat, moves the chat there
//! instead (steer kind "harness", `deliver_harness_request`).

use super::*;

impl App {
    /// `P` in the agent list.
    pub(crate) fn open_agent_model_picker(&mut self) {
        if self.subagent_cursor.is_some()
            || self.drawer_cursor.is_some_and(|bucket| self.drawer_open() != Some(bucket))
        {
            return;
        }
        let Some(run) = self.agents.get(self.selected_agent) else {
            // No agent yet: the picker sets the model new agents start on.
            self.model_for = None;
            self.replace_task_input("/model ".to_string());
            self.select_current_model_picker_choice();
            self.focus = FocusPane::Task;
            self.notice = Some("pick the model new agents start on".to_string());
            return;
        };
        if let Some(refusal) = model_switch_refusal(run) {
            self.notice = Some(refusal.to_string());
            return;
        }
        let name = short_task(if run.task_summary.trim().is_empty() { &run.task } else { &run.task_summary });
        let backend = run.backend;
        self.model_for = Some(run.id.clone());
        self.replace_task_input(format!("/model {} ", backend.as_str()));
        self.select_current_model_picker_choice();
        self.focus = FocusPane::Task;
        self.notice = Some(format!("pick a model and effort for {name}"));
    }

    /// The run id the `/model` draft is choosing for, if any.
    fn model_target_id(&self) -> Option<String> {
        self.model_for
            .clone()
            .filter(|_| self.task_input.trim_start().starts_with("/model"))
    }

    /// The agent the `/model` draft is choosing for, if any.
    pub(crate) fn model_picker_target(&self) -> Option<&AgentRun> {
        let id = self.model_target_id()?;
        self.agents.iter().find(|run| run.id == id)
    }

    /// What the `/model` picker marks as current: the target agent's model, or
    /// the default for new agents.
    pub(crate) fn model_picker_current(&self) -> (Backend, String, Option<EffortLevel>) {
        match self.model_picker_target() {
            Some(run) => (run.backend, run.model.clone(), run.effort),
            None => (self.backend, self.model.clone(), self.effort),
        }
    }

    /// A finished task-pane key: a draft that is no longer a `/model` line
    /// is not choosing for an agent any more.
    pub(crate) fn forget_stale_model_target(&mut self) {
        if self.model_for.is_some() && !self.task_input.trim_start().starts_with("/model") {
            self.model_for = None;
        }
    }

    /// A model picked (or a `/model` line entered) while the draft chooses for
    /// an agent. True when it was handled here.
    pub(crate) fn apply_agent_model_choice(&mut self, model: &str, effort: Option<EffortLevel>) -> bool {
        // An agent deleted while its picker was open fails below ("target not
        // found"); it never turns into a change of the default.
        let Some(id) = self.model_target_id() else {
            return false;
        };
        self.model_for = None;
        self.task_input.clear();
        self.task_cursor = 0;
        self.picker_index = 0;
        let spec = format!("{model} {}", effort.map(EffortLevel::as_str).unwrap_or("auto"));
        match self.deliver_model_request(&id, &spec) {
            SteerDelivery::Delivered => {
                if let Some(index) = self.agent_index_for_token(&id) {
                    let run = &self.agents[index];
                    self.notice = Some(format!("now on {}", agent_model_label(run.backend, &run.model, run.effort)));
                    self.selected_agent = index;
                }
                self.focus = FocusPane::Worker;
            }
            SteerDelivery::Failed(why) => {
                self.notice = Some(format!("model not changed: {why}"));
                self.focus = FocusPane::Agents;
            }
        }
        self.dirty = true;
        true
    }

    /// A typed `/model [provider] <model> [effort]` line while choosing for an
    /// agent. True when it was handled here.
    pub(crate) fn apply_agent_model_command(&mut self, args: &[&str]) -> bool {
        if self.model_target_id().is_none() {
            return false;
        }
        let Some(backend) = self.model_picker_target().map(|run| run.backend) else {
            self.model_for = None;
            self.notice = Some("model not changed: that agent is gone".to_string());
            return true;
        };
        let args = match args.split_first() {
            Some((first, rest)) if provider_backend(first).is_some() => rest,
            _ => args,
        };
        if args.is_empty() {
            self.notice = Some("pick a model, then an effort".to_string());
            return true;
        }
        // The agent's own backend, and only a model its list knows.
        let mut full = vec![backend.as_str()];
        full.extend_from_slice(args);
        match parse_model_args(&full) {
            Ok((_, model, effort)) => self.apply_agent_model_choice(&model, effort),
            Err(refusal) => {
                self.model_for = None;
                self.notice = Some(format!("model not changed: {refusal}"));
                true
            }
        }
    }
}

/// Why this row keeps the model it was started with, the same rows
/// `deliver_model_request` refuses.
pub(crate) fn model_switch_refusal(run: &AgentRun) -> Option<&'static str> {
    (run.is_orchestrator() || run.gam.is_some() || run.plain_process || is_cloud_owned_agent(run))
        .then_some("this agent keeps the model it was started with")
}

/// Why this row cannot move to `backend`, or `None` when it may.
fn harness_switch_refusal(run: &AgentRun, backend: Backend) -> Option<String> {
    if let Some(refusal) = model_switch_refusal(run) {
        return Some(refusal.to_string());
    }
    if run.backend == backend {
        return Some(format!("this chat already runs on {}; pick one of its models instead", backend.as_str()));
    }
    if run.status == AgentStatus::Merged {
        return Some("its work is already merged; start a new agent instead".to_string());
    }
    if !run.cwd.is_dir() {
        return Some(format!("its workspace is gone: {}", run.cwd.display()));
    }
    None
}

impl App {
    /// Steer kind "harness": move one chat to another harness, from the app's
    /// model control. The instruction is `<harness> <model> <effort|auto>`,
    /// then the handoff on the lines after it, which the home builds from the
    /// old conversation (cloud/worker/handoff.mjs). The row keeps its id,
    /// workspace and files. Its old process ends, and the new harness starts a
    /// new conversation whose first prompt is the handoff, framed the way the
    /// row's mode frames a task. A row mid-turn moves too: the handoff says
    /// what it was doing, and the new harness carries on with it.
    pub(crate) fn deliver_harness_request(&mut self, task_id: &str, instruction: &str) -> SteerDelivery {
        let expected = || SteerDelivery::Failed("expected <harness> <model> <effort|auto>, then the handoff".to_string());
        let Some((head, handoff)) = instruction.split_once('\n') else {
            return expected();
        };
        let mut parts = head.split_whitespace();
        let (Some(harness), Some(model), Some(effort_arg), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return expected();
        };
        let Some(backend) = Backend::parse(harness) else {
            return SteerDelivery::Failed(format!("unknown harness: {harness}"));
        };
        let effort = parse_effort_arg(effort_arg);
        if effort.is_none() && !effort_arg.eq_ignore_ascii_case("auto") {
            return SteerDelivery::Failed(format!("unknown effort: {effort_arg}"));
        }
        let handoff = sanitize_steer_instruction(handoff);
        if handoff.is_empty() {
            return SteerDelivery::Failed("the handoff is empty".to_string());
        }
        let Some(index) = self.agent_index_for_token(task_id) else {
            return SteerDelivery::Failed(format!("target not found: {task_id}"));
        };
        let run = &mut self.agents[index];
        if let Some(refusal) = harness_switch_refusal(run, backend) {
            return SteerDelivery::Failed(refusal);
        }
        if let Some(other) = known_model_backend(model).filter(|other| *other != backend) {
            return SteerDelivery::Failed(format!("{model} is a {} model, not a {} one", other.as_str(), backend.as_str()));
        }
        if !effort_options_for(backend, model).contains(&effort) {
            return SteerDelivery::Failed(format!("{model} has no {effort_arg} effort"));
        }
        if let Some(refusal) = openrouter_key_refusal(backend, model) {
            return SteerDelivery::Failed(refusal.to_string());
        }
        let label = run.node_id.clone().unwrap_or_else(|| run.id.clone());
        let from = agent_model_label(run.backend, &run.model, run.effort);
        let session = mint_session_id_for(backend);
        let mut command = agent_command(backend, model, effort, &handoff, run.mode, session.as_deref());
        let wired = if run.is_main() && run.execution_profile.is_none() {
            signals::augment_worker_command(&mut command, backend, run.mode, &run.id)
        } else {
            // A profile belongs to the harness that ran it (claude-sdk,
            // codex-app-server): the new harness starts on the native one.
            signals::augment_existing_worker_command(&mut command, backend, run.mode, &run.id, None)
        };
        // Refused before the old process is touched: it keeps running.
        if let Err(error) = wired {
            return SteerDelivery::Failed(format!("could not start {label} on {}: {error}", backend.as_str()));
        }
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
                return SteerDelivery::Failed(format!("could not start {label} on {}: {error}", backend.as_str()));
            }
        };
        let _ = terminal.drain_output();
        Self::attach_output_waker(&self.pty_output_waker, &terminal);
        let now = now_stamp();
        let run = &mut self.agents[index];
        run.backend = backend;
        run.model = model.to_string();
        run.effort = effort;
        run.execution_profile = terminal.execution_profile().map(str::to_string);
        run.terminal = Some(terminal);
        run.session_id = session.clone();
        run.status = AgentStatus::Running;
        run.completed_at = None;
        run.last_output_at = Instant::now();
        run.needs_permission = false;
        run.needs_user_input = false;
        run.wait_signal = None;
        run.last_error = None;
        run.worker_exit_note = None;
        // A new conversation: until the new harness writes it, the chat shows
        // the prompts recorded here, and the old harness's are not in it.
        run.turns.clear();
        run.turns.push(AgentTurn { ts: now.clone(), prompt: handoff.clone(), source: "user".to_string() });
        run.current_prompt = handoff;
        run.last_user_input_at = now;
        run.last_worker_input_at = Some(Instant::now());
        let mut unrecorded = Vec::new();
        if let Err(error) = save_native_run_record(&self.cwd, run) {
            unrecorded.push(format!("its record was not saved: {error}"));
        }
        // The row follows the session its worker's hooks last reported. That
        // report may name a session from an earlier stint on this harness, so
        // the new one is recorded now (Claude), or the old report goes (Codex
        // names its new thread once it starts).
        let reported = match session.as_deref().filter(|_| backend == Backend::Claude) {
            Some(session) => signals::claude_session_path(&run.id)
                .ok_or_else(|| "HOME is not set".to_string())
                .and_then(|file| {
                    let temp = file.with_extension(format!("claude-session.{}.tmp", std::process::id()));
                    std::fs::write(&temp, session)
                        .and_then(|()| std::fs::rename(&temp, &file))
                        .map_err(|error| error.to_string())
                }),
            None => signals::signal_path(&run.id)
                .ok_or_else(|| "HOME is not set".to_string())
                .and_then(|signal| match std::fs::remove_file(format!("{}.session", signal.display())) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
                    _ => Ok(()),
                }),
        };
        if let Err(error) = reported {
            unrecorded.push(format!("its new session was not recorded: {error}"));
        }
        let to = agent_model_label(backend, model, effort);
        self.push_activity(format!("{label}: moved from {from} to {to}"));
        self.mirror_graph();
        self.dirty = true;
        if unrecorded.is_empty() {
            SteerDelivery::Delivered
        } else {
            SteerDelivery::Failed(format!("it moved to {to}, but {}", unrecorded.join("; ")))
        }
    }
}
