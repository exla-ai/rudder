//! App-side adapter for normal managed retirement. The source/task workers
//! retain actual helper, checkpoint and join custody. This actor only requests
//! their normal stop and adopts their existing fenced messages; it never turns
//! a signal, process exit or timeout into a drain acknowledgement.
use super::*;

pub(crate) struct ManagedAppDrain {
    repository: PathBuf,
    fence: OwnerFence,
    deadline: Instant,
    requested: bool,
    joined: bool,
    // Joined source checkpoint DATA stays with the original App owner. Taking
    // it allows the source supervisor's actual cleanup to finish, and neither
    // publishes nor accepts history. Task capsules stay with their ingress
    // owner until its explicit publication/discard has settled them.
    checkpoints: Vec<managed_source_app::JoinedSourceCheckpoint>,
}

impl ManagedAppDrain {
    pub(crate) fn capture(owner: &EngineOwnership) -> Self {
        let (repository, fence) = owner.app_identity();
        Self {
            repository: repository.into(), fence: fence.clone(),
            deadline: Instant::now() + Duration::from_secs(30),
            requested: false, joined: false, checkpoints: Vec::new(),
        }
    }

    /// Called only by the original managed App loop. Pending keeps that App,
    /// its panes and its engine owner alive. Ready proves only App custody,
    /// never the machine's other source/seed/Root producers. Error also retains
    /// the same drain and deadline; callers must not finish the machine owner
    /// without that independent runtime proof.
    pub(crate) fn poll(&mut self, app: &mut App, owner: &mut EngineOwnership,
        gate: &managed_machine_entry::ManagedMachineStartupGate) -> Result<bool> {
        let (repository, fence) = owner.app_identity();
        anyhow::ensure!(app.managed_project_mode && self.repository == repository
            && app.cwd == self.repository && self.fence == *fence,
            "managed App drain differs from its actual engine owner");
        owner.assert_app_cleanup_epoch()?;
        if self.joined { return Ok(true); }
        anyhow::ensure!(Instant::now() < self.deadline,
            "managed App drain timed out; original helper/checkpoint owners retained");
        if !self.requested {
            for index in 0..app.agents.len() {
                if app.agents[index].status != AgentStatus::Running
                    && app.agents[index].terminal.is_none() { continue; }
                if let Some(fence) = app.agents[index].managed_local.as_ref()
                    .and_then(managed_launch::ManagedRunSource::fence) {
                    anyhow::ensure!(fence.owner == self.fence,
                        "managed App drain contains another run owner");
                    anyhow::ensure!(managed_app::stop(app, index) == Some(true),
                        "managed App refused its normal retained stop");
                } else {
                    anyhow::ensure!(app.agents[index].terminal.is_none(),
                        "managed App has a pane without retained runtime ownership");
                }
            }
            self.requested = true;
        }
        let (journal, fence) = owner.journal_parts();
        managed_source_app::poll(app, fence);
        managed_app::poll_with_startup_gate(app, journal, fence, gate)?;
        fabric_runtime::poll(app, journal, fence)?;
        app.poll_agents();
        let ids: Vec<_> = app.agents.iter().map(|run| run.id.clone()).collect();
        for id in ids {
            if let Some(checkpoint) = app.take_joined_source_checkpoint(&id) {
                self.checkpoints.push(checkpoint);
            }
        }
        if app.ensure_managed_source_shutdown_ready().is_err()
            || app.ensure_managed_task_shutdown_ready().is_err()
            || app.agents.iter().any(|run| run.terminal.is_some()) {
            return Ok(false);
        }
        // No helper/supervisor/checkpoint authority remains in the App. Source
        // generation/seed/Root owners are outside this actor, so this cannot
        // call App shutdown or EngineOwnership::finish on their behalf.
        fabric_runtime::shutdown(app, journal, fence)?;
        self.joined = true;
        Ok(true)
    }
}
