//! `P`: switch the selected agent's model and effort, as the model control in
//! the app's chat does (steer kind "model", `deliver_model_request`). The task
//! pane's `/model` picker is the chooser, opened at the agent's own backend
//! with its current model marked; a pick relaunches the agent's conversation
//! on the new flags when it is at its prompt. `/model` typed into the task
//! pane on its own still sets the model new agents start on.

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
