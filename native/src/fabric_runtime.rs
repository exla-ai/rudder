//! Durable commands enter the existing App actor here. Only this thread owns
//! App/PTys; the journal records intent before any external effect starts.

use super::*;
use crate::agent_broker::{self, AgentBroker, BrokerConfig};
use crate::agent_link::{
    self, AgentLinkCommand, AgentLinkEvent, AgentLinkEventKind, AgentLinkHandle, AgentLinkLaunch,
    AgentLinkOperation, AgentLinkOutcome, PeerMessage,
};
use rudder_engine::agent_api::AgentIdentityFile;
use rudder_engine::journal::{CapabilityGrant, Journal, OwnerFence};
use rudder_engine::journal::{EffectClaim, Reconciliation, RuntimeObservation, RuntimePhase};
use rudder_engine::paths::FabricPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    mpsc::{Receiver, Sender},
    Arc,
};

/// Transport threads produce evidence. This state belongs exclusively to the
/// App actor; it neither owns another scheduler nor mutates App from a thread.
pub(crate) struct RuntimeState {
    events_tx: Sender<AgentLinkEvent>,
    events_rx: Receiver<AgentLinkEvent>,
    broker_tx: Sender<BrokerResult>,
    broker_rx: Receiver<BrokerResult>,
    links: BTreeMap<String, RuntimeLink>,
    pending: BTreeMap<String, PendingControl>,
    last_housekeeping: Option<Instant>,
}

struct RuntimeLink {
    attempt: String,
    epoch: u64,
    profile: String,
    backend: Backend,
    transport_uncertain: bool,
    link: Option<AgentLinkHandle>,
    broker: Option<AgentBroker>,
    broker_work: Option<BrokerWork>,
    projection_hash: Option<[u8; 32]>,
    broker_failed_hash: Option<[u8; 32]>,
    thread: Option<String>,
    active: Option<String>,
    interrupted: Option<String>,
    phase: RuntimePhase,
    qualified: bool,
    uncertain: bool,
    closed: bool,
}

struct BrokerWork {
    hash: [u8; 32],
    cancelled: Arc<AtomicBool>,
}
struct BrokerResult {
    run_id: String,
    attempt: String,
    run_epoch: u64,
    owner_epoch: u64,
    hash: [u8; 32],
    result: Result<Option<AgentBroker>>,
}

#[derive(Clone)]
struct PendingControl {
    claim: EffectClaim,
    attempt: String,
    deadline: Instant,
    uncertain: bool,
    /// Reserve the row before waking an idle conversation, so integration
    /// cannot race the accepted-but-not-yet-started replacement turn.
    reserved_status: Option<(AgentStatus, Option<Instant>)>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (broker_tx, broker_rx) = std::sync::mpsc::channel();
        Self {
            events_tx,
            events_rx,
            broker_tx,
            broker_rx,
            links: BTreeMap::new(),
            pending: BTreeMap::new(),
            last_housekeeping: None,
        }
    }
}

impl RuntimeLink {
    fn observation(&self, run_id: &str, backend: Backend, owned: bool) -> RuntimeObservation {
        let mut capabilities = BTreeSet::from(["inspect".to_string()]);
        if owned {
            capabilities.insert("cancel".into());
        }
        if owned && !self.closed && !self.uncertain && self.thread.is_some() {
            if self.qualified {
                match self.phase {
                    RuntimePhase::Working => {
                        capabilities.extend(["steer".into(), "interrupt".into()]);
                    }
                    RuntimePhase::Interrupted => {
                        capabilities.insert("resume".into());
                    }
                    RuntimePhase::Waiting | RuntimePhase::Completed => {
                        capabilities.insert("deliver".into());
                    }
                    _ => {}
                }
            } else if self.profile == "claude-sdk" && self.phase == RuntimePhase::Waiting {
                // A resumed SDK process has a trusted requested session but no
                // system/init until actual input. The bridge permits only the
                // first idle inbox item, then checks observed CLI capabilities.
                capabilities.insert("deliver".into());
            }
        }
        RuntimeObservation {
            run_id: run_id.into(),
            run_epoch: self.epoch,
            attempt_id: self.attempt.clone(),
            backend: backend.as_str().into(),
            execution_profile: self.profile.clone(),
            thread_id: self.thread.clone(),
            active_turn_id: self.active.clone(),
            last_interrupted_turn_id: self.interrupted.clone(),
            phase: if self.uncertain {
                RuntimePhase::Uncertain
            } else if !owned {
                RuntimePhase::Closed
            } else {
                self.phase
            },
            capabilities,
            pty_owned: owned,
        }
    }

    fn close(&mut self) {
        if let Some(link) = &self.link {
            link.close();
        }
        if let Some(broker) = &self.broker {
            broker.close();
        }
        if let Some(work) = &self.broker_work {
            work.cancelled.store(true, Ordering::Release);
        }
        self.closed = true;
    }

    fn close_callback(&self) -> Arc<dyn Fn() + Send + Sync> {
        let link = self.link.as_ref().map(AgentLinkHandle::close_callback);
        let broker = self.broker.as_ref().map(AgentBroker::close_callback);
        let work = self
            .broker_work
            .as_ref()
            .map(|work| Arc::clone(&work.cancelled));
        Arc::new(move || {
            if let Some(work) = &work {
                work.store(true, Ordering::Release);
            }
            if let Some(callback) = &link {
                callback();
            }
            if let Some(callback) = &broker {
                callback();
            }
        })
    }
}

impl RuntimeState {
    pub(crate) fn close(&mut self, run_id: &str) {
        if let Some(link) = self.links.get_mut(run_id) {
            link.close();
        }
    }

    fn poll(&mut self, app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
        let now = epoch_ms()?;
        self.broker_results(app, owner);
        let housekeeping = self
            .last_housekeeping
            .is_none_or(|last| last.elapsed() >= Duration::from_millis(500));
        let inventory_changed = app
            .agents
            .iter()
            .filter(|run| !run.plain_process && !is_cloud_owned_agent(run))
            .any(|run| {
                run.terminal.as_ref().is_some_and(|terminal| {
                    self.links
                        .get(&run.id)
                        .is_none_or(|slot| slot.attempt != terminal.instance_id())
                })
            })
            || self.links.iter().any(|(id, slot)| {
                !slot.closed
                    && !app.agents.iter().any(|run| {
                        run.id == *id
                            && run
                                .terminal
                                .as_ref()
                                .is_some_and(|terminal| terminal.instance_id() == slot.attempt)
                    })
            });
        if housekeeping || inventory_changed {
            self.inventory(app, journal, owner, now)?;
            self.last_housekeeping = Some(Instant::now());
        }
        // Drain before poll_agents can drop a fast-exiting PTY. A retired slot
        // remains available for exact late evidence, never for another attempt.
        for _ in 0..256 {
            let Ok(event) = self.events_rx.try_recv() else {
                break;
            };
            self.event(app, journal, owner, event, now)?;
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| !pending.uncertain && pending.deadline <= Instant::now())
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.uncertain(
                journal,
                &id,
                "adapter result deadline elapsed; no replay",
                now,
            )?;
        }
        if housekeeping || inventory_changed {
            self.publish(app, journal, owner, epoch_ms()?)?;
        }
        // A bounded durable outbox head per target. The journal independently
        // rechecks sender grant, target epoch and the accepted thread/turn pin.
        for receipt in journal.ready_commands(owner, 8)? {
            let Some(claim) = journal.claim_effect(owner, &receipt.command_id, epoch_ms()?)? else {
                continue;
            };
            self.dispatch(app, journal, claim, receipt.accepted_at_ms, epoch_ms()?)?;
        }
        Ok(())
    }

    fn inventory(
        &mut self,
        app: &mut App,
        journal: &mut Journal,
        owner: &OwnerFence,
        now: i64,
    ) -> Result<()> {
        for (id, slot) in &mut self.links {
            if !app.agents.iter().any(|run| run.id == *id) {
                slot.close();
            }
        }
        for run in app
            .agents
            .iter()
            .filter(|run| !run.plain_process && !is_cloud_owned_agent(run))
        {
            if let Some(terminal) = &run.terminal {
                journal.observe_run_attempt(owner, &run.id, terminal.instance_id(), now)?;
            } else {
                match journal.run_epoch(&owner.project_id, &run.id) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.downcast_ref::<rusqlite::Error>(),
                            Some(rusqlite::Error::QueryReturnedNoRows)
                        ) =>
                    {
                        journal.register_run(owner, &run.id, now)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let mut notices = Vec::new();
        let home = signals::rudder_home();
        let peers = journal
            .owned_runs(owner)?
            .into_iter()
            .map(|run| (run.run_id, run.run_epoch))
            .collect();
        for index in 0..app.agents.len() {
            let run = &app.agents[index];
            if run.plain_process || is_cloud_owned_agent(run) {
                continue;
            }
            let Some(terminal) = &run.terminal else {
                if let Some(slot) = self.links.get_mut(&run.id) {
                    slot.close();
                }
                let ids: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, pending)| pending.claim.target_run == run.id && !pending.uncertain)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    self.uncertain(
                        journal,
                        &id,
                        "PTY ownership ended before an exact adapter receipt",
                        now,
                    )?;
                }
                continue;
            };
            let attempt = terminal.instance_id().to_string();
            let epoch = journal.run_epoch(&owner.project_id, &run.id)?;
            let identity = if let Some(home) = &home {
                Some(issue_identity(
                    home, journal, owner, &run.id, epoch, &peers, now,
                )?)
            } else {
                None
            };
            let replaced = self
                .links
                .get(&run.id)
                .is_some_and(|slot| slot.attempt != attempt);
            if replaced {
                self.close(&run.id);
                let ids: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, pending)| pending.claim.target_run == run.id && !pending.uncertain)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    self.uncertain(journal, &id, "target PTY attempt was replaced", now)?;
                }
                self.links.remove(&run.id);
            }
            if !self.links.contains_key(&run.id) {
                let profile = terminal.execution_profile().unwrap_or("native").to_string();
                let prepared = (|| -> Result<Option<AgentLinkHandle>> {
                    let launch = terminal
                        .agent_link_launch_json()
                        .map(serde_json::from_str::<AgentLinkLaunch>)
                        .transpose()?;
                    if let Some(launch) = &launch {
                        anyhow::ensure!(
                            launch.run_id == run.id
                                && launch.attempt_id == attempt
                                && ((profile == "claude-sdk" && launch.backend == "claude-sdk")
                                    || (profile == "codex-app-server"
                                        && launch.backend == "codex")),
                            "agent link metadata does not match the owned PTY attempt/profile"
                        );
                    }
                    let link = launch
                        .map(|launch| {
                            agent_link::spawn_link(
                                launch,
                                &run.cwd,
                                run.session_id.clone(),
                                self.events_tx.clone(),
                            )
                        })
                        .transpose()?;
                    Ok(link)
                })();
                let (link, failed) = match prepared {
                    Ok(link) => (link, false),
                    Err(error) => {
                        notices.push(format!(
                            "{}: managed backend control unavailable: {error}",
                            run.id
                        ));
                        (None, true)
                    }
                };
                let slot = RuntimeLink {
                    attempt,
                    epoch,
                    profile,
                    backend: run.backend,
                    transport_uncertain: failed,
                    link,
                    broker: None,
                    broker_work: None,
                    projection_hash: None,
                    broker_failed_hash: None,
                    thread: None,
                    active: None,
                    interrupted: None,
                    phase: RuntimePhase::Starting,
                    qualified: false,
                    uncertain: failed,
                    closed: false,
                };
                self.links.insert(run.id.clone(), slot);
            }
            let slot = self.links.get_mut(&run.id).unwrap();
            if let (Some(home), Some(identity_path)) = (&home, identity) {
                let identity: AgentIdentityFile = read_private(&identity_path, 16 * 1024)?;
                let hash: [u8; 32] = Sha256::digest(serde_json::to_vec(&identity)?).into();
                if !slot.closed
                    && slot.broker_work.is_none()
                    && slot.projection_hash != Some(hash)
                    && slot.broker_failed_hash != Some(hash)
                {
                    let socket =
                        agent_broker::socket_path(home, &owner.project_id, &run.id, &slot.attempt);
                    let config = BrokerConfig {
                        journal_path: FabricPaths::new(home).journal,
                        identity_file: socket
                            .parent()
                            .context("broker endpoint parent")?
                            .join("identity.json"),
                        socket_path: socket,
                    };
                    let cancelled = Arc::new(AtomicBool::new(false));
                    slot.broker_work = Some(BrokerWork {
                        hash,
                        cancelled: Arc::clone(&cancelled),
                    });
                    let tx = self.broker_tx.clone();
                    let run_id = run.id.clone();
                    let attempt = slot.attempt.clone();
                    let run_epoch = slot.epoch;
                    let owner_epoch = owner.epoch;
                    let starting = slot.broker.is_none();
                    // Disk publication/SQLite opens/socket binding happen off
                    // the App actor. The result cannot adopt another attempt.
                    let spawn = std::thread::Builder::new()
                        .name("rudder-agent-broker".into())
                        .spawn(move || {
                            let result = (|| -> Result<Option<AgentBroker>> {
                                anyhow::ensure!(
                                    !cancelled.load(Ordering::Acquire),
                                    "broker launch was cancelled"
                                );
                                let unchanged = read_private::<AgentIdentityFile>(
                                    &config.identity_file,
                                    16 * 1024,
                                )
                                .ok()
                                .and_then(|existing| serde_json::to_vec(&existing).ok())
                                .is_some_and(|bytes| {
                                    <[u8; 32]>::from(Sha256::digest(bytes)) == hash
                                });
                                if !unchanged {
                                    agent_broker::project_identity(&config.socket_path, &identity)?;
                                }
                                anyhow::ensure!(
                                    !cancelled.load(Ordering::Acquire),
                                    "broker launch was cancelled"
                                );
                                let broker = if starting {
                                    Some(AgentBroker::spawn(config)?)
                                } else {
                                    None
                                };
                                if cancelled.load(Ordering::Acquire) {
                                    if let Some(broker) = &broker {
                                        broker.close();
                                    }
                                    anyhow::bail!("broker launch was cancelled");
                                }
                                Ok(broker)
                            })();
                            let _ = tx.send(BrokerResult {
                                run_id,
                                attempt,
                                run_epoch,
                                owner_epoch,
                                hash,
                                result,
                            });
                        });
                    if let Err(error) = spawn {
                        slot.broker_work = None;
                        slot.broker_failed_hash = Some(hash);
                        notices.push(format!("{}: agent tools could not start: {error}", run.id));
                    }
                }
            }

            // One universal callback covers every stop/drop/replacement path,
            // including paths that were added before the fabric extraction.
            let callback = slot.close_callback();
            if let Some(terminal) = app.agents[index].terminal.as_mut() {
                terminal.set_before_terminate(callback);
            }
        }
        for notice in notices {
            app.push_activity(notice);
        }
        Ok(())
    }

    fn broker_results(&mut self, app: &mut App, owner: &OwnerFence) {
        for _ in 0..64 {
            let Ok(result) = self.broker_rx.try_recv() else {
                break;
            };
            let Some(slot) = self.links.get_mut(&result.run_id).filter(|slot| {
                !slot.closed
                    && slot.attempt == result.attempt
                    && slot.epoch == result.run_epoch
                    && owner.epoch == result.owner_epoch
                    && slot
                        .broker_work
                        .as_ref()
                        .is_some_and(|work| work.hash == result.hash)
            }) else {
                continue;
            };
            let Some(run) = app.agents.iter_mut().find(|run| {
                run.id == result.run_id
                    && run
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| terminal.instance_id() == result.attempt)
            }) else {
                slot.close();
                continue;
            };
            slot.broker_work = None;
            match result.result {
                Ok(broker) => {
                    if let Some(broker) = broker {
                        slot.broker = Some(broker);
                    }
                    slot.projection_hash = Some(result.hash);
                    slot.broker_failed_hash = None;
                }
                Err(error) => {
                    slot.broker_failed_hash = Some(result.hash);
                    app.push_activity(format!(
                        "{}: agent tools unavailable: {error}",
                        result.run_id
                    ));
                    continue;
                }
            }
            if let Some(terminal) = run.terminal.as_mut() {
                terminal.set_before_terminate(slot.close_callback());
            }
        }
    }

    fn publish(
        &self,
        app: &App,
        journal: &mut Journal,
        owner: &OwnerFence,
        now: i64,
    ) -> Result<()> {
        let mut observations = Vec::new();
        for (id, slot) in &self.links {
            let Some(run) = app.agents.iter().find(|run| run.id == *id) else {
                if journal.run_epoch(&owner.project_id, id)? == slot.epoch {
                    observations.push(slot.observation(id, slot.backend, false));
                }
                continue;
            };
            // Do not publish the retired slot over a replacement PTY. The run
            // epoch check in SQLite independently rejects stale observations.
            if run
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.instance_id() != slot.attempt)
            {
                continue;
            }
            let owned = run
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.instance_id() == slot.attempt);
            if journal.run_epoch(&owner.project_id, id)? == slot.epoch {
                observations.push(slot.observation(id, run.backend, owned));
            }
        }
        journal.observe_runtimes(owner, &observations, now)?;
        Ok(())
    }

    fn event(
        &mut self,
        app: &mut App,
        journal: &mut Journal,
        owner: &OwnerFence,
        event: AgentLinkEvent,
        now: i64,
    ) -> Result<()> {
        if let AgentLinkEventKind::CommandResult {
            command_id,
            outcome,
        } = &event.kind
        {
            if self.pending.get(command_id).is_some_and(|pending| {
                pending.attempt == event.attempt_id && pending.claim.target_run == event.run_id
            }) {
                self.outcome(app, journal, owner, command_id, outcome, now)?;
            }
        }
        let Some(slot) = self
            .links
            .get_mut(&event.run_id)
            .filter(|slot| slot.attempt == event.attempt_id)
        else {
            return Ok(());
        };
        let mut lifecycle = None;
        let mut notice = None;
        match event.kind {
            AgentLinkEventKind::ProfileObserved {
                qualified,
                uncertain,
            } => {
                slot.qualified = qualified;
                slot.transport_uncertain = uncertain;
                slot.uncertain = uncertain
                    || self.pending.values().any(|pending| {
                        pending.uncertain
                            && pending.claim.target_run == event.run_id
                            && pending.attempt == event.attempt_id
                    });
                if uncertain {
                    notice = Some(
                        "backend profile negotiation is uncertain; controls are blocked"
                            .to_string(),
                    );
                }
            }
            AgentLinkEventKind::Bound {
                thread_id,
                active_turn_id,
                last_interrupted_turn_id,
                ..
            } => {
                if slot
                    .thread
                    .as_ref()
                    .is_some_and(|thread| thread != &thread_id)
                {
                    return Ok(());
                }
                slot.thread = Some(thread_id);
                slot.active = active_turn_id;
                slot.interrupted = last_interrupted_turn_id;
                slot.phase = if slot.active.is_some() {
                    RuntimePhase::Working
                } else if slot.interrupted.is_some() {
                    RuntimePhase::Interrupted
                } else {
                    RuntimePhase::Waiting
                };
                lifecycle = Some(slot.phase);
            }
            AgentLinkEventKind::TurnStarted { thread_id, turn_id } => {
                if slot.thread.as_deref() != Some(thread_id.as_str()) || slot.closed {
                    return Ok(());
                }
                slot.active = Some(turn_id);
                slot.interrupted = None;
                slot.phase = RuntimePhase::Working;
                lifecycle = Some(RuntimePhase::Working);
            }
            AgentLinkEventKind::TurnCompleted {
                thread_id,
                turn_id,
                status,
            } => {
                if slot.thread.as_deref() != Some(thread_id.as_str())
                    || slot.active.as_deref() != Some(turn_id.as_str())
                {
                    return Ok(());
                }
                slot.active = None;
                slot.phase = match status.as_str() {
                    "interrupted" => {
                        slot.interrupted = Some(turn_id);
                        RuntimePhase::Interrupted
                    }
                    "completed" => RuntimePhase::Completed,
                    "failed" => RuntimePhase::Failed,
                    "idle" if slot.profile == "claude-sdk" => RuntimePhase::Waiting,
                    _ => {
                        slot.uncertain = true;
                        slot.transport_uncertain = true;
                        RuntimePhase::Uncertain
                    }
                };
                lifecycle = Some(slot.phase);
            }
            AgentLinkEventKind::Closed { reason } => {
                slot.closed = true;
                if slot.active.is_some() {
                    slot.uncertain = true;
                    slot.transport_uncertain = true;
                }
                notice = Some(format!("backend control link closed: {reason}"));
                let ids: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, pending)| {
                        pending.claim.target_run == event.run_id
                            && pending.attempt == event.attempt_id
                            && !pending.uncertain
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    self.uncertain(
                        journal,
                        &id,
                        "backend transport closed before a result",
                        now,
                    )?;
                }
            }
            AgentLinkEventKind::CommandResult { .. } => {}
        }
        // Exact evidence can settle an old receipt, but may never revive a
        // stopped row or change the lifecycle of a newly launched attempt.
        if let Some(run) = app.agents.iter_mut().find(|run| run.id == event.run_id) {
            let same_attempt = run
                .terminal
                .as_ref()
                .is_none_or(|terminal| terminal.instance_id() == event.attempt_id);
            let terminal_status = matches!(
                run.status,
                AgentStatus::Stopped
                    | AgentStatus::Merged
                    | AgentStatus::Migrated
                    | AgentStatus::Orphaned
            );
            if same_attempt && !terminal_status {
                let slot = self.links.get(&event.run_id).unwrap();
                if let Some(thread) = &slot.thread {
                    run.session_id = Some(thread.clone());
                }
                if let Some(phase) = lifecycle.filter(|phase| {
                    run.terminal.is_some()
                        || !matches!(phase, RuntimePhase::Working | RuntimePhase::Waiting)
                }) {
                    apply_lifecycle(run, phase);
                    let _ = save_native_run_record(&app.cwd, run);
                    app.dirty = true;
                }
            }
        }
        if let Some(notice) = notice {
            app.push_activity(format!("{}: {notice}", event.run_id));
        }
        if let Some(run) = app.agents.iter().find(|run| run.id == event.run_id) {
            let slot = self.links.get(&event.run_id).unwrap();
            let owned = run
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.instance_id() == slot.attempt);
            if journal.run_epoch(&owner.project_id, &event.run_id)? == slot.epoch {
                journal.observe_runtime(
                    owner,
                    &slot.observation(&event.run_id, run.backend, owned),
                    now,
                )?;
            }
        }
        Ok(())
    }

    fn uncertain(
        &mut self,
        journal: &mut Journal,
        command_id: &str,
        reason: &str,
        now: i64,
    ) -> Result<()> {
        if let Some(pending) = self.pending.get_mut(command_id) {
            if !pending.uncertain {
                journal.mark_effect_uncertain(
                    &pending.claim,
                    &serde_json::json!({"reason":reason,"attempt_id":pending.attempt}),
                    now,
                )?;
                pending.uncertain = true;
            }
            if let Some(slot) = self
                .links
                .get_mut(&pending.claim.target_run)
                .filter(|slot| slot.attempt == pending.attempt)
            {
                slot.uncertain = true;
            }
        }
        Ok(())
    }

    fn outcome(
        &mut self,
        app: &mut App,
        journal: &mut Journal,
        owner: &OwnerFence,
        command_id: &str,
        outcome: &AgentLinkOutcome,
        now: i64,
    ) -> Result<()> {
        if let AgentLinkOutcome::Uncertain { reason } = outcome {
            return self.uncertain(journal, command_id, reason, now);
        }
        let Some(mut pending) = self.pending.get(command_id).cloned() else {
            return Ok(());
        };
        let result = serde_json::json!({"adapter":outcome,"attempt_id":pending.attempt,"run_id":pending.claim.target_run});
        if journal.run_epoch(&owner.project_id, &pending.claim.target_run)?
            != pending.claim.target_epoch
            && !pending.uncertain
        {
            journal.mark_effect_uncertain(&pending.claim, &serde_json::json!({"reason":"target was replaced before final receipt","attempt_id":pending.attempt}), now)?;
            pending.uncertain = true;
        }
        if pending.uncertain {
            journal.reconcile_effect(owner, command_id, &Reconciliation::Completed { result, evidence: serde_json::json!({"source":"exact_attempt_adapter_receipt","attempt_id":pending.attempt,"outcome":outcome}) }, now)?;
        } else {
            journal.finish_effect(&pending.claim, &result, now)?;
        }
        self.pending.remove(command_id);
        if let Some(slot) = self
            .links
            .get_mut(&pending.claim.target_run)
            .filter(|slot| slot.attempt == pending.attempt)
        {
            slot.uncertain = slot.transport_uncertain
                || self.pending.values().any(|other| {
                    other.uncertain
                        && other.claim.target_run == pending.claim.target_run
                        && other.attempt == pending.attempt
                });
            if let Some((status, completed_at)) = pending.reserved_status {
                if matches!(
                    outcome,
                    AgentLinkOutcome::Rejected { .. } | AgentLinkOutcome::Unsupported { .. }
                ) && slot.active.is_none()
                {
                    if let Some(run) = app.agents.iter_mut().find(|run| {
                        run.id == pending.claim.target_run
                            && run
                                .terminal
                                .as_ref()
                                .is_some_and(|terminal| terminal.instance_id() == pending.attempt)
                    }) {
                        if run.status == AgentStatus::Running {
                            run.status = status;
                            run.completed_at = completed_at;
                            let _ = save_native_run_record(&app.cwd, run);
                            app.dirty = true;
                        }
                    }
                }
            }
        }
        app.record_activity(format!(
            "{}: {} command {}",
            pending.claim.target_run,
            match outcome {
                AgentLinkOutcome::TransportAccepted { .. } => "backend accepted input for",
                AgentLinkOutcome::InterruptConfirmed { .. } => "confirmed interruption for",
                AgentLinkOutcome::Rejected { .. } => "rejected",
                AgentLinkOutcome::Unsupported { .. } => "does not support",
                AgentLinkOutcome::Uncertain { .. } => "has uncertain",
            },
            command_id
        ));
        Ok(())
    }

    fn dispatch(
        &mut self,
        app: &mut App,
        journal: &mut Journal,
        claim: EffectClaim,
        accepted_at: i64,
        now: i64,
    ) -> Result<()> {
        let index = app.agents.iter().position(|run| run.id == claim.target_run);
        let immediate = match (claim.kind.as_str(), index) {
            ("inspect", Some(index)) => {
                let run = &app.agents[index];
                Some(
                    serde_json::json!({"status":"observed","run_id":run.id,"run_epoch":claim.target_epoch,"lifecycle":run.status.as_str(),"backend":run.backend.as_str(),"task":run.task_summary}),
                )
            }
            ("cancel", Some(index)) => {
                let owned = app.agents[index].terminal.is_some();
                let accepted = app.stop_agent_at(index);
                Some(
                    serde_json::json!({"status":if accepted {"cancelled"} else {"rejected"},"run_id":claim.target_run,"pty_was_owned":owned,"process_tree_termination_requested":accepted && owned}),
                )
            }
            (_, None) => Some(
                serde_json::json!({"status":"rejected","reason":"target is no longer present in this engine"}),
            ),
            ("steer" | "interrupt" | "resume" | "deliver", _) => None,
            _ => Some(
                serde_json::json!({"status":"unsupported","reason":"unknown execution-profile operation"}),
            ),
        };
        if let Some(result) = immediate {
            journal.finish_effect(&claim, &result, now)?;
            return Ok(());
        }
        let index = index.unwrap();
        let run = &app.agents[index];
        let Some(slot) = self.links.get(&claim.target_run) else {
            journal.finish_effect(
                &claim,
                &serde_json::json!({"status":"unsupported","reason":"target has no managed link"}),
                now,
            )?;
            return Ok(());
        };
        let owned = run
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.instance_id() == slot.attempt);
        let capabilities = slot.observation(&run.id, run.backend, owned).capabilities;
        if slot.epoch != claim.target_epoch
            || !capabilities.contains(&claim.kind)
            || matches!(
                run.status,
                AgentStatus::Stopped
                    | AgentStatus::Merged
                    | AgentStatus::Migrated
                    | AgentStatus::Orphaned
            )
        {
            journal.finish_effect(&claim, &serde_json::json!({"status":"rejected","reason":"target attempt/profile does not currently permit this operation"}), now)?;
            return Ok(());
        }
        let Some(expected_thread) = claim
            .payload
            .get("expected_thread_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            journal.finish_effect(&claim, &serde_json::json!({"status":"rejected","reason":"accepted control has no immutable thread pin; no adapter write was attempted"}), now)?;
            return Ok(());
        };
        let expected_turn = claim
            .payload
            .get("expected_turn_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        // Never infer author or role from payload text. These fields were
        // authenticated and durably copied at command acceptance.
        let message = PeerMessage {
            sender_run_id: claim.sender_run.clone(),
            message_id: claim.command_id.clone(),
            text: serde_json::to_string(
                &serde_json::json!({"principal":claim.principal,"sender_epoch":claim.sender_epoch,"instruction":claim.payload.get("instruction"),"reason":claim.payload.get("reason")}),
            )?,
        };
        let operation = match claim.kind.as_str() {
            "steer" => AgentLinkOperation::Steer { message },
            "resume" => AgentLinkOperation::Resume { message },
            "deliver" => AgentLinkOperation::Deliver { message },
            _ => AgentLinkOperation::Interrupt,
        };
        let timeout_ms = claim
            .payload
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30_000)
            .min(120_000);
        let dispatch_now = epoch_ms()?;
        let remaining = accepted_at
            .saturating_add(timeout_ms as i64)
            .saturating_sub(dispatch_now);
        if remaining <= 0 {
            journal.finish_effect(&claim, &serde_json::json!({"status":"rejected","reason":"command expired before any adapter write"}), now)?;
            return Ok(());
        }
        let Some(link) = &slot.link else {
            journal.finish_effect(&claim, &serde_json::json!({"status":"unsupported","reason":"no qualified transport is available"}), now)?;
            return Ok(());
        };
        let attempt = slot.attempt.clone();
        let command = AgentLinkCommand {
            command_id: claim.command_id.clone(),
            run_id: claim.target_run.clone(),
            attempt_id: attempt.clone(),
            expected_thread_id: expected_thread,
            expected_turn_id: expected_turn,
            operation,
            timeout: Duration::from_millis(remaining as u64),
        };
        let reserved_status = if matches!(claim.kind.as_str(), "deliver" | "resume") {
            Some((run.status, run.completed_at))
        } else {
            None
        };
        if let Err(error) = link.submit(command) {
            journal.finish_effect(&claim, &serde_json::json!({"status":"rejected","reason":format!("adapter queue rejected before transport write: {error}")}), now)?;
            return Ok(());
        }
        if reserved_status.is_some() {
            let run = &mut app.agents[index];
            run.status = AgentStatus::Running;
            run.completed_at = None;
            let _ = save_native_run_record(&app.cwd, run);
            app.dirty = true;
        }
        self.pending.insert(
            claim.command_id.clone(),
            PendingControl {
                claim,
                attempt,
                deadline: Instant::now() + Duration::from_millis(remaining as u64),
                uncertain: false,
                reserved_status,
            },
        );
        Ok(())
    }
}

fn apply_lifecycle(run: &mut AgentRun, phase: RuntimePhase) {
    run.needs_permission = false;
    run.needs_user_input = false;
    run.wait_signal = None;
    match phase {
        RuntimePhase::Working => {
            run.status = AgentStatus::Running;
            run.completed_at = None;
            run.last_error = None;
        }
        RuntimePhase::Waiting => {
            run.status = AgentStatus::Running;
            run.completed_at = None;
            run.needs_user_input = true;
            run.wait_signal = Some(WaitSignal::Input);
        }
        RuntimePhase::Interrupted => {
            run.status = AgentStatus::Paused;
            run.completed_at = None;
        }
        RuntimePhase::Completed => {
            mark_run_done(run);
        }
        RuntimePhase::Failed => {
            run.status = AgentStatus::Failed;
            run.completed_at = None;
            run.last_error = Some("backend reported a failed turn".into());
        }
        _ => {}
    }
}

pub(crate) fn poll(app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
    let mut state = std::mem::take(&mut app.fabric_runtime);
    let result = state.poll(app, journal, owner);
    app.fabric_runtime = state;
    result
}

/// Call before releasing the owner epoch. Nothing here retries an effect.
pub(crate) fn shutdown(app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
    let mut state = std::mem::take(&mut app.fabric_runtime);
    let result = (|| -> Result<()> {
        let now = epoch_ms()?;
        for slot in state.links.values_mut() {
            slot.close();
        }
        let ids: Vec<_> = state.pending.keys().cloned().collect();
        for id in ids {
            state.uncertain(
                journal,
                &id,
                "engine gracefully released live transport ownership",
                now,
            )?;
        }
        state.publish(app, journal, owner, now)
    })();
    app.fabric_runtime = state;
    result
}

#[cfg(test)]
#[path = "fabric_runtime_tests.rs"]
mod tests;

fn epoch_ms() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantProjection {
    format_version: u32,
    owner_epoch: u64,
    grant: CapabilityGrant,
}

pub(crate) fn identity_path(home: &Path, owner: &OwnerFence, run_id: &str) -> PathBuf {
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{}", owner.project_id, run_id).as_bytes())
    );
    FabricPaths::new(home)
        .root
        .join("identities")
        .join(format!("{digest}.json"))
}

fn read_private<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "invalid engine identity projection"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "engine identity projection is too large"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("engine credential directory")?;
    fs::create_dir_all(parent)?;
    anyhow::ensure!(
        fs::symlink_metadata(parent)?.is_dir(),
        "engine credential directory is a symlink"
    );
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let tmp = parent.join(format!(".identity-{}.tmp", uuid::Uuid::new_v4()));
    let result: Result<()> = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn issue_identity(
    home: &Path,
    journal: &mut Journal,
    owner: &OwnerFence,
    run_id: &str,
    run_epoch: u64,
    recipients: &BTreeMap<String, u64>,
    now: i64,
) -> Result<PathBuf> {
    let path = identity_path(home, owner, run_id);
    let projection_path = path.with_extension("grant.json");
    let old_identity = read_private::<AgentIdentityFile>(&path, 16 * 1024).ok();
    let old_grant = read_private::<GrantProjection>(&projection_path, 1024 * 1024).ok();
    let operations = [
        "agents:find",
        "message:send",
        "mailbox:read",
        "mailbox:ack",
        "command:inspect",
        "command:steer",
        "command:interrupt",
        "command:resume",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<BTreeSet<_>>();
    let grant = CapabilityGrant {
        principal: format!("agent:{run_id}"),
        run_id: run_id.into(),
        run_epoch,
        operations,
        recipients: recipients.clone(),
        scopes: BTreeSet::from(["project".into()]),
        topics: BTreeSet::new(),
        expires_at_ms: Some(
            now.checked_add(24 * 60 * 60 * 1000)
                .context("identity expiry overflow")?,
        ),
    };
    if let (Some(identity), Some(previous)) = (&old_identity, &old_grant) {
        let mut expected = grant.clone();
        expected.expires_at_ms = previous.grant.expires_at_ms;
        if previous.format_version == 1
            && previous.owner_epoch == owner.epoch
            && previous.grant == expected
            && identity.project_id == owner.project_id
            && identity.run_id == run_id
            && identity.run_epoch == run_epoch
            && identity.owner_epoch == owner.epoch
            && previous
                .grant
                .expires_at_ms
                .is_some_and(|at| at > now + 60_000)
            && journal
                .authenticate_agent(&identity.token, now)
                .is_ok_and(|auth| {
                    auth.project_id == owner.project_id
                        && auth.run_id == run_id
                        && auth.run_epoch == run_epoch
                        && auth.owner_epoch == owner.epoch
                })
        {
            return Ok(path);
        }
    }
    let token = journal.issue_capability(owner, &grant, now)?;
    // Revoke the old credential before publishing the new one. A file failure
    // fails closed and the next actor heartbeat remints instead of broadening
    // a stale process's authority.
    if let Some(previous) = old_identity.filter(|old| old.project_id == owner.project_id) {
        journal.revoke_capability(owner, &previous.token, now)?;
    }
    let identity = AgentIdentityFile {
        format_version: 1,
        project_id: owner.project_id.clone(),
        run_id: run_id.into(),
        run_epoch,
        owner_epoch: owner.epoch,
        token,
    };
    write_private(&path, &identity)?;
    write_private(
        &projection_path,
        &GrantProjection {
            format_version: 1,
            owner_epoch: owner.epoch,
            grant,
        },
    )?;
    Ok(path)
}

/// Called before process spawn so MCP binds the SAME attempt the PTY records.
/// Only this process's current fabric owner may mint these launch credentials.
pub(crate) fn prepare_agent_tools(
    command: &mut TerminalCommand,
    backend: Backend,
    run_id: &str,
) -> Result<()> {
    let repo = env::current_dir()?.canonicalize()?;
    let record_path = repo.join(".rudder/engine/owner.json");
    let record = match read_private::<serde_json::Value>(&record_path, 16 * 1024) {
        Ok(record) => record,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    if record["authority"] != "fabric" || record["pid"].as_u64() != Some(std::process::id() as u64)
    {
        return Ok(());
    }
    anyhow::ensure!(
        record["format_version"] == 1,
        "unsupported engine ownership projection"
    );
    let owner: OwnerFence = serde_json::from_value(record["owner"].clone())?;
    let home = signals::rudder_home().context("Rudder home unavailable for agent tools")?;
    let paths = FabricPaths::new(&home);
    let mut journal = Journal::open(&paths.journal)?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let epoch = journal.observe_run_attempt(&owner, run_id, &attempt, epoch_ms()?)?;
    let recipients = journal
        .owned_runs(&owner)?
        .into_iter()
        .map(|run| (run.run_id, run.run_epoch))
        .collect();
    let identity = issue_identity(
        &home,
        &mut journal,
        &owner,
        run_id,
        epoch,
        &recipients,
        epoch_ms()?,
    )?;
    let broker_socket = agent_broker::socket_path(&home, &owner.project_id, run_id, &attempt);
    let projected = agent_broker::project_identity(
        &broker_socket,
        &read_private::<AgentIdentityFile>(&identity, 16 * 1024)?,
    )?;
    let exe = env::current_exe()?;
    command
        .env
        .push(("RUDDER_FABRIC_ATTEMPT_ID".into(), attempt));
    command.env.push((
        "RUDDER_FABRIC_IDENTITY".into(),
        projected.display().to_string(),
    ));
    command.env.push((
        "RUDDER_FABRIC_BROKER_SOCKET".into(),
        broker_socket.display().to_string(),
    ));
    let arguments = vec![
        "fabric".to_string(),
        "mcp".into(),
        "--identity-file".into(),
        projected.display().to_string(),
        "--broker-socket".into(),
        broker_socket.display().to_string(),
    ];
    match backend {
        Backend::Claude => {
            let config = identity.with_extension("mcp.json");
            write_private(
                &config,
                &serde_json::json!({"mcpServers":{"rudder_agents":{"type":"stdio","command":exe,"args":arguments}}}),
            )?;
            command
                .args
                .extend(["--mcp-config".into(), config.display().to_string()]);
        }
        Backend::Codex => {
            let configs = [
                format!(
                    "mcp_servers.rudder_agents.command={}",
                    serde_json::to_string(&exe.display().to_string())?
                ),
                format!(
                    "mcp_servers.rudder_agents.args={}",
                    serde_json::to_string(&arguments)?
                ),
                "mcp_servers.rudder_agents.enabled=true".into(),
            ];
            command.args.splice(
                0..0,
                configs
                    .into_iter()
                    .flat_map(|config| ["-c".to_string(), config]),
            );
        }
        Backend::Opencode => {}
    }
    Ok(())
}
