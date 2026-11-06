//! Actual App bootstrap and retirement. Views borrow this App's PTYs; they
//! never acquire a writer, create an App, or run a scheduler of their own.
use super::*;
use rudder_engine::journal::{Journal, OwnerFence};

pub(crate) struct AppEngineLifecycle {
    repository: PathBuf,
    fence: OwnerFence,
    // A second connection to the actual owner's held database, not a database
    // selected from a projection/path. Opening it creates no scheduling owner.
    writer: Option<Journal>,
    lease: Option<EngineSourceLease>,
    control: Option<EngineControl>,
}

impl AppEngineLifecycle {
    /// Before constructing App or resuming any work. Failure leaves an explicit
    /// uncertain owner receipt; Drop never attempts SQLite reconciliation.
    pub(crate) fn prepare(owner: &mut EngineOwnership) -> Result<Self> {
        let prepared: Result<Self> = (|| {
            owner.assert_current()?;
            let (repository, fence) = owner.app_identity();
            let (lease, writer) = if owner.is_fabric() {
                let lease = owner.source_lease(fence)?;
                let writer = lease.open_source_writer()?;
                (Some(lease), Some(writer))
            } else {
                (None, None)
            };
            Ok(Self {
                repository: repository.into(),
                fence: fence.clone(),
                lease,
                writer,
                control: None,
            })
        })();
        match prepared {
            Ok(lifecycle) => Ok(lifecycle),
            Err(error) => {
                let reconciliation = owner.finish(false);
                match reconciliation {
                    Ok(()) => Err(error),
                    Err(reconcile) => {
                        Err(error.context(format!("startup reconciliation failed: {reconcile:#}")))
                    }
                }
            }
        }
    }

    fn check(&self, app: &App, owner: &EngineOwnership) -> Result<()> {
        let (repository, fence) = owner.app_identity();
        anyhow::ensure!(
            self.repository == repository && self.fence == *fence && app.cwd == self.repository,
            "App differs from its actual engine owner"
        );
        if let Some(lease) = &self.lease {
            lease.check_active_for_adoption()?;
        }
        Ok(())
    }

    /// The App actor binds its ingress before resuming sessions. All sockets
    /// belong to this single owner, including when no surfaces are attached.
    pub(crate) fn start(
        &mut self,
        app: &mut App,
        owner: &mut EngineOwnership,
    ) -> Result<serde_json::Value> {
        self.check(app, owner)?;
        owner.assert_current()?;
        anyhow::ensure!(
            app.attach_server.is_none() && self.control.is_none(),
            "App ingress is already bound"
        );
        if let Some(recovery) = owner.restart_recovery() {
            apply_recovered_runtimes_on_app(app, recovery)?;
        }
        app.attach_server = Some(AttachServer::start(
            app.attach_socket_path(),
            app.pty_output_waker.clone(),
        )?);
        if owner.is_fabric() {
            self.control = Some(EngineControl::bind(owner, app.pty_output_waker.clone())?);
        }
        app.resume_migrated_agents();
        app.reconcile_orphaned_runs();
        app.mirror_graph();
        owner.ready(self.control.as_ref().map(EngineControl::path))
    }

    /// Only the App loop calls this. Worker messages are still adopted by the
    /// existing runtime's exact run/attempt/owner fences, with no extra scheduler.
    pub(crate) fn poll(&mut self, app: &mut App, owner: &mut EngineOwnership) -> Result<bool> {
        self.check(app, owner)?;
        owner.assert_current()?;
        if self
            .control
            .as_mut()
            .is_some_and(|control| control.poll(owner, app))
        {
            return Ok(true);
        }
        if let Some(writer) = &mut self.writer {
            fabric_runtime::poll(app, writer, &self.fence)?;
        }
        Ok(false)
    }

    pub(crate) fn finish(
        mut self,
        app: &mut App,
        owner: &mut EngineOwnership,
        result: Result<()>,
    ) -> Result<()> {
        let reason = if owner.is_fabric() {
            "the Rudder engine shut down; resume explicitly"
        } else {
            "the Rudder dashboard closed; resume explicitly"
        };
        let can_publish = self.repository == app.cwd && owner.assert_app_cleanup_epoch().is_ok();
        let shutdown = shutdown_app_until(
            app,
            reason,
            Instant::now() + Duration::from_secs(3),
            can_publish,
        );
        // An epoch change stops new work but cannot be used to publish over the
        // successor. Preserve the original runtime drain error independently.
        let runtime_shutdown = if let Some(writer) = &mut self.writer {
            owner
                .assert_current()
                .and_then(|()| fabric_runtime::shutdown(app, writer, &self.fence))
        } else {
            Ok(())
        };
        self.control.take();
        let released = owner.finish(result.is_ok() && shutdown.is_ok() && runtime_shutdown.is_ok());
        // Preserve all failures. An earlier loop error must not hide failure to
        // persist paused tasks or reconcile the owner.
        let errors: Vec<_> = [result, shutdown, runtime_shutdown, released]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
}

/// Startup application is performed by the same App actor that owns PTYs.
/// Recovery never substitutes a remembered PID for an owned live terminal.
fn apply_recovered_runtimes_on_app(
    app: &mut App,
    recovery: &crate::engine_host::restart::NativeRestartRecovery,
) -> Result<()> {
    use crate::engine_host::restart::RecoveredRuntimeDisposition::*;
    // Validate the entire batch before changing any App row. A remembered
    // observation cannot retire a terminal still owned by this App actor.
    for disposition in &recovery.runtimes {
        let attempt = match disposition {
            Paused(attempt) | Completed(attempt) | Failed(attempt) | Quarantined {attempt, ..} => attempt,
        };
        anyhow::ensure!(!app.agents.iter().any(|run|run.id == attempt.run_id && run.terminal.is_some()),
            "recovered runtime still has an actual App terminal");
    }
    for disposition in &recovery.runtimes {
        let attempt = match disposition {
            Paused(attempt) | Completed(attempt) | Failed(attempt) | Quarantined {attempt, ..} => attempt,
        };
        let Some(run) = app.agents.iter_mut().find(|run|run.id == attempt.run_id) else {continue;};
        run.needs_permission = false;
        run.needs_user_input = false;
        match disposition {
            Paused(_) => {
                if matches!(run.status, AgentStatus::Running | AgentStatus::Paused | AgentStatus::Migrated | AgentStatus::Orphaned) {
                    run.status = AgentStatus::Paused;
                    run.completed_at = None;
                    run.last_error = Some("engine restarted; resume explicitly".into());
                }
            }
            Quarantined {..} => {
                if matches!(run.status, AgentStatus::Running | AgentStatus::Paused | AgentStatus::Migrated | AgentStatus::Orphaned) {
                    run.status = AgentStatus::Orphaned;
                    run.completed_at = None;
                }
                run.last_error = Some("runtime custody requires explicit reconciliation".into());
            }
            Completed(_) | Failed(_) => {
                if matches!(run.status, AgentStatus::Running | AgentStatus::Paused | AgentStatus::Migrated | AgentStatus::Orphaned) {
                    run.status = if matches!(disposition, Completed(_)) {AgentStatus::Done} else {AgentStatus::Failed};
                }
            }
        }
        app.pending_migration_resumes.retain(|entry|entry.run_id != attempt.run_id);
        app.boot_orphaned_ids.remove(&attempt.run_id);
        app.restart_home_boot_suppressed.insert(attempt.run_id.clone());
        if matches!(disposition, Quarantined {..}) {
            app.restart_quarantined_ids.insert(attempt.run_id.clone());
        } else {
            app.restart_quarantined_ids.remove(&attempt.run_id);
        }
        save_native_run_record(&app.cwd, run)?;
    }
    Ok(())
}

/// Explicit engine/task retirement only. Surface detach never calls this.
/// The App actor retains panes while the reserved process reaper works, and
/// polls observed exit rather than turning a termination request into Paused.
pub(crate) fn shutdown_app(app: &mut App, reason: &str) -> Result<()> {
    shutdown_app_until(app, reason, Instant::now() + Duration::from_secs(3), true)
}

fn shutdown_app_until(
    app: &mut App,
    reason: &str,
    deadline: Instant,
    can_publish: bool,
) -> Result<()> {
    app.close_attach_clients(reason);
    let mut failures = Vec::new();
    let mut pending = Vec::new();
    for (index, run) in app.agents.iter_mut().enumerate() {
        let Some(terminal) = run.terminal.as_mut() else {
            continue;
        };
        terminal.request_termination();
        pending.push(index);
    }
    while !pending.is_empty() {
        pending.retain(|index| {
            let run = &mut app.agents[*index];
            let terminal = run.terminal.as_mut().unwrap();
            terminal.drain_output();
            match terminal.try_wait() {
                Ok(Some(_)) => {
                    if run.backend == Backend::Codex && run.session_id.is_none() {
                        run.session_id = latest_codex_session_id_for_cwd(&run.cwd);
                    }
                    run.terminal = None;
                    if run.status == AgentStatus::Running {
                        // Mid-turn agents get one "continue" turn when the home
                        // machine boots again; agents idle at their prompt do not.
                        run.resume_turn = agent_is_busy(run);
                        run.status = AgentStatus::Paused;
                        run.needs_permission = false;
                        run.needs_user_input = false;
                        run.completed_at = None;
                        run.last_error = Some(format!("paused because {reason}"));
                    }
                    // Save Done processes too: actual exit removes `process`.
                    if let Err(error) = if can_publish { save_native_run_record(&app.cwd, run) } else { Ok(()) } {
                        failures.push(format!("{}: {error:#}", run.id));
                    }
                    false
                }
                Ok(None) if Instant::now() < deadline => true,
                outcome => {
                    let detail = match outcome { Err(error) => format!("{error:#}"),
                        _ => "process exit remains unconfirmed".into() };
                    run.last_error = Some(format!("shutdown uncertain because {reason}: {detail}"));
                    failures.push(format!("{}: {detail}", run.id));
                    // Keep the actual pane and live process record. Recovery
                    // must not silently resume or replace uncertain work.
                    if let Err(error) = if can_publish { save_native_run_record(&app.cwd, run) } else { Ok(()) } {
                        failures.push(format!("{}: {error:#}", run.id));
                    }
                    false
                }
            }
        });
        if !pending.is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
    }
    if let Err(error) = if can_publish {
        app.write_rudder_context_timed(None)
    } else {
        Ok(())
    } {
        failures.push(format!("context: {error:#}"));
    }
    anyhow::ensure!(
        failures.is_empty(),
        "could not persist paused engine state: {}",
        failures.join("; ")
    );
    Ok(())
}

#[cfg(test)]
#[path = "../app_engine_lifecycle_tests.rs"]
mod tests;
