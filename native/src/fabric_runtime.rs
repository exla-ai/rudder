//! Durable commands enter the existing App actor here. Only this thread owns
//! App/PTys; the journal records intent before any external effect starts.

use super::*;
use crate::agent_broker::{self, AgentBroker, BrokerConfig};
use crate::agent_link::{
    self, AgentLinkCommand, AgentLinkEvent, AgentLinkEventKind, AgentLinkHandle, AgentLinkLaunch,
    AgentLinkOperation, AgentLinkOutcome, AgentSteerMode, PeerMessage,
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
#[cfg(target_os = "linux")]
use rudder_engine::execution::handoff::{OwnedWorkerJoin, WorkerJoined};

#[path="fabric_mailbox_delivery.rs"]
mod mailbox_delivery;
#[cfg(test)]
pub(crate) fn automatic_mailbox_message_for_test(claim: &EffectClaim) -> Result<Option<PeerMessage>> {
    mailbox_delivery::message(claim)
}

/// Transport threads produce evidence. This state belongs exclusively to the
/// App actor; it neither owns another scheduler nor mutates App from a thread.
pub(crate) struct RuntimeState {
    events_tx: Sender<AgentLinkEvent>,
    events_rx: Receiver<AgentLinkEvent>,
    broker_tx: Sender<BrokerResult>,
    broker_rx: Receiver<BrokerResult>,
    links: BTreeMap<String, RuntimeLink>,
    /// At most one actual broker-work owner per managed slot, including its
    /// already-received result and joined proof until replacement/extraction.
    #[cfg(target_os = "linux")]
    managed_work: BTreeMap<String, ManagedBrokerWork>,
    #[cfg(target_os = "linux")]
    managed_projections: BTreeMap<String, SharedAgentToolsProjectionOwner>,
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
    steer_mode: Option<AgentSteerMode>,
    phase: RuntimePhase,
    qualified: bool,
    uncertain: bool,
    closed: bool,
    draining: bool,
    workspace: Option<crate::managed_launch::WorkspaceAssignment>,
    managed_home: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    owned_backend: Option<Arc<rudder_engine::execution::handoff::BackendProcessWitness>>,
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

#[cfg(target_os = "linux")]
struct ManagedBrokerWork {
    assignment: crate::managed_launch::WorkspaceAssignment,
    hash: [u8; 32],
    cancelled: Arc<AtomicBool>,
    worker: OwnedWorkerJoin,
    receiver: Receiver<ManagedBrokerResult>,
    result: Option<ManagedBrokerResult>,
    joined: Option<WorkerJoined>,
    completed: bool,
    projection: Option<SharedAgentToolsProjectionOwner>,
}
#[cfg(target_os = "linux")]
struct ManagedBrokerResult {
    run_id: String, attempt: String, run_epoch: u64, owner_epoch: u64, hash: [u8;32],
    result: Result<Option<AgentToolsProjection>>,
}
#[cfg(target_os = "linux")]
impl ManagedBrokerWork {
    fn receive(&mut self) {
        if !self.completed && self.result.is_none() {
            if let Ok(result) = self.receiver.try_recv() { self.result = Some(result); }
        }
    }
    fn check_result(&self) -> Result<()> {
        let result = self.result.as_ref().context("managed broker result is not retained")?;
        let fence = &self.assignment.fence;
        anyhow::ensure!(result.run_id == fence.run_id && result.attempt == fence.attempt_id
            && result.run_epoch == fence.run_epoch && result.owner_epoch == fence.owner.epoch && result.hash == self.hash,
            "managed broker result differs from its actual worker assignment");
        if let Ok(Some(projection)) = &result.result {
            let socket = agent_broker::socket_path(&self.assignment_home()?, &fence.owner.project_id,&fence.run_id,&fence.attempt_id);
            anyhow::ensure!(projection.broker_socket == socket && projection.identity_file == socket.parent().unwrap().join("identity.json"),
                "managed projection result changed its owned endpoint");
        }
        Ok(())
    }
    fn assignment_home(&self) -> Result<PathBuf> {
        // Filled from the actual retained runtime projection alias at queueing,
        // never inferred from a saved path or a result's proposed endpoint.
        let projection = self.projection.as_ref().context("managed projection owner is not retained")?;
        anyhow::ensure!(projection.assignment() == &self.assignment, "managed projection owner changed assignment");
        Ok(projection.publication.home.clone())
    }
    fn observe_joined(&mut self) -> Result<bool> {
        self.receive();
        if self.joined.is_none() { self.joined = self.worker.try_joined()?; }
        Ok(self.joined.is_some())
    }
    fn finish(&mut self) -> Result<ManagedBrokerResult> {
        anyhow::ensure!(!self.completed && self.joined.is_some(), "managed broker thread has not actually joined");
        self.check_result()?;
        let result = self.result.take().unwrap(); self.completed = true; Ok(result)
    }
}

/// Actual App-to-worker owner transfer, never reconstructed from an observation.
/// Retain outside catch_unwind through retry/timeout and dispose OFF App only.
#[cfg(target_os = "linux")]
#[must_use = "retain actual runtime owners through explicit off-App join"]
pub(crate) struct ManagedRuntimeDrain {
    assignment: crate::managed_launch::WorkspaceAssignment,
    slot: RuntimeLink,
    work: Option<ManagedBrokerWork>,
    _projection: Option<SharedAgentToolsProjectionOwner>,
    link_joined: Option<crate::agent_link::AgentLinkWorkerJoined>,
    work_joined: Option<WorkerJoined>,
    settled: bool,
}
/// Actual runtime actor/setup-thread joins only. Broker server threads, managed
/// control worker and physical helper/backend exit remain independent facts.
#[cfg(target_os = "linux")]
pub(crate) struct ManagedRuntimeJoined {
    assignment: crate::managed_launch::WorkspaceAssignment,
    _link: Option<crate::agent_link::AgentLinkWorkerJoined>,
    _work: Option<WorkerJoined>,
}
#[cfg(target_os = "linux")]
impl ManagedRuntimeJoined {
    pub(crate) fn assignment(&self) -> &crate::managed_launch::WorkspaceAssignment { &self.assignment }
}
#[cfg(target_os = "linux")]
impl ManagedRuntimeDrain {
    pub(crate) fn assignment(&self) -> &crate::managed_launch::WorkspaceAssignment { &self.assignment }
    pub(crate) fn await_closed(&mut self, timeout: Duration) -> Result<ManagedRuntimeJoined> {
        anyhow::ensure!(!self.settled, "managed runtime joins already returned");
        anyhow::ensure!(!timeout.is_zero(), "managed runtime join deadline expired; carrier retained");
        let end = Instant::now().checked_add(timeout).context("managed runtime join timeout overflow")?;
        self.slot.close(); // OFF App: broker close may lock/shutdown owned sockets.
        if let Some(work) = &mut self.work {
            work.cancelled.store(true, Ordering::Release);
            if self.work_joined.is_none() {
                if work.joined.is_none() {
                    work.worker.start_join_observer()?;
                    work.joined = Some(work.worker.await_joined(end.saturating_duration_since(Instant::now()))?);
                }
                if !work.completed { work.receive(); work.finish()?; } // Discard outcome OFF App, never replay.
                self.work_joined = work.joined.take();
            }
        }
        if self.link_joined.is_none() {
            if let Some(link) = &mut self.slot.link {
                self.link_joined = Some(link.await_closed(end.saturating_duration_since(Instant::now()))?);
            }
        }
        let assignment = self.assignment.clone();
        anyhow::ensure!(Instant::now() < end, "managed runtime join deadline expired; carrier retained");
        self.settled = true;
        Ok(ManagedRuntimeJoined { assignment, _link: self.link_joined.take(), _work: self.work_joined.take() })
    }
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
            #[cfg(target_os = "linux")]
            managed_work: BTreeMap::new(),
            #[cfg(target_os = "linux")]
            managed_projections: BTreeMap::new(),
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
        if owned && !self.closed && !self.draining && !self.uncertain && self.thread.is_some() {
            if self.qualified {
                match self.phase {
                    RuntimePhase::Working => {
                        capabilities.insert("interrupt".into());
                        if self.steer_mode == Some(AgentSteerMode::ActiveAppend) {
                            capabilities.insert("steer".into());
                        }
                    }
                    // Interrupted is a turn observation, not tool/child settlement.
                    RuntimePhase::Interrupted => {}
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
        // Managed App callbacks fence admission only. Actual broker stop can
        // lock/shutdown sockets and belongs to the retained off-App drain.
        let broker = if self.workspace.is_none() { self.broker.as_ref().map(AgentBroker::close_callback) } else { self.broker.as_ref().map(AgentBroker::admission_fence_callback) };
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
    /// App-pure extraction. No IO/join/wait or path/PID lookup; a mismatch leaves
    /// every actual owner in this state. The caller keeps this carrier OFF App.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_managed_drain(&mut self,
        assignment: &crate::managed_launch::WorkspaceAssignment) -> Result<ManagedRuntimeDrain> {
        assignment.validate(&assignment.fence)?;
        let fence = &assignment.fence;
        let slot = self.links.get(&fence.run_id).context("managed runtime is absent")?;
        anyhow::ensure!(slot.attempt == fence.attempt_id && slot.epoch == fence.run_epoch
            && slot.workspace.as_ref() == Some(assignment), "managed drain belongs to another full assignment");
        if let Some(work) = self.managed_work.get(&fence.run_id) {
            anyhow::ensure!(work.assignment == *assignment && slot.broker_work.as_ref()
                .is_some_and(|pending| pending.hash == work.hash && Arc::ptr_eq(&pending.cancelled, &work.cancelled)),
                "managed broker work differs from its runtime owner");
        } else {
            anyhow::ensure!(slot.broker_work.is_none(), "managed broker work has no retained join owner");
        }
        if let Some(projection) = self.managed_projections.get(&fence.run_id) {
            anyhow::ensure!(projection.assignment() == assignment, "managed projection owner differs from drain assignment");
            if let Some(work_projection)=self.managed_work.get(&fence.run_id).and_then(|work|work.projection.as_ref()) {
                anyhow::ensure!(Arc::ptr_eq(&projection.inner,&work_projection.inner) && Arc::ptr_eq(&projection.host,&work_projection.host),
                    "managed worker changed its actual projection/writer owner");
            }
        }
        let assignment = assignment.clone(); // All fallible checks precede moving actual owners.
        let mut slot = self.links.remove(&fence.run_id).unwrap();
        slot.closed = true; slot.draining = true;
        if let Some(link) = &slot.link { link.close(); } // Atomic fence only, never a join.
        if let Some(work) = &slot.broker_work { work.cancelled.store(true, Ordering::Release); }
        let work = self.managed_work.remove(&fence.run_id);
        let projection = self.managed_projections.remove(&fence.run_id);
        Ok(ManagedRuntimeDrain { assignment, slot, work, _projection: projection, link_joined: None, work_joined: None, settled: false })
    }

    /// App-only adoption, with no IO. Background preparation already persisted
    /// the exact assignment/grant and started this broker before helper spawn.
    pub(crate) fn adopt_managed_broker(&mut self, owner: &OwnerFence,
        assignment: &crate::managed_launch::WorkspaceAssignment,
        home: &Path, terminal: &mut TerminalPane, broker: AgentBroker, projection_hash: [u8; 32]) -> Result<()> {
        assignment.validate(&assignment.fence)?;
        let fence = &assignment.fence;
        anyhow::ensure!(*owner == fence.owner && terminal.instance_id() == fence.attempt_id,
            "managed broker result no longer owns this PTY/fence");
        let profile = terminal.execution_profile().context("managed PTY has no explicit profile")?;
        let backend = match profile { "codex-app-server" => Backend::Codex, "claude-sdk" => Backend::Claude,
            _ => anyhow::bail!("managed PTY profile is unsupported") };
        let binding = broker.binding();
        anyhow::ensure!(binding.project_id == owner.project_id && binding.owner_epoch == owner.epoch
            && binding.run_id == fence.run_id && binding.run_epoch == fence.run_epoch
            && broker.socket_path() == agent_broker::socket_path(home, &owner.project_id, &fence.run_id, &fence.attempt_id),
            "prepared broker belongs to another managed run/owner");
        anyhow::ensure!(self.links.get(&fence.run_id).is_none_or(|old|old.closed && old.workspace.is_none()),
            "another runtime slot still owns the managed run; take its exact drain first");
        #[cfg(target_os = "linux")]
        anyhow::ensure!(!self.managed_work.contains_key(&fence.run_id), "managed broker work still owns this run");
        #[cfg(target_os = "linux")]
        anyhow::ensure!(!self.managed_projections.contains_key(&fence.run_id), "managed projection still owns this run");
        let slot = RuntimeLink { attempt: fence.attempt_id.clone(), epoch: fence.run_epoch,
            profile: profile.into(), backend, transport_uncertain: false, link: None,
            broker: Some(broker), broker_work: None, projection_hash: Some(projection_hash), broker_failed_hash: None,
            thread: None, active: None, interrupted: None, steer_mode: None, phase: RuntimePhase::Starting,
            qualified: false, uncertain: false, closed: false, draining: false, workspace: Some(assignment.clone()), managed_home: Some(home.into()),
            #[cfg(target_os = "linux")]
            owned_backend: None };
        terminal.set_before_terminate(slot.close_callback());
        self.links.insert(fence.run_id.clone(), slot);
        Ok(())
    }

    /// New task path: App-pure validation/transfer. Every Err leaves the actual
    /// broker in its original caller-owned slot and retains the shared issuer.
    #[cfg(target_os = "linux")]
    pub(crate) fn adopt_managed_broker_retaining(&mut self, owner: &OwnerFence,
        assignment: &crate::managed_launch::WorkspaceAssignment, home: &Path, terminal: &mut TerminalPane,
        broker: &mut Option<AgentBroker>, projection_hash: [u8;32], projection: &SharedAgentToolsProjectionOwner) -> Result<()> {
        assignment.validate(&assignment.fence)?; let fence=&assignment.fence;
        anyhow::ensure!(*owner==fence.owner && terminal.instance_id()==fence.attempt_id
            && projection.assignment()==assignment && projection.publication.home==home && projection.host.owner()==owner,
            "retained broker result changed exact runtime/projection owner");
        let snapshot=projection.snapshot()?.context("retained projection is busy or retired")?;
        anyhow::ensure!(!snapshot.pending && snapshot.hash==projection_hash, "retained projection publication is not acknowledged");
        let profile=terminal.execution_profile().context("managed PTY has no explicit profile")?;
        let backend=match profile { "codex-app-server"=>Backend::Codex,"claude-sdk"=>Backend::Claude,_=>anyhow::bail!("unsupported managed profile") };
        anyhow::ensure!(backend==projection.publication.backend,"retained projection backend differs");
        let actual=broker.as_ref().context("actual prepared broker absent")?; let binding=actual.binding();
        anyhow::ensure!(binding.project_id==owner.project_id && binding.owner_epoch==owner.epoch
            && binding.run_id==fence.run_id && binding.run_epoch==fence.run_epoch
            && actual.socket_path()==agent_broker::socket_path(home,&owner.project_id,&fence.run_id,&fence.attempt_id),"retained broker binding differs");
        anyhow::ensure!(self.links.get(&fence.run_id).is_none_or(|old|old.closed&&old.workspace.is_none())
            && !self.managed_work.contains_key(&fence.run_id)&&!self.managed_projections.contains_key(&fence.run_id),
            "previous managed runtime must be taken before replacement");
        let slot=RuntimeLink { attempt:fence.attempt_id.clone(),epoch:fence.run_epoch,profile:profile.into(),backend,
            transport_uncertain:false,link:None,broker:broker.take(),broker_work:None,projection_hash:Some(projection_hash),broker_failed_hash:None,
            thread:None,active:None,interrupted:None,steer_mode:None,phase:RuntimePhase::Starting,qualified:false,uncertain:false,
            closed:false,draining:false,workspace:Some(assignment.clone()),managed_home:Some(home.into()),owned_backend:None };
        terminal.set_before_terminate(slot.close_callback());
        self.managed_projections.insert(fence.run_id.clone(),projection.clone()); self.links.insert(fence.run_id.clone(),slot); Ok(())
    }

    /// App-only attachment of evidence from its owned control handle. A saved
    /// launch JSON, guest cwd or socket file cannot construct this witness.
    #[cfg(target_os = "linux")]
    pub(crate) fn attach_managed_backend(&mut self, owner: &OwnerFence,
        assignment: &crate::managed_launch::WorkspaceAssignment, terminal: &mut TerminalPane,
        launch: AgentLinkLaunch, guest_cwd: PathBuf, expected_thread: Option<String>,
        process: Arc<rudder_engine::execution::handoff::BackendProcessWitness>) -> Result<()> {
        assignment.validate(&assignment.fence)?;
        let fence = &assignment.fence;
        anyhow::ensure!(*owner == fence.owner && terminal.instance_id() == fence.attempt_id,
            "managed backend readiness arrived for a retired PTY/owner");
        let slot = self.links.get_mut(&fence.run_id).context("managed broker was not adopted before backend readiness")?;
        anyhow::ensure!(!slot.closed && !slot.draining && !slot.uncertain && slot.epoch == fence.run_epoch
            && slot.attempt == fence.attempt_id && slot.workspace.as_ref() == Some(assignment)
            && terminal.execution_profile() == Some(slot.profile.as_str())
            && launch.run_id == fence.run_id && launch.attempt_id == fence.attempt_id,
            "managed backend readiness belongs to another live assignment");
        if let Some(owned) = &slot.owned_backend {
            anyhow::ensure!(Arc::ptr_eq(owned, &process), "managed backend identity cannot be replaced");
            return Ok(());
        }
        let handle = agent_link::spawn_managed_link(launch, guest_cwd, expected_thread, self.events_tx.clone(), process.clone())?;
        slot.owned_backend = Some(process); slot.link = Some(handle);
        terminal.set_before_terminate(slot.close_callback());
        Ok(())
    }

    /// Stop admitting new backend controls without revoking/closing the broker
    /// authority that the helper still needs for final writer/VCS/RPC drain.
    pub(crate) fn begin_managed_shutdown(&mut self, owner: &OwnerFence,
        assignment: &crate::managed_launch::WorkspaceAssignment, terminal: &TerminalPane) -> Result<()> {
        assignment.validate(&assignment.fence)?;
        let fence = &assignment.fence;
        let slot = self.links.get_mut(&fence.run_id).context("managed runtime is absent")?;
        anyhow::ensure!(*owner == fence.owner && terminal.instance_id() == fence.attempt_id
            && slot.attempt == fence.attempt_id && slot.epoch == fence.run_epoch
            && slot.workspace.as_ref() == Some(assignment), "managed shutdown belongs to another owned runtime");
        slot.draining = true;
        if let Some(link) = &slot.link { link.close(); }
        Ok(())
    }

    pub(crate) fn close(&mut self, run_id: &str) {
        if let Some(link) = self.links.get_mut(run_id) {
            link.close();
        }
    }

    /// A late managed lifecycle result may retire only its exact old slot,
    /// even when another attempt has already reused the logical run id.
    pub(crate) fn close_managed_assignment(&mut self, assignment: &crate::managed_launch::WorkspaceAssignment) {
        if let Some(slot) = self.links.get_mut(&assignment.fence.run_id)
            .filter(|slot|slot.attempt == assignment.fence.attempt_id && slot.epoch == assignment.fence.run_epoch
                && slot.workspace.as_ref() == Some(assignment)) { slot.close(); }
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
        if housekeeping || inventory_changed {
            self.deliver_idle_mailboxes(app, journal, owner)?;
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
            let (run_home,workspace) = match (&run.managed_local,&run.managed_store) {
                (None,None) => (home.clone(),None),
                (Some(crate::managed_launch::ManagedRunSource::Assigned { .. }),Some(location)) => {
                    anyhow::ensure!(journal.database_path()?==location.store().binding().journal,
                        "managed inventory differs from its established journal identity");
                    let explicit_home=location.store().binding().home.clone();
                    anyhow::ensure!(self.links.get(&run.id).and_then(|slot|slot.managed_home.as_ref()).is_none_or(|home|home==&explicit_home),
                        "managed inventory changed its adopted identity home");
                    (Some(explicit_home),crate::managed_launch::assignment_for_location(location,owner,&run.id,epoch,&attempt)?)
                }
                _ => anyhow::bail!("managed inventory requires its exact assigned private location"),
            };
            if let Some(expected) = self.links.get(&run.id).and_then(|slot|slot.workspace.as_ref()) {
                anyhow::ensure!(workspace.as_ref() == Some(expected), "managed assignment disappeared or changed during identity rotation");
            }
            if let Some(source) = &run.managed_local {
                anyhow::ensure!(matches!(source, crate::managed_launch::ManagedRunSource::Assigned { assignment }
                    if workspace.as_ref() == Some(assignment.as_ref())), "managed PTY has no exact persisted workspace assignment");
            }
            // Only explicit actual-broker adoption creates a managed slot.
            // Extraction/retained exited panes must not recreate namespace work
            // from row DATA, or rotate identity after shutdown admission closes.
            if workspace.is_some() && self.links.get(&run.id).is_none_or(|slot| slot.closed || slot.draining) {
                continue;
            }
            #[cfg(target_os = "linux")]
            if let Some(assignment) = &workspace {
                self.queue_managed_refresh(assignment,&peers,now)?;
                let callback=self.links.get(&run.id).unwrap().close_callback();
                if let Some(terminal)=app.agents[index].terminal.as_mut() { terminal.set_before_terminate(callback); }
                continue; // Managed identity is issued/projected ONLY by its retained off-App owner.
            }
            let identity = if let Some(home) = &run_home {
                Some(issue_identity(
                    home, journal, owner, &run.id, epoch, &peers, now, workspace.as_ref(),
                )?)
            } else {
                None
            };
            let replaced = self
                .links
                .get(&run.id)
                .is_some_and(|slot| slot.attempt != attempt);
            if replaced {
                anyhow::ensure!(self.links.get(&run.id).is_none_or(|old|old.workspace.is_none()),
                    "managed runtime replacement requires its exact retained drain");
                #[cfg(target_os = "linux")]
                anyhow::ensure!(!self.managed_work.contains_key(&run.id), "managed broker work remains recoverable for old attempt");
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
                    // A managed assignment always waits for App adoption of an
                    // authenticated helper pidfd. Never downgrade to the legacy
                    // socket/serialized-launch path after timeout or restart.
                    if workspace.is_some() { return Ok(None); }
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
                    steer_mode: None,
                    phase: RuntimePhase::Starting,
                    qualified: false,
                    uncertain: failed,
                    closed: false,
                    draining: false,
                    managed_home: workspace.as_ref().and_then(|_|run_home.clone()), workspace,
                    #[cfg(target_os = "linux")]
                    owned_backend: None,
                };
                self.links.insert(run.id.clone(), slot);
            }
            let slot = self.links.get_mut(&run.id).unwrap();
            if let (Some(home), Some(identity_path)) = (&run_home, identity) {
                let identity: AgentIdentityFile = read_private(&identity_path, 16 * 1024)?;
                let hash: [u8; 32] = Sha256::digest(serde_json::to_vec(&identity)?).into();
                if !slot.closed
                    && (!slot.draining || slot.workspace.is_none())
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
                            let result = BrokerResult {
                                run_id,
                                attempt,
                                run_epoch,
                                owner_epoch,
                                hash,
                                result,
                            };
                            let _ = tx.send(result);
                        });
                    match spawn {
                        Err(error) => {
                            slot.broker_work = None;
                            slot.broker_failed_hash = Some(hash);
                            notices.push(format!("{}: agent tools could not start: {error}", run.id));
                        }
                        Ok(thread) => { drop(thread); } // Legacy-only detached transport behavior.

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

    #[cfg(target_os = "linux")]
    fn queue_managed_refresh(&mut self, assignment: &crate::managed_launch::WorkspaceAssignment,
        recipients: &BTreeMap<String,u64>, issued_at: i64) -> Result<()> {
        let fence=&assignment.fence;
        let live=self.links.get(&fence.run_id).context("managed refresh runtime absent")?;
        // Do not mutate the two-entry broker directory while the helper is
        // still validating/mounting it. Actual backend attachment follows mount.
        if live.closed||live.draining||live.owned_backend.is_none()||live.link.is_none() { return Ok(()); }
        let projection=self.managed_projections.get(&fence.run_id).context("managed refresh requires its retained original projection owner")?.clone();
        anyhow::ensure!(projection.assignment()==assignment,"managed refresh projection assignment differs");
        let Some(snapshot)=projection.snapshot()? else { return Ok(()); }; // Busy/retired: no App wait or effects.
        if snapshot.pending { return Ok(()); } // Never retry an uncertain publication or issue a third token.
        if snapshot.recipients==*recipients && snapshot.expires.is_some_and(|at|at>issued_at.saturating_add(60_000)) { return Ok(()); }
        if let Some(previous)=self.managed_work.get(&fence.run_id) {
            anyhow::ensure!(previous.assignment==*assignment,"old managed worker remains recoverable for another assignment");
            if !previous.completed { return Ok(()); }
        }
        let bytes=serde_json::to_vec(&(snapshot.hash,recipients))?;
        anyhow::ensure!(bytes.len()<=64*1024,"managed refresh peer request exceeds bound");
        let hash: [u8;32]=Sha256::digest(bytes).into();
        let slot=self.links.get_mut(&fence.run_id).context("managed refresh runtime absent")?;
        anyhow::ensure!(slot.workspace.as_ref()==Some(assignment) && slot.broker.is_some()
            && slot.projection_hash==Some(snapshot.hash),"managed refresh has no exact acknowledged server/projection");
        if slot.closed||slot.draining||slot.broker_failed_hash==Some(hash) { return Ok(()); }
        let issued_at=epoch_ms()?; // Capture this new maintenance request before any queue/work.
        let budget=rudder_engine::execution::finalization_budget::FinalizationBudget::begin(Duration::from_secs(30))?; // Original request, before queue.
        let cancelled=Arc::new(AtomicBool::new(false));
        let (result_tx,result_rx)=std::sync::mpsc::sync_channel(1);
        let (owner_tx,owner_rx)=std::sync::mpsc::sync_channel::<OwnedWorkerJoin>(1);
        let worker_projection=projection.clone(); let worker_cancelled=cancelled.clone();
        let wanted=recipients.clone(); let binding=assignment.clone();
        let spawned=std::thread::Builder::new().name("rudder-managed-projection".into()).spawn(move || {
            let actual=owner_rx.recv();
            let result=(|| -> Result<Option<AgentToolsProjection>> {
                actual.as_ref().map_err(|_|anyhow::anyhow!("managed projection worker custody transfer failed"))?.start_join_observer()?;
                worker_projection.refresh(&wanted,issued_at,&budget,&worker_cancelled)
            })();
            let _=result_tx.send(ManagedBrokerResult {run_id:binding.fence.run_id,attempt:binding.fence.attempt_id,
                run_epoch:binding.fence.run_epoch,owner_epoch:binding.fence.owner.epoch,hash,result});
        });
        match spawned {
            Err(_) => { slot.broker_failed_hash=Some(hash); }
            Ok(thread) => {
                let worker=OwnedWorkerJoin::new(thread);
                // Replace only an actually joined completed owner; one retained entry per slot.
                self.managed_work.insert(fence.run_id.clone(),ManagedBrokerWork {assignment:assignment.clone(),hash,
                    cancelled:cancelled.clone(),worker:worker.clone(),receiver:result_rx,result:None,joined:None,completed:false,projection:Some(projection)});
                slot.broker_work=Some(BrokerWork {hash,cancelled});
                if owner_tx.try_send(worker).is_err() {
                    slot.closed=true;slot.uncertain=true;
                    if let Some(work)=&slot.broker_work { work.cancelled.store(true,Ordering::Release); }
                    if let Some(link)=&slot.link { link.close(); }
                }
            }
        }
        Ok(())
    }

    fn broker_results(&mut self, app: &mut App, owner: &OwnerFence) {
        #[cfg(target_os = "linux")]
        self.managed_broker_results(app, owner);
        for _ in 0..64 {
            let Ok(result) = self.broker_rx.try_recv() else {
                break;
            };
            #[cfg(target_os = "linux")]
            if self.links.get(&result.run_id).is_some_and(|slot| slot.workspace.is_some()) { continue; }
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

    #[cfg(target_os = "linux")]
    fn managed_broker_results(&mut self, app: &mut App, owner: &OwnerFence) {
        for (id, work) in &mut self.managed_work {
            if work.completed { continue; }
            let Some(slot) = self.links.get_mut(id) else {
                work.cancelled.store(true, Ordering::Release); continue; // Retain uncertain custody, never overwrite.
            };
            let same = slot.workspace.as_ref() == Some(&work.assignment)
                && slot.attempt == work.assignment.fence.attempt_id && slot.epoch == work.assignment.fence.run_epoch;
            if !same { work.cancelled.store(true, Ordering::Release); continue; }
            match work.observe_joined() {
                Ok(false) => continue,
                Ok(true) => {},
                Err(_) => {
                    slot.closed = true; slot.uncertain = true;
                    work.cancelled.store(true, Ordering::Release);
                    if let Some(link) = &slot.link { link.close(); }
                    continue; // Panic/uncertainty keeps result AND actual worker owner.
                }
            }
            if work.result.is_none() { continue; }
            if work.check_result().is_err() {
                slot.closed = true; slot.uncertain = true; work.cancelled.store(true, Ordering::Release); continue;
            }
            let live = !slot.closed && !slot.draining && *owner == work.assignment.fence.owner;
            let run = app.agents.iter_mut().find(|run| run.id == *id && run.terminal.as_ref()
                .is_some_and(|terminal| terminal.instance_id() == work.assignment.fence.attempt_id));
            let result = work.finish().unwrap(); // Keep the joined owner/proof until replacement or drain.
            if !live || run.is_none() { continue; }
            match result.result {
                Ok(Some(projection)) => { slot.projection_hash = Some(projection.projection_hash); slot.broker_failed_hash = None; }
                Ok(None) => { slot.broker_failed_hash = None; }
                Err(_) => {
                    slot.broker_failed_hash = Some(work.hash);
                    app.push_activity(format!("{id}: agent tools could not refresh"));
                    continue;
                }
            }
            if let Some(terminal) = run.unwrap().terminal.as_mut() { terminal.set_before_terminate(slot.close_callback()); }
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
                steer_mode,
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
                slot.steer_mode = Some(steer_mode);
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
            || !mailbox_delivery::still_allowed(slot, run, owned, &claim)
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
        let message = mailbox_delivery::message(&claim)?.unwrap_or(PeerMessage {
            sender_run_id: claim.sender_run.clone(),
            message_id: claim.command_id.clone(),
            text: serde_json::to_string(
                &serde_json::json!({"principal":claim.principal,"sender_epoch":claim.sender_epoch,"instruction":claim.payload.get("instruction"),"reason":claim.payload.get("reason")}),
            )?,
        });
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

#[cfg(all(test, target_os = "linux"))]
#[path = "fabric_managed_runtime_drain_tests.rs"]
mod managed_drain_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "fabric_managed_projection_owner_tests.rs"]
mod managed_projection_owner_tests;

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
    workspace: Option<&crate::managed_launch::WorkspaceAssignment>,
) -> Result<PathBuf> {
    let path = identity_path(home, owner, run_id);
    let projection_path = path.with_extension("grant.json");
    let old_identity = read_private::<AgentIdentityFile>(&path, 16 * 1024).ok();
    let old_grant = read_private::<GrantProjection>(&projection_path, 1024 * 1024).ok();
    let mut operations = [
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
    let mut scopes = BTreeSet::from(["project".into()]);
    if let Some(workspace) = workspace {
        workspace.validate(&workspace.fence)?;
        anyhow::ensure!(workspace.fence.owner == *owner && workspace.fence.run_id == run_id
            && workspace.fence.run_epoch == run_epoch, "workspace assignment belongs to another run/owner");
        workspace.fence.check_current(journal)?;
        operations.insert("workspace:observe".into());
        scopes.insert(rudder_engine::execution::workspace_scope(&workspace.observation)?);
    }
    let grant = CapabilityGrant {
        principal: format!("agent:{run_id}"),
        run_id: run_id.into(),
        run_epoch,
        operations,
        recipients: recipients.clone(),
        scopes,
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
        None,
    )?;
    let broker_socket = agent_broker::socket_path(&home, &owner.project_id, run_id, &attempt);
    let projected = agent_broker::project_identity(
        &broker_socket,
        &read_private::<AgentIdentityFile>(&identity, 16 * 1024)?,
    )?;
    configure_agent_tools(command, backend, &projected, &broker_socket, &identity, &attempt)?;
    Ok(())
}

pub(crate) struct AgentToolsProjection {
    pub identity_file: PathBuf,
    pub broker_socket: PathBuf,
    pub projection_hash: [u8; 32],
}

/// Empty custody first, then actual prepared issuance BEFORE its write. No
/// Debug/Clone/Serde/token constructor or projection-file recovery path.
#[cfg(target_os = "linux")]
#[must_use = "retain until the exact original issuance is absent/revoked"]
pub(crate) struct AgentToolsProjectionOwner {
    current: Option<rudder_engine::journal::PreparedCapabilityIssuance>,
    pending: Option<rudder_engine::journal::PreparedCapabilityIssuance>,
    assignment: Option<crate::managed_launch::WorkspaceAssignment>,
    started: bool,
    fenced: bool,
    publication: Option<ProjectionPublication>,
    shared: Option<SharedAgentToolsProjectionOwner>,
}
#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ProjectionPublication {
    home: PathBuf, backend: Backend, assignment: crate::managed_launch::WorkspaceAssignment,
    grant: CapabilityGrant, issued_at: i64, hash: [u8; 32],
}
#[cfg(target_os = "linux")]
struct RetainedProjection {
    issuance: rudder_engine::journal::PreparedCapabilityIssuance,
    grant: CapabilityGrant, issued_at: i64, hash: Option<[u8; 32]>,
}
#[cfg(target_os = "linux")]
struct SharedProjectionState { current: RetainedProjection, pending: Option<RetainedProjection> }
#[cfg(target_os = "linux")]
struct SharedProjectionInner {
    state: std::sync::Mutex<SharedProjectionState>,
    fenced: AtomicBool,
    settled: AtomicBool,
}
/// Aliases retain the same actual issuance owners AND actual supervisor writer
/// custody. No JSON/path/token constructor, and no App writer/mutex wait.
#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) struct SharedAgentToolsProjectionOwner {
    inner: Arc<SharedProjectionInner>,
    host: Arc<crate::managed_app::ManagedTaskJournalOwner>,
    publication: ProjectionPublication,
}
#[cfg(target_os = "linux")]
struct ProjectionSnapshot { recipients: BTreeMap<String,u64>, expires: Option<i64>, hash: [u8;32], pending: bool }
#[cfg(target_os = "linux")]
impl SharedAgentToolsProjectionOwner {
    pub(crate) fn assignment(&self) -> &crate::managed_launch::WorkspaceAssignment { &self.publication.assignment }
    /// Pure restriction on the SAME actual host/issuance, never a grant constructor.
    pub(crate) fn broker_admission(&self,host:&Arc<crate::managed_app::ManagedTaskJournalOwner>,assignment:&crate::managed_launch::WorkspaceAssignment)
        ->rudder_engine::journal::AgentAdmissionState {
        use rudder_engine::journal::AgentAdmissionState::*;
        if !Arc::ptr_eq(&self.host,host)||self.assignment()!=assignment||host.owner()!=&assignment.fence.owner
            ||self.inner.fenced.load(Ordering::Acquire)||self.inner.settled.load(Ordering::Acquire){return Fenced;}
        if let Err(error)=host.check_shared_admission(){return if error.is::<crate::engine_host::EngineSharedAdmissionBusy>(){Busy}else{Fenced};}
        // Host check precedes projection lock; callbacks never acquire a Journal.
        let state=match self.inner.state.try_lock(){Ok(state)=>state,Err(std::sync::TryLockError::WouldBlock)=>return Busy,Err(_)=>return Fenced};
        if state.pending.is_some(){return Busy;}
        if state.current.issuance.is_retired()||state.current.issuance.acknowledged_token().is_err()||state.current.hash.is_none(){return Fenced;}
        Ready
    }

    fn snapshot(&self) -> Result<Option<ProjectionSnapshot>> {
        if self.inner.fenced.load(Ordering::Acquire) { return Ok(None); }
        let state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(_) => anyhow::bail!("projection ownership is poisoned; retain recovery"),
        };
        Ok(Some(ProjectionSnapshot { recipients: state.current.grant.recipients.clone(),
            expires: state.current.grant.expires_at_ms, hash: state.current.hash.context("current projection ACK absent")?,
            pending: state.pending.is_some() }))
    }
    fn settle_unused(&self, journal: &mut Journal,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget) -> Result<()> {
        retire_projection_with_writer(&self.inner,journal,budget)
    }
    fn refresh(&self, recipients: &BTreeMap<String,u64>, issued_at: i64,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget, cancelled: &AtomicBool) -> Result<Option<AgentToolsProjection>> {
        // Global order is actual host writer BEFORE projection state. App never
        // takes either blocking mutex, opens a path writer or performs effects.
        self.host.with_writer(budget,cancelled,|journal| self.refresh_with_writer(journal,recipients,issued_at,budget,cancelled))
    }
    fn refresh_with_writer(&self, journal: &mut Journal, recipients: &BTreeMap<String,u64>, issued_at: i64,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget, cancelled: &AtomicBool) -> Result<Option<AgentToolsProjection>> {
        refresh_projection_with_writer(&self.publication,&self.inner,journal,recipients,issued_at,budget,cancelled)

    }
}
#[cfg(target_os = "linux")]
fn retire_projection_with_writer(inner: &SharedProjectionInner, journal: &mut Journal,
    budget: &rudder_engine::execution::finalization_budget::FinalizationBudget) -> Result<()> {
        inner.fenced.store(true,Ordering::Release);
        let mut state = inner.state.lock().unwrap_or_else(|poison| poison.into_inner());
        // Cleanup may recover a poisoned state only to retire actual originals;
        // poison can never enable another issuance/publication.
        if let Some(pending) = state.pending.as_mut() {
            if !pending.issuance.is_retired() { budget.remaining()?; journal.retire_prepared_capability(&mut pending.issuance,epoch_ms()?)?; }
        }
        if !state.current.issuance.is_retired() {
            budget.remaining()?; journal.retire_prepared_capability(&mut state.current.issuance,epoch_ms()?)?;
        }
        anyhow::ensure!(state.current.issuance.is_retired() && state.pending.as_ref().is_none_or(|p|p.issuance.is_retired()),
            "exact projection issuances are not retired");
        inner.settled.store(true,Ordering::Release); Ok(())
}
#[cfg(target_os = "linux")]
fn refresh_projection_with_writer(binding: &ProjectionPublication, inner: &SharedProjectionInner, journal: &mut Journal,
    recipients: &BTreeMap<String,u64>, issued_at: i64,
    budget: &rudder_engine::execution::finalization_budget::FinalizationBudget, cancelled: &AtomicBool) -> Result<Option<AgentToolsProjection>> {
        agent_tools_current(budget,cancelled)?;
        let mut state = inner.state.lock().map_err(|_|anyhow::anyhow!("projection ownership poisoned; retain recovery"))?;
        agent_tools_current(budget,cancelled)?;
        anyhow::ensure!(!inner.fenced.load(Ordering::Acquire) && state.pending.is_none(),
            "pending projection needs exact reconciliation before another refresh");
        let fence = &binding.assignment.fence;
        binding.assignment.validate(fence)?; fence.check_current(journal)?;
        anyhow::ensure!(journal.database_path()? == FabricPaths::new(&binding.home).journal
            && journal.observe_prepared_capability(&state.current.issuance)? == rudder_engine::journal::CapabilityIssuanceObservation::Present,
            "current projection issuance is not the exact live original");
        state.current.issuance.acknowledged_token()?; // Present alone cannot restore first ACK.
        let now = epoch_ms()?;
        anyhow::ensure!(issued_at >= state.current.issued_at && now >= issued_at, "projection refresh clock regressed");
        let actual: BTreeMap<_,_> = journal.owned_runs(&fence.owner)?.into_iter().map(|run|(run.run_id,run.run_epoch)).collect();
        anyhow::ensure!(actual == *recipients, "projection peer snapshot advanced before its original request");
        if state.current.grant.recipients == *recipients && state.current.grant.expires_at_ms.is_some_and(|at|at > now.saturating_add(60_000)) {
            return Ok(None);
        }
        let mut grant = state.current.grant.clone(); grant.recipients = recipients.clone();
        grant.expires_at_ms = Some(issued_at.checked_add(24*60*60*1000).context("projection expiry overflow")?);
        agent_tools_current(budget,cancelled)?;
        let issuance = journal.prepare_capability_issuance(&fence.owner,&grant,issued_at)?;
        state.pending = Some(RetainedProjection { issuance, grant: grant.clone(), issued_at, hash: None });
        agent_tools_current(budget,cancelled)?;
        anyhow::ensure!(!inner.fenced.load(Ordering::Acquire), "projection cleanup fenced issuance");
        journal.issue_prepared_capability(&mut state.pending.as_mut().unwrap().issuance,epoch_ms()?)?;
        agent_tools_current(budget,cancelled)?; fence.check_current(journal)?;
        anyhow::ensure!(!inner.fenced.load(Ordering::Acquire), "projection cleanup fenced refresh");
        let identity = AgentIdentityFile { format_version:1, project_id:fence.owner.project_id.clone(), run_id:fence.run_id.clone(),
            run_epoch:fence.run_epoch, owner_epoch:fence.owner.epoch,
            token:state.pending.as_ref().unwrap().issuance.acknowledged_token()?.to_owned() };
        let hash = Sha256::digest(serde_json::to_vec(&identity)?).into(); state.pending.as_mut().unwrap().hash=Some(hash);
        let path=identity_path(&binding.home,&fence.owner,&fence.run_id);
        write_private(&path,&identity)?;
        agent_tools_current(budget,cancelled)?; fence.check_current(journal)?;
        write_private(&path.with_extension("grant.json"),&GrantProjection {format_version:1,owner_epoch:fence.owner.epoch,grant})?;
        agent_tools_current(budget,cancelled)?; fence.check_current(journal)?;
        let socket=agent_broker::socket_path(&binding.home,&fence.owner.project_id,&fence.run_id,&fence.attempt_id);
        let projected=agent_broker::project_identity(&socket,&identity)?;
        agent_tools_current(budget,cancelled)?; fence.check_current(journal)?;
        anyhow::ensure!(!inner.fenced.load(Ordering::Acquire), "projection cleanup fenced publication");
        // Only original replacement projection ACK permits old exact retirement.
        journal.retire_prepared_capability(&mut state.current.issuance,epoch_ms()?)?;
        anyhow::ensure!(state.current.issuance.is_retired(), "old projection retirement ACK absent");
        agent_tools_current(budget,cancelled)?; fence.check_current(journal)?;
        state.current=state.pending.take().unwrap(); // Old slot was actually retired, never guessed from files.
        Ok(Some(AgentToolsProjection {identity_file:projected,broker_socket:socket,projection_hash:hash}))
}

#[cfg(target_os = "linux")]
impl AgentToolsProjectionOwner {
    pub(crate) fn new() -> Self {
        Self { current: None, pending: None, assignment: None, started: false, fenced: false, publication: None, shared: None }
    }
    pub(crate) fn is_settled(&self) -> bool {
        self.shared.as_ref().map_or_else(|| self.current.as_ref().is_none_or(|p| p.is_retired())
            && self.pending.as_ref().is_none_or(|p| p.is_retired()), |shared| shared.inner.settled.load(Ordering::Acquire))
    }
    /// OFF App, after initial successful publication only. Transfers original
    /// custody once; later aliases cannot replace its actual writer host.
    pub(crate) fn share(&mut self, host: Arc<crate::managed_app::ManagedTaskJournalOwner>) -> Result<SharedAgentToolsProjectionOwner> {
        anyhow::ensure!(!self.fenced, "projection owner already fenced for retirement");
        if let Some(shared) = &self.shared {
            anyhow::ensure!(Arc::ptr_eq(&shared.host,&host), "projection writer host cannot be replaced");
            anyhow::ensure!(!shared.inner.fenced.load(Ordering::Acquire), "shared projection owner retired");
            return Ok(shared.clone());
        }
        anyhow::ensure!(!self.fenced && self.pending.is_none(), "projection publication needs retained recovery");
        let publication = self.publication.as_ref().context("initial projection publication ACK absent")?.clone();
        anyhow::ensure!(host.owner() == &publication.assignment.fence.owner, "projection host owner differs");
        self.current.as_ref().context("original projection issuance absent")?.acknowledged_token()?;
        let current = RetainedProjection { issuance: self.current.take().unwrap(), grant: publication.grant.clone(),
            issued_at: publication.issued_at, hash: Some(publication.hash) };
        let shared = SharedAgentToolsProjectionOwner { inner: Arc::new(SharedProjectionInner {
            state: std::sync::Mutex::new(SharedProjectionState { current, pending: None }), fenced: AtomicBool::new(false), settled: AtomicBool::new(false) }),
            host, publication };
        self.shared = Some(shared.clone()); Ok(shared)
    }
    /// OFF App after the full actual helper/control/actor/broker hierarchy joins.
    /// Retire/retry only exact original owners; never reissue or read a token file.
    pub(crate) fn settle_unused(&mut self, journal: &mut Journal,
        budget: &rudder_engine::execution::finalization_budget::FinalizationBudget) -> Result<()> {
        self.fenced = true;
        if let Some(shared) = &self.shared { return shared.settle_unused(journal,budget); }
        if self.is_settled() { return Ok(()); } // Empty early failure has no effect/time renewal.
        for slot in [&mut self.pending, &mut self.current] {
            if let Some(prepared) = slot.as_mut().filter(|p| !p.is_retired()) {
                budget.remaining()?;
                let observed = journal.retire_prepared_capability(prepared, epoch_ms()?)?;
                anyhow::ensure!(matches!(observed, rudder_engine::journal::CapabilityIssuanceObservation::Absent
                    | rudder_engine::journal::CapabilityIssuanceObservation::Revoked) && prepared.is_retired(),
                    "original agent tools issuance retirement is not acknowledged");
                budget.remaining()?;
            }
        }
        Ok(())
    }
}
#[cfg(target_os = "linux")]
fn agent_tools_current(budget: &rudder_engine::execution::finalization_budget::FinalizationBudget,
    cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "agent tools preparation cancelled");
    budget.remaining()?; Ok(())
}

/// OFF App managed-only entry. Caller supplies its actual EngineSourceLease-
/// derived writer and checks lease.check_journal before/after. A borrowed owner
/// stays in the supervisor outside catch_unwind before every uncertain effect.
#[cfg(target_os = "linux")]
pub(crate) fn prepare_agent_tools_for_attempt_retaining(
    retained: &mut AgentToolsProjectionOwner, journal: &mut Journal, backend: Backend, home: &Path,
    fence: &crate::managed_launch::ManagedLaunchFence,
    workspace: Option<&crate::managed_launch::WorkspaceAssignment>,
    budget: &rudder_engine::execution::finalization_budget::FinalizationBudget, cancelled: &AtomicBool,
) -> Result<AgentToolsProjection> {
    prepare_agent_tools_with_location_retaining(retained,journal,backend,home,fence,workspace,None,budget,cancelled)
}

/// OFF App. New logical namespaces pass the SAME actual retained location;
/// neither a UUID/path nor assignment DATA can reopen/mint a fresh floor.
#[cfg(target_os = "linux")]
pub(crate) fn prepare_agent_tools_for_location_retaining(
    retained:&mut AgentToolsProjectionOwner,journal:&mut Journal,backend:Backend,home:&Path,
    fence:&crate::managed_launch::ManagedLaunchFence,workspace:Option<&crate::managed_launch::WorkspaceAssignment>,
    location:&crate::managed_store::ManagedRunLocation,
    budget:&rudder_engine::execution::finalization_budget::FinalizationBudget,cancelled:&AtomicBool,
)->Result<AgentToolsProjection>{prepare_agent_tools_with_location_retaining(retained,journal,backend,home,fence,workspace,Some(location),budget,cancelled)}

#[cfg(target_os = "linux")]
fn prepare_agent_tools_with_location_retaining(
    retained:&mut AgentToolsProjectionOwner,journal:&mut Journal,backend:Backend,home:&Path,
    fence:&crate::managed_launch::ManagedLaunchFence,workspace:Option<&crate::managed_launch::WorkspaceAssignment>,
    location:Option<&crate::managed_store::ManagedRunLocation>,
    budget:&rudder_engine::execution::finalization_budget::FinalizationBudget,cancelled:&AtomicBool,
)->Result<AgentToolsProjection>{
    anyhow::ensure!(!retained.started && !retained.fenced, "agent tools preparation already attempted; retire original issuance");
    retained.started = true;
    let issued_at = epoch_ms()?;
    agent_tools_current(budget, cancelled)?;
    anyhow::ensure!(matches!(backend, Backend::Codex | Backend::Claude), "managed agent tools backend unsupported");
    let workspace = workspace.context("retained managed agent tools require a complete workspace assignment")?;
    workspace.validate(fence)?; fence.check_current(journal)?;
    anyhow::ensure!(home.is_absolute() && journal.database_path()? == FabricPaths::new(home).journal,
        "agent tools writer differs from the explicitly held managed Journal");
    retained.assignment = Some(workspace.clone());
    if let Some(location)=location{
        anyhow::ensure!(location.run_id()==fence.run_id&&location.store().binding().home==home
            &&location.store().binding().project_id==fence.owner.project_id
            &&location.store().binding().journal==journal.database_path()?
            &&location.store().reopen_current(journal)?==*location.store(),"agent tools original managed location differs from held Journal/attempt");
        location.persist_assignment(workspace)?;
    }else{crate::managed_launch::persist_assignment(home, workspace)?;}
    agent_tools_current(budget, cancelled)?; fence.check_current(journal)?;
    let recipients = journal.owned_runs(&fence.owner)?.into_iter().map(|run| (run.run_id, run.run_epoch)).collect();
    let operations = ["agents:find", "message:send", "mailbox:read", "mailbox:ack", "command:inspect",
        "command:steer", "command:interrupt", "command:resume", "workspace:observe"]
        .into_iter().map(str::to_string).collect();
    let grant = CapabilityGrant { principal: format!("agent:{}", fence.run_id), run_id: fence.run_id.clone(),
        run_epoch: fence.run_epoch, operations, recipients,
        scopes: BTreeSet::from(["project".into(), rudder_engine::execution::workspace_scope(&workspace.observation)?]),
        topics: BTreeSet::new(), expires_at_ms: Some(issued_at.checked_add(24 * 60 * 60 * 1000).context("agent tools expiry overflow")?) };
    agent_tools_current(budget, cancelled)?;
    retained.pending = Some(journal.prepare_capability_issuance(&fence.owner, &grant, issued_at)?);
    // The original private token and held Journal anchor already belong to the
    // outer retained carrier if this write, its ACK, or later projection fails.
    agent_tools_current(budget, cancelled)?;
    journal.issue_prepared_capability(retained.pending.as_mut().unwrap(), epoch_ms()?)?;
    agent_tools_current(budget, cancelled)?; fence.check_current(journal)?;
    let identity = AgentIdentityFile { format_version: 1, project_id: fence.owner.project_id.clone(), run_id: fence.run_id.clone(),
        run_epoch: fence.run_epoch, owner_epoch: fence.owner.epoch,
        token: retained.pending.as_ref().unwrap().acknowledged_token()?.to_owned() };
    let identity_path = identity_path(home, &fence.owner, &fence.run_id);
    write_private(&identity_path, &identity)?;
    agent_tools_current(budget, cancelled)?; fence.check_current(journal)?;
    write_private(&identity_path.with_extension("grant.json"), &GrantProjection { format_version: 1, owner_epoch: fence.owner.epoch, grant: grant.clone() })?;
    agent_tools_current(budget, cancelled)?; fence.check_current(journal)?;
    let socket = agent_broker::socket_path(home, &fence.owner.project_id, &fence.run_id, &fence.attempt_id);
    let projection_hash = Sha256::digest(serde_json::to_vec(&identity)?).into();
    let projected = agent_broker::project_identity(&socket, &identity)?;
    agent_tools_current(budget, cancelled)?; fence.check_current(journal)?;
    retained.current = retained.pending.take(); // Original first ACK + all projection acknowledgments, never readback.
    retained.publication = Some(ProjectionPublication { home: home.into(), backend, assignment: workspace.clone(), grant, issued_at, hash: projection_hash });
    Ok(AgentToolsProjection { identity_file: projected, broker_socket: socket, projection_hash })
}

/// Blocking identity/config IO, called off-App after accepting the reserved
/// launch result. Never observes an attempt, reads cwd or chooses a profile.
pub(crate) fn prepare_agent_tools_for_attempt(
    command: &mut TerminalCommand,
    backend: Backend,
    home: &Path,
    fence: &crate::managed_launch::ManagedLaunchFence,
    workspace: Option<&crate::managed_launch::WorkspaceAssignment>,
) -> Result<AgentToolsProjection> {
    let mut journal = Journal::open(&FabricPaths::new(home).journal)?;
    fence.check_current(&journal)?;
    if let Some(workspace) = workspace {
        workspace.validate(fence)?;
        crate::managed_launch::persist_assignment(home, workspace)?;
    }
    let recipients = journal.owned_runs(&fence.owner)?.into_iter()
        .map(|run| (run.run_id, run.run_epoch)).collect();
    let identity = issue_identity(home, &mut journal, &fence.owner, &fence.run_id,
        fence.run_epoch, &recipients, epoch_ms()?, workspace)?;
    let socket = agent_broker::socket_path(home, &fence.owner.project_id, &fence.run_id, &fence.attempt_id);
    let identity_record = read_private::<AgentIdentityFile>(&identity, 16 * 1024)?;
    let projection_hash = Sha256::digest(serde_json::to_vec(&identity_record)?).into();
    let projected = agent_broker::project_identity(&socket, &identity_record)?;
    if workspace.is_none() {
        configure_agent_tools(command, backend, &projected, &socket, &identity, &fence.attempt_id)?;
    }
    Ok(AgentToolsProjection { identity_file: projected, broker_socket: socket, projection_hash })
}

fn configure_agent_tools(command: &mut TerminalCommand, backend: Backend,
    projected: &Path, broker_socket: &Path, identity: &Path, attempt: &str) -> Result<()> {
    let exe = env::current_exe()?;
    command
        .env
        .push(("RUDDER_FABRIC_ATTEMPT_ID".into(), attempt.into()));
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

#[cfg(all(test,target_os="linux"))]
#[path="fabric_retained_location_projection_tests.rs"]
mod retained_location_projection_tests;
