//! App ownership for explicitly admitted source work.
//! Startup and default routing remain separately gated.
use super::*;
use rudder_engine::journal::OwnerFence;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::{
        managed_launch::{ManagedLaunchFence, ManagedRunSource, RemoteSourceAssignment},
        managed_launch::source_prepare::{self, ManagedSourcePrepareRequest,
            ManagedSourcePrepareFailure, PreparedManagedSource},
        managed_store::ManagedRunLocation,
        source_issuer::HostSourceIssuer,
        source_shutdown_join::{ExitedSourceShutdown, JoinedSourceShutdown},
    };
    use rudder_engine::{
        execution::{
            finalization_budget::FinalizationBudget,
            handoff::{HandoffReceipt, ManagedExecutionEvent, ManagedExecutionHandle,
                ManagedExecutionHandoff, helper_process::HelperProcessWitness,
                source_shutdown::ProvisionalSourceShutdown},
            CheckpointRef, ExecutionRecord, ManagedFrontendStart, QuiescedPartialCheckpointV1,
            StopAndCheckpoint,
        },
        journal::Journal,
        vfs::remote_launch::RemoteLaunchRequest,
    };
    use rudder_native::pty_terminal::{ManagedHelperCustody, SuccessfulManagedHelperExit};
    use std::{collections::BTreeMap, sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender}}};

    const LIMIT: usize = 2;
    const STARTUP_LIMIT: Duration = Duration::from_secs(30);
    const EVENT_LIMIT: usize = 8;
    mod supervision { use super::*; include!("managed_source_supervisor.rs"); }
    pub(crate) use supervision::{SourceOwnerLifetimeSupervisor, SourceSupervision,
        SourceSupervisionKey, SourceSupervisorStatus, StoppedSourceSupervisor};

    /// Supplied by the actual owner. Called ONLY by the source worker. It opens
    /// a bounded writer for the held Journal; it never starts a scheduler and
    /// cannot derive ownership from serialized request paths.
    pub(crate) type SourceJournalFactory = Box<dyn FnOnce() -> Result<Journal> + Send>;

    /// DATA for a subsequent bounded publication worker. Only this module's
    /// successful fresh join + actual retirement creates it. Backend exit,
    /// joined checkpoint, DATA publication and history acceptance stay separate.
    pub(crate) struct JoinedSourceCheckpoint {
        pub assignment: RemoteSourceAssignment,
        pub managed_run: ManagedRunLocation,
        pub store_root: PathBuf,
        pub record: ExecutionRecord,
        pub q: QuiescedPartialCheckpointV1,
    }

    // No Drop-based decrement: panic, cancellation, timeout and lost messages do
    // not establish settlement. Actual returned ownership retains this charge.
    struct Charge(Arc<AtomicUsize>);
    impl Charge {
        fn admit(count: &Arc<AtomicUsize>) -> Result<Self> {
            count.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                |n| (n < LIMIT).then_some(n + 1))
                .map_err(|_| anyhow::anyhow!("source ownership slots are busy or require recovery"))?;
            Ok(Self(count.clone()))
        }
        fn settled(self) { self.0.fetch_sub(1, Ordering::AcqRel); }
    }
    /// Lives OUTSIDE catch_unwind. Fallible work borrows these owners; a panic
    /// cannot unwind their actual server/broker/lease/Journal handles away.
    struct OwnerCarrier {
        request: Option<ManagedSourcePrepareRequest>,
        writer: Option<SourceJournalFactory>,
        journal: Option<Journal>,
        preparation: Option<std::result::Result<PreparedManagedSource, ManagedSourcePrepareFailure>>,
        joined: Option<JoinedSourceShutdown>,
        pending_exit: Option<ExitedSourceShutdown>,
        retired: Option<RetiredSourceCheckpoint>,
        charge: Option<Charge>,
        adopted: bool,
        may_have_helper: bool,
        guest_settled: Option<FinalizationBudget>,
        panic_payload: Option<Box<dyn std::any::Any + Send>>,
    }
    /// Opaque actual ownership, never a serialized recovery record. Keep this
    /// value in a bounded supervisor or pass it by ownership to a retry worker.
    pub(crate) struct SourceRecoveryOwner {
        carrier: OwnerCarrier, detail: &'static str, thread_settlement_failed: bool,
    }
    pub(crate) struct RetiredSourceCheckpoint {
        joined: JoinedSourceShutdown,
        checkpoint: JoinedSourceCheckpoint,
        projection: Result<()>,
    }
    pub(crate) enum SourceOwnerExit {
        Settled { unused: bool, retired: Option<RetiredSourceCheckpoint> },
        Recovery(SourceRecoveryOwner),
    }
    pub(crate) struct SourceOwnerJoinHandoff {
        fence: ManagedLaunchFence,
        worker: thread::JoinHandle<SourceOwnerExit>,
        supervision: SourceSupervisionKey,
    }
    pub(crate) struct SourceOwnerJoined {
        pub fence: ManagedLaunchFence,
        pub result: thread::Result<SourceOwnerExit>,
        pub supervision: SourceSupervisionKey,
    }
    pub(crate) struct SourceRecoveryHandoff {
        pub fence: ManagedLaunchFence,
        pub owner: SourceRecoveryOwner,
        pub supervision: SourceSupervisionKey,
    }
    pub(crate) struct SourceOwnerShutdownHandoff {
        slots: [Option<SourceSlotShutdown>; LIMIT],
    }
    struct SourceSlotShutdown {
        supervision: SourceSupervision,
        worker: Option<thread::JoinHandle<SourceOwnerExit>>,
        recovery: Option<SourceRecoveryOwner>,
        checkpoint: Option<JoinedSourceCheckpoint>,
        live: Option<Live>,
    }
    impl SourceOwnerJoinHandoff {
        /// OFF-APP ONLY, on an already charged bounded join owner. is_finished
        /// is only a hint; actual join can still wait for TLS teardown.
        pub(crate) fn join_off_app(self) -> SourceOwnerJoined {
            SourceOwnerJoined { fence: self.fence, result: self.worker.join(), supervision: self.supervision }
        }
    }
    impl SourceRecoveryOwner {
        /// OFF-APP ONLY. Borrow the actual owner across the retry worker's own
        /// catch boundary; return/store this owner after every error or panic.
        pub(crate) fn retry_unused_off_app(&mut self) -> Result<()> {
            anyhow::ensure!(!self.thread_settlement_failed && !self.carrier.adopted && !self.carrier.may_have_helper,
                "returned owner cannot prove that no helper exists");
            if let Some(payload) = self.carrier.panic_payload.take() { drop(payload); } // Off-App, under outer retained catch.
            let journal = self.carrier.journal.as_mut().context("returned source writer absent")?;
            match self.carrier.preparation.as_mut() {
                Some(Ok(prepared)) => prepared.retire_unused(journal)?,
                Some(Err(failed)) => { failed.settle_unused(journal)?;
                    anyhow::ensure!(failed.is_settled(), "failed source owner has not settled"); }
                None => anyhow::bail!("returned preparation custody is unresolved"),
            }
            if let Some(charge) = self.carrier.charge.take() { charge.settled(); }
            Ok(())
        }
        /// OFF-APP ONLY. Resolves source ownership; the supervisor must still
        /// actually join the retry worker before App accepts its result.
        pub(crate) fn into_exit(self) -> SourceOwnerExit {
            if self.carrier.charge.is_none() && self.carrier.panic_payload.is_none() && !self.thread_settlement_failed {
                SourceOwnerExit::Settled { unused: !self.carrier.adopted && !self.carrier.may_have_helper,
                    retired: self.carrier.retired }
            }
            else { SourceOwnerExit::Recovery(self) }
        }
    }
    enum Command {
        // Only the App path that has NEVER attempted a spawn may send this.
        NoHelper,
        Activate(Arc<HelperProcessWitness>),
        SettleGuest(FinalizationBudget),
        Join(ExitedSourceShutdown),
        RetryUnused,
        Uncertain,
    }
    enum Event {
        Offered { assignment: RemoteSourceAssignment, handoff: ManagedExecutionHandoff,
            issuer: Arc<HostSourceIssuer> },
        Activated(Result<()>),
        GuestSettled(Result<()>),
        UnusedSettled { failed: bool },
        RecoveryRequired { retry_unused: bool, detail: &'static str },
    }
    struct Message { fence: ManagedLaunchFence, event: Event }
    struct Stop { request: StopAndCheckpoint, budget: FinalizationBudget, guest_settled: bool }
    struct Live {
        assignment: RemoteSourceAssignment,
        issuer: Arc<HostSourceIssuer>,
        custody: Option<ManagedHelperCustody>,
        handoff: HandoffReceipt,
        control: ManagedExecutionHandle,
        handoff_transferred: bool,
        activation_requested: bool,
        activated: bool,
        backend_ready: bool,
        stop: Option<Stop>,
        provisional: Option<ProvisionalSourceShutdown>,
        exit: Option<SuccessfulManagedHelperExit>,
        pending_exit: Option<ExitedSourceShutdown>,
        joining: bool,
    }
    struct Entry {
        fence: ManagedLaunchFence,
        selection: RemoteLaunchRequest,
        assigned: Option<RemoteSourceAssignment>,
        location: ManagedRunLocation,
        native: PathBuf,
        commands: SyncSender<Command>,
        cancelled: Arc<AtomicBool>,
        started: Instant,
        stop_requested: bool,
        recovery: bool,
        retry_unused: bool,
        worker: Option<thread::JoinHandle<SourceOwnerExit>>,
        worker_join_pending: bool,
        worker_settled: bool,
        remove_after_join: bool,
        recovery_owner: Option<SourceRecoveryOwner>,
        supervision: SourceSupervision,
        live: Option<Live>,
        checkpoint: Option<JoinedSourceCheckpoint>,
    }
    pub(crate) struct ManagedSourceAppState {
        charged: Arc<AtomicUsize>,
        send: SyncSender<Message>,
        receive: Receiver<Message>,
        entries: BTreeMap<String, Entry>,
        supervisor: Option<Arc<SourceOwnerLifetimeSupervisor>>,
    }
    impl Default for ManagedSourceAppState {
        fn default() -> Self {
            let (send, receive) = mpsc::sync_channel(EVENT_LIMIT);
            Self { charged: Arc::new(AtomicUsize::new(0)), send, receive, entries: BTreeMap::new(), supervisor: None }
        }
    }
    impl Drop for ManagedSourceAppState {
        fn drop(&mut self) {
            if self.entries.is_empty() { return; }
            let handoff = shutdown_handoff(std::mem::take(&mut self.entries));
            self.supervisor.as_ref().expect("source lifetime supervisor absent").retain_shutdown(handoff);
        }
    }
    fn shutdown_handoff(entries: BTreeMap<String, Entry>) -> SourceOwnerShutdownHandoff {
        let mut handoff = SourceOwnerShutdownHandoff { slots: std::array::from_fn(|_| None) };
        for (index, (_, mut entry)) in entries.into_iter().enumerate() {
            entry.cancelled.store(true, Ordering::Release);
            drop(entry.commands); // Control/custody are retained below; no PTY leaves App.
            handoff.slots[index] = Some(SourceSlotShutdown { supervision: entry.supervision,
                worker: entry.worker.take(), recovery: entry.recovery_owner.take(),
                checkpoint: entry.checkpoint.take(), live: entry.live.take() });
        }
        handoff
    }
    fn emit(send: &SyncSender<Message>, wake: &Option<PtyOutputWaker>,
        fence: &ManagedLaunchFence, event: Event) -> bool {
        let delivered = send.send(Message { fence: fence.clone(), event }).is_ok();
        if let Some(wake) = wake { wake(); }
        delivered
    }
    fn source_marker(assignment: &RemoteSourceAssignment) -> ManagedRunSource {
        ManagedRunSource::SourceAssigned { assignment: Box::new(assignment.clone()) }
    }
    fn row<'a>(app: &'a mut App, entry: &Entry, owner: &OwnerFence) -> Option<&'a mut AgentRun> {
        if &entry.fence.owner != owner { return None; }
        app.agents.iter_mut().find(|run| run.id == entry.fence.run_id
            && run.managed_store.as_ref() == Some(&entry.location)
            && match (&run.managed_local, &entry.assigned) {
                (Some(ManagedRunSource::Reserved { fence, runnable_root }), None) =>
                    fence == &entry.fence && runnable_root == &entry.selection.root.root,
                (Some(ManagedRunSource::SourceAssigned { assignment }), Some(expected)) =>
                    assignment.as_ref() == expected,
                _ => false,
            })
    }
    fn diagnostic(run: &mut AgentRun, recovery: bool, text: &str) {
        if recovery { run.status = AgentStatus::Orphaned; }
        run.completed_at = None; run.last_error = Some(text.into());
    }

    impl App {
        #[cfg(test)]
        pub(crate) fn managed_source_diagnostic(&self, id: &str) -> serde_json::Value {
            let Some(entry) = self.managed_source.entries.get(id) else {
                return serde_json::json!({"entry":false});
            };
            serde_json::json!({"entry":true,"recovery":entry.recovery,
                "cancelled":entry.cancelled.load(Ordering::Acquire),
                "worker_finished_hint":entry.worker.as_ref().is_some_and(|worker|worker.is_finished()),
                "worker_join_pending":entry.worker_join_pending,"worker_settled":entry.worker_settled,
                "recovery_owner":entry.recovery_owner.is_some(),"checkpoint":entry.checkpoint.is_some(),
                "live":entry.live.is_some(),"activated":entry.live.as_ref().is_some_and(|live|live.activated),
                "handoff_transferred":entry.live.as_ref().is_some_and(|live|live.handoff_transferred),
                "activation_requested":entry.live.as_ref().is_some_and(|live|live.activation_requested),
                "backend_ready":entry.live.as_ref().is_some_and(|live|live.backend_ready),
                "stop":entry.live.as_ref().is_some_and(|live|live.stop.is_some()),
                "guest_settled":entry.live.as_ref().and_then(|live|live.stop.as_ref()).is_some_and(|stop|stop.guest_settled),
                "provisional":entry.live.as_ref().is_some_and(|live|live.provisional.is_some()),
                "successful_exit":entry.live.as_ref().is_some_and(|live|live.exit.is_some()),
                "joining":entry.live.as_ref().is_some_and(|live|live.joining)})
        }

        pub(crate) fn ensure_managed_source_shutdown_ready(&self) -> Result<()> {
            anyhow::ensure!(self.managed_source.entries.is_empty()
                && self.managed_source.supervisor.as_ref().is_none_or(|supervisor|
                    supervisor.status().iter().all(|slot| !slot.occupied)),
                "source helper, checkpoint or cleanup ownership still requires its App and Engine owner");
            Ok(())
        }
        pub(crate) fn configure_source_owner_supervisor(&mut self,
            supervisor: Arc<SourceOwnerLifetimeSupervisor>) -> Result<()> {
            anyhow::ensure!(self.managed_source.entries.is_empty() && self.managed_source.supervisor.is_none(),
                "source lifetime supervisor cannot replace live ownership");
            self.managed_source.supervisor = Some(supervisor);
            Ok(())
        }
        /// The caller has already reserved the durable owner/run epoch and the
        /// matching new App row. This method does NO Journal, FS or CAS IO.
        pub(crate) fn queue_managed_source(&mut self, index: usize,
            request: ManagedSourcePrepareRequest, native: PathBuf,
            writer: SourceJournalFactory, owner: &OwnerFence) -> Result<()> {
            let fence = request.fence().clone();
            let selection = request.selection().clone();
            let location = request.managed_run()?;
            let journal_path = request.journal_path().to_path_buf();
            let run = self.agents.get(index).context("source launch row absent")?;
            anyhow::ensure!(self.managed_source.supervisor.is_some(),
                "source launch requires an explicit lifetime join/recovery supervisor");
            anyhow::ensure!(&fence.owner == owner && native.is_absolute() && native.to_str().is_some()
                && journal_path == location.store().binding().journal
                && location.store().binding().repository == self.cwd
                && location.store().binding().project_id == owner.project_id
                && run.id == fence.run_id && run.status == AgentStatus::Running
                && run.managed_store.as_ref() == Some(&location)
                && run.managed_local.as_ref() == Some(&ManagedRunSource::Reserved {
                    fence: fence.clone(), runnable_root: selection.root.root.clone() })
                && run.execution_profile.as_deref() == Some("offline-command-v1")
                && run.terminal.is_none() && run.session_id.is_none() && !run.plain_process
                && !run.is_main() && !run.is_orchestrator() && !is_cloud_owned_agent(run)
                && run.workspace_path.is_none() && run.jj_change_id.is_none(),
                "source launch differs from the exact pre-reserved source row");
            anyhow::ensure!(self.managed_source.entries.len() < LIMIT
                && !self.managed_source.entries.contains_key(&fence.run_id), "source row already owned");
            let supervision = self.managed_source.supervisor.as_ref().unwrap().reserve(&fence)?;
            let charge = match Charge::admit(&self.managed_source.charged) {
                Ok(charge) => charge, Err(error) => { supervision.cancel_before_effects(); return Err(error); }
            };
            let (commands, receive) = mpsc::sync_channel(2);
            let cancelled = Arc::new(AtomicBool::new(false));
            let worker_cancelled = cancelled.clone();
            let send = self.managed_source.send.clone();
            let wake = self.pty_output_waker.clone();
            let worker_fence = fence.clone();
            let projection = serde_json::json!({"id":run.id,"task":run.task,
                "backend":run.backend.as_str(),"model":run.model,"mode":"execute",
                "status":"orphaned","executionProfile":"offline-command-v1"});
            let worker_location = location.clone();
            let spawn = thread::Builder::new().name("rudder-source-owner".into()).spawn(move || {
                let mut carrier = OwnerCarrier { request: Some(request), writer: Some(writer),
                    journal: None, preparation: None, joined: None, pending_exit: None, retired: None, charge: Some(charge),
                    adopted: false, may_have_helper: false, guest_settled: None, panic_payload: None };
                let mut projection = projection;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner_worker(&mut carrier, &journal_path, &worker_location, &mut projection,
                        &worker_cancelled, &receive, &send, &wake, &worker_fence)
                }));
                let panicked = match result { Ok(()) => false,
                    Err(payload) => { carrier.panic_payload = Some(payload); true } };
                let escaped_preparation = carrier.request.is_none() && carrier.preparation.is_none();
                if carrier.charge.is_none() && carrier.panic_payload.is_none() {
                    return SourceOwnerExit::Settled { unused: !carrier.adopted && !carrier.may_have_helper,
                        retired: carrier.retired };
                }
                // Ownership returns through the retained JoinHandle even when
                // the event channel or waker failed. Do not notify by dropping
                // the actual owner, and never join this handle on App.
                SourceOwnerExit::Recovery(SourceRecoveryOwner { carrier, thread_settlement_failed: false, detail:
                    if escaped_preparation {
                        "preparation escaped its staged panic boundary; source custody requires reconciliation"
                    } else if panicked { "source owner panicked; actual staged ownership returned for recovery" }
                    else { "source owner returned before settlement; actual ownership requires recovery" } })
            });
            let worker = match spawn { Ok(worker) => worker, Err(error) => {
                // No worker or source effect exists on a failed thread spawn.
                self.managed_source.charged.fetch_sub(1, Ordering::AcqRel);
                supervision.cancel_before_effects(); return Err(error.into());
            } };
            self.managed_source.entries.insert(fence.run_id.clone(), Entry { fence,
                selection, assigned: None, location, native, commands, cancelled, started: Instant::now(),
                stop_requested: false, recovery: false, retry_unused: false, worker: Some(worker),
                worker_join_pending: false, worker_settled: false, remove_after_join: false,
                recovery_owner: None, supervision, live: None, checkpoint: None });
            diagnostic(&mut self.agents[index], false, "preparing reserved remote source");
            self.dirty = true;
            Ok(())
        }
        /// Pure adoption after an ACTUAL off-App join. A stale/missing row
        /// returns the entire owning result to its supervisor for retention.
        pub(crate) fn adopt_managed_source_owner_join(&mut self, joined: SourceOwnerJoined,
            owner: &OwnerFence) -> std::result::Result<(), SourceOwnerJoined> {
            let Some(mut entry) = self.managed_source.entries.remove(&joined.fence.run_id) else { return Err(joined); };
            if entry.fence != joined.fence || !entry.worker_join_pending
                || !entry.supervision.matches(&joined.supervision) || row(self, &entry, owner).is_none() {
                self.managed_source.entries.insert(entry.fence.run_id.clone(), entry); return Err(joined);
            }
            entry.worker_join_pending = false;
            let detail = match joined.result {
                Ok(SourceOwnerExit::Settled { unused, retired }) => {
                    entry.worker_settled = true;
                    if let Some(retired) = retired {
                        let fresh = !entry.recovery && entry.live.as_ref().is_some_and(|live|
                            live.joining && retired.joined.matches_issuer(&live.issuer))
                            && retired.joined.provisional().is_ok()
                            && entry.assigned.as_ref() == Some(&retired.checkpoint.assignment)
                            && retired.checkpoint.managed_run == entry.location;
                        if !fresh {
                            entry.worker_settled = false;
                            entry.worker_join_pending = true;
                            self.managed_source.entries.insert(entry.fence.run_id.clone(), entry);
                            return Err(SourceOwnerJoined { fence: joined.fence,
                                supervision: joined.supervision,
                                result: Ok(SourceOwnerExit::Settled { unused, retired: Some(retired) }) });
                        }
                        if let Some(run) = row(self, &entry, owner) {
                            run.status = if retired.projection.is_ok() { AgentStatus::Paused } else { AgentStatus::Orphaned };
                            diagnostic(run, false, "helper exited; retained checkpoint awaits publication");
                        }
                        entry.checkpoint = Some(retired.checkpoint);
                    }
                    if unused && entry.live.is_none() {
                        entry.remove_after_join = true;
                        if let Some(run) = row(self, &entry, owner) {
                            run.status = if entry.stop_requested { AgentStatus::Paused } else { AgentStatus::Orphaned };
                            diagnostic(run, false, "unused source owner settled; no helper checkpoint accepted");
                        }
                        None
                    } else if unused {
                        entry.recovery = true; Some("unused source outcome differs from the live App helper; recovery required")
                    } else { None }
                }
                Ok(SourceOwnerExit::Recovery(recovery)) => {
                    let detail = recovery.detail;
                    entry.recovery = true; entry.retry_unused = false; entry.recovery_owner = Some(recovery);
                    Some(detail)
                }
                Err(_) => { entry.recovery = true;
                    Some("source owner join failed outside its retained boundary; custody unresolved") }
            };
            if let Some(detail) = detail { if let Some(run) = row(self, &entry, owner) { diagnostic(run, true, detail); } }
            let remove = entry.worker_settled && entry.remove_after_join && entry.checkpoint.is_none();
            if !remove { self.managed_source.entries.insert(entry.fence.run_id.clone(), entry); }
            else { entry.supervision.release_settled(entry.live.take()); }
            self.dirty = true;
            Ok(())
        }
        /// Required before App destruction. Root must adopt this into its
        /// lifetime supervisor; transfer is NOT source or helper settlement.
        pub(crate) fn handoff_managed_source_shutdown(&mut self) -> SourceOwnerShutdownHandoff {
            shutdown_handoff(std::mem::take(&mut self.managed_source.entries))
        }
        pub(crate) fn retry_managed_source_unused_settlement(&mut self, id: &str) -> Result<()> {
            let supervisor = self.managed_source.supervisor.clone().context("source lifetime supervisor absent")?;
            let entry = self.managed_source.entries.get_mut(id).context("source owner absent")?;
            if let Some(owner) = entry.recovery_owner.take() {
                let job = SourceRecoveryHandoff { fence: entry.fence.clone(), owner, supervision: entry.supervision.key() };
                match supervisor.try_retry_unused(job) {
                    Ok(()) => { entry.worker_join_pending = true; return Ok(()); }
                    Err(job) => { entry.recovery_owner = Some(job.owner);
                        anyhow::bail!("source recovery is busy or needs owned recovery; actual owner remains retained"); }
                }
            }
            anyhow::ensure!(entry.recovery && entry.retry_unused && entry.live.is_none(),
                "source owner has no retryable unused settlement");
            entry.commands.try_send(Command::RetryUnused).context("source cleanup admission failed")?;
            entry.retry_unused = false;
            Ok(())
        }
        /// Caller must first admit its own bounded publication worker. Moving
        /// this DATA does not publish it or accept a new source/history head.
        pub(crate) fn take_joined_source_checkpoint(&mut self, id: &str) -> Option<JoinedSourceCheckpoint> {
            let entry = self.managed_source.entries.get_mut(id)?;
            let value = entry.checkpoint.take()?;
            entry.remove_after_join = true;
            if entry.worker_settled {
                let mut entry = self.managed_source.entries.remove(id).unwrap();
                entry.supervision.release_settled(entry.live.take());
            }
            Some(value)
        }
    }

    fn owner_worker(carrier: &mut OwnerCarrier, journal_path: &Path,
        location: &ManagedRunLocation, projection: &mut serde_json::Value,
        cancelled: &AtomicBool, commands: &Receiver<Command>, send: &SyncSender<Message>,
        wake: &Option<PtyOutputWaker>, fence: &ManagedLaunchFence) {
        let initial = (|| -> Result<()> {
            anyhow::ensure!(!cancelled.load(Ordering::Acquire), "source cancelled before effects");
            let writer = carrier.writer.take().context("source writer factory absent")?;
            carrier.journal = Some(writer()?);
            anyhow::ensure!(carrier.journal.as_ref().unwrap().database_path()? == journal_path,
                "source writer Journal differs");
            let reserved = ManagedRunSource::Reserved { fence: fence.clone(),
                runnable_root: carrier.request.as_ref().unwrap().selection().root.root.clone() };
            // These are idempotent immutable reservation + mutable projection
            // writes, never substitutes for the actual lease checked by prepare.
            location.persist_reservation(&reserved)?;
            location.write_projection(&reserved, projection)?;
            Ok(())
        })();
        if initial.is_err() {
            carrier.charge.take().unwrap().settled(); emit(send, wake, fence,
                Event::UnusedSettled { failed: !cancelled.load(Ordering::Acquire) }); return;
        }
        // prepare_owned MUST itself stage its resources outside an inner catch:
        // it consumes request, so the outer carrier cannot rescue an escaping
        // panic inside that transfer. The companion patch supplies this boundary.
        carrier.preparation = Some(source_prepare::prepare_owned(carrier.request.take().unwrap(),
            carrier.journal.as_mut().unwrap(), cancelled));
        let OwnerCarrier { journal, preparation, joined, pending_exit, retired, charge, adopted, may_have_helper, guest_settled, .. } = carrier;
        let journal = journal.as_mut().unwrap();
        if let Some(Err(failed)) = preparation.as_mut() {
                if settle_unused(|| { failed.settle_unused(journal)?;
                    anyhow::ensure!(failed.is_settled(), "failed source owner has not settled"); Ok(()) },
                    commands, send, wake, fence) {
                    charge.take().unwrap().settled(); emit(send, wake, fence,
                        Event::UnusedSettled { failed: !cancelled.load(Ordering::Acquire) });
                }
                return;
        }
        let prepared = match preparation.as_mut() { Some(Ok(prepared)) => prepared, _ => unreachable!() };
        let offered = (|| -> Result<Event> {
            anyhow::ensure!(&prepared.managed_run == location
                && prepared.binding().format_version == 4
                && prepared.binding().execution_profile == "offline-command-v1"
                && prepared.binding().frontend_start == ManagedFrontendStart::Immediate,
                "source helper profile differs from qualified offline startup");
            location.write_projection(&source_marker(prepared.assignment()), projection)?;
            Ok(Event::Offered { assignment: prepared.assignment().clone(),
                handoff: ManagedExecutionHandoff::new(prepared.execution_spec(), prepared.binding().clone())?,
                issuer: prepared.issuer()? })
        })();
        let delivered = if let Ok(event) = offered {
            *may_have_helper = true; // A delivered offer can race App spawning.
            let delivered = emit(send, wake, fence, event);
            if !delivered { *may_have_helper = false; }
            delivered
        } else { false };
        if !delivered {
            if settle_unused(|| prepared.retire_unused(journal), commands, send, wake, fence) {
                charge.take().unwrap().settled(); emit(send, wake, fence, Event::UnusedSettled { failed: true });
            }
            return;
        }
        // Once the offer was delivered the App may have spawned a helper. A
        // cancellation, missing reply or channel loss CANNOT imply NoHelper.
        while let Ok(command) = commands.recv() {
            match command {
                Command::NoHelper if !*adopted => {
                    *may_have_helper = false; // Explicit App proof, never timeout inference.
                    if settle_unused(|| prepared.retire_unused(journal), commands, send, wake, fence) {
                        charge.take().unwrap().settled(); emit(send, wake, fence, Event::UnusedSettled { failed: false });
                    }
                    return;
                }
                Command::Activate(helper) if !*adopted => {
                    prepared.adopt(); *adopted = true;
                    let result = prepared.activate_helper(helper);
                    emit(send, wake, fence, Event::Activated(result));
                }
                Command::SettleGuest(budget) if *adopted => {
                    // StopV4 was already queued. Success here additionally proves
                    // revoke + actual broker acceptor/request/TLS settlement.
                    *guest_settled = None;
                    let result = prepared.settle_guest(journal, &budget);
                    if result.is_ok() { *guest_settled = Some(budget); }
                    emit(send, wake, fence, Event::GuestSettled(result));
                }
                Command::Join(exited) => {
                    if pending_exit.is_some() {
                        // The App can send only one linear packet. Keep a
                        // duplicate as an explicit invariant failure, never
                        // replace and discard the original ownership.
                        unreachable!("source exit packet submitted twice");
                    }
                    *pending_exit = Some(exited);
                    let result = (|| -> Result<JoinedSourceCheckpoint> {
                        anyhow::ensure!(*adopted && guest_settled.is_some(),
                            "source exit has no actual guest settlement");
                        // Fresh actual issuer query after actual child wait. Once
                        // returned, joined proof also stays in the outer carrier.
                        *joined = Some(ExitedSourceShutdown::join_retaining(pending_exit)?);
                        let joined = joined.as_ref().unwrap();
                        anyhow::ensure!(joined.provisional()?.budget().constraint()
                            == guest_settled.as_ref().unwrap().constraint(), "guest settlement budget changed");
                        let record = joined.provisional()?.record().clone();
                        let Some(CheckpointRef::PartialRemote(q)) = record.checkpoint() else {
                            anyhow::bail!("joined source has no partial checkpoint")
                        };
                        let checkpoint = JoinedSourceCheckpoint { assignment: prepared.assignment().clone(),
                            managed_run: location.clone(), store_root: prepared.execution_spec().store_root.clone(),
                            q: q.clone(), record };
                        joined.provisional()?.budget().remaining()?;
                        location.persist_source_joined_checkpoint(fence, &checkpoint.record)?;
                        joined.provisional()?.budget().remaining()?;
                        prepared.retire_joined(joined, journal)?;
                        Ok(checkpoint)
                    })();
                    if let Ok(checkpoint) = result {
                        projection["status"] = "paused".into();
                        let projection = location.write_projection(&source_marker(prepared.assignment()), projection);
                        *retired = Some(RetiredSourceCheckpoint { joined: joined.take().unwrap(), checkpoint, projection });
                        charge.take().unwrap().settled();
                        return;
                    }
                    emit(send, wake, fence, Event::RecoveryRequired { retry_unused: false,
                        detail: "source exit join or actual source retirement failed; ownership retained" });
                }
                _ => {
                    prepared.adopt(); *adopted = true;
                    emit(send, wake, fence, Event::RecoveryRequired { retry_unused: false,
                        detail: "source helper may exist; retained source requires owned recovery" });
                }
            }
        }
    }
    fn settle_unused(mut settle: impl FnMut() -> Result<()>, commands: &Receiver<Command>,
        send: &SyncSender<Message>, wake: &Option<PtyOutputWaker>, fence: &ManagedLaunchFence) -> bool {
        loop {
            if settle().is_ok() { return true; }
            emit(send, wake, fence, Event::RecoveryRequired { retry_unused: true,
                detail: "unused source cleanup has not actually settled; admission remains charged" });
            if !matches!(commands.recv(), Ok(Command::RetryUnused)) { return false; }
        }
    }

    fn uncertain(entry: &mut Entry, run: Option<&mut AgentRun>, text: &str) {
        if !entry.recovery { let _ = entry.commands.try_send(Command::Uncertain); }
        entry.recovery = true;
        if let Some(run) = run {
            if let (Some(terminal), Some(custody)) = (run.terminal.as_mut(),
                entry.live.as_ref().and_then(|live|live.custody.as_ref())) {
                if terminal.matches_managed_helper_custody(custody) { terminal.request_termination(); }
            }
            diagnostic(run, true, text);
        }
    }
    impl ManagedSourceAppState {
        fn poll(&mut self, app: &mut App, owner: &OwnerFence) {
            for _ in 0..EVENT_LIMIT {
                let Ok(message) = self.receive.try_recv() else { break; };
                let Some(mut entry) = self.entries.remove(&message.fence.run_id) else { continue; };
                if entry.fence != message.fence { self.entries.insert(entry.fence.run_id.clone(), entry); continue; }
                match message.event {
                    Event::Offered { assignment, handoff, issuer } => {
                        let allowed = !entry.recovery && !entry.cancelled.load(Ordering::Acquire)
                            && entry.started.elapsed() < STARTUP_LIMIT
                            && assignment.validate(&entry.fence).is_ok()
                            && assignment.selection == entry.selection
                            && row(app, &entry, owner).is_some_and(|run|run.terminal.is_none()
                                && run.status == AgentStatus::Running);
                        if !allowed { let _ = entry.commands.try_send(Command::NoHelper); }
                        else {
                            let run = row(app, &entry, owner).unwrap();
                            run.managed_local = Some(source_marker(&assignment));
                            let options = TerminalPaneOptions { size: run.terminal_size.unwrap_or_default(),
                                cwd: None, ..TerminalPaneOptions::default() };
                            let command = TerminalCommand::with_args(entry.native.display().to_string(), handoff.helper_args())
                                .with_env("RUDDER_FABRIC_ATTEMPT_ID", &entry.fence.attempt_id)
                                .with_env("RUDDER_EXECUTION_PROFILE", "offline-command-v1");
                            entry.assigned = Some(assignment.clone());
                            let spawned = (|| -> Result<Live> {
                                let terminal = TerminalPane::spawn_shell_or_command(Some(command), options)?;
                                App::attach_output_waker(&app.pty_output_waker, &terminal);
                                // Install the owned pane before any fallible activation.
                                let run = app.agents.iter_mut().find(|run|run.id == entry.fence.run_id).unwrap();
                                run.terminal = Some(terminal);
                                let terminal = run.terminal.as_ref().unwrap();
                                let custody = terminal.retain_managed_helper_custody()?;
                                let (receipt, control) = handoff.activate(terminal.child_process_id().context("source helper PID absent")?)?;
                                Ok(Live { assignment, issuer, custody: Some(custody), handoff: receipt, control,
                                    handoff_transferred: false, activation_requested: false,
                                    activated: false, backend_ready: false, stop: None, provisional: None, exit: None,
                                    pending_exit: None, joining: false })
                            })();
                            match spawned {
                                Ok(live) => {
                                    entry.live = Some(live);
                                }
                                Err(_) => {
                                    // This path owns the just-created pane, even if custody/activation failed.
                                    if let Some(run) = app.agents.iter_mut().find(|run|run.id == entry.fence.run_id) {
                                        if let Some(terminal) = run.terminal.as_mut() { terminal.request_termination(); }
                                        diagnostic(run, true, "source helper spawn/activation uncertain; source retained");
                                    }
                                    uncertain(&mut entry, None, "source helper spawn uncertain");
                                }
                            }
                        }
                    }
                    Event::Activated(result) => {
                        if result.is_ok() { if let Some(live) = &mut entry.live { live.activated = true; } }
                        else { uncertain(&mut entry, None, "source activation failed"); }
                    }
                    Event::GuestSettled(result) => {
                        if result.is_ok() { if let Some(stop) = entry.live.as_mut().and_then(|live|live.stop.as_mut()) { stop.guest_settled = true; } }
                        else { uncertain(&mut entry, None, "source guest revocation or callback settlement failed"); }
                    }
                    Event::RecoveryRequired { retry_unused, detail } => {
                        entry.retry_unused = retry_unused;
                        entry.recovery = true;
                        if let Some(run) = row(app, &entry, owner) { diagnostic(run, true, detail); }
                    }
                    Event::UnusedSettled { failed } => {
                        if let Some(run) = row(app, &entry, owner) {
                            run.status = if failed { AgentStatus::Orphaned } else { AgentStatus::Paused };
                            diagnostic(run, false, "unused remote source settled; no helper/checkpoint was accepted");
                        }
                        entry.remove_after_join = true;
                    }
                }
                app.dirty = true;
                let remove = entry.worker_settled && entry.remove_after_join && entry.checkpoint.is_none();
                if !remove { self.entries.insert(entry.fence.run_id.clone(), entry); }
                else { entry.supervision.release_settled(entry.live.take()); }
            }
            let ids: Vec<_> = self.entries.keys().cloned().collect();
            for id in ids {
                let mut entry = self.entries.remove(&id).unwrap();
                if entry.checkpoint.is_some() || entry.remove_after_join
                    || (entry.recovery_owner.is_some() && entry.live.is_none()) {
                    self.entries.insert(id, entry); continue;
                }
                if entry.live.is_none() {
                    if entry.started.elapsed() >= STARTUP_LIMIT {
                        entry.cancelled.store(true, Ordering::Release);
                        if let Some(run) = row(app, &entry, owner) {
                            diagnostic(run, true, "source preparation timed out; awaiting actual unused settlement");
                        }
                    }
                    self.entries.insert(id, entry); continue;
                }
                if row(app, &entry, owner).is_none() { uncertain(&mut entry, None, "source row or owner replaced"); }
                let was_recovery = entry.recovery;
                let mut output_changed = false;
                if let Some(run) = row(app, &entry, owner) {
                    let last_output = run.last_output_at;
                    tick_live(&mut entry, run);
                    output_changed = run.last_output_at != last_output;
                    if entry.recovery { diagnostic(run, true, "source outcome uncertain; ownership retained for recovery"); }
                }
                app.dirty |= output_changed || entry.recovery != was_recovery;
                self.entries.insert(id, entry);
            }
        }
    }
    fn tick_live(entry: &mut Entry, run: &mut AgentRun) {
        let live = entry.live.as_mut().unwrap();
        if live.joining { return; }
        if run.status != AgentStatus::Running { entry.recovery = true; }
        let exact_terminal = run.terminal.as_ref().zip(live.custody.as_ref())
            .is_some_and(|(terminal,custody)|terminal.matches_managed_helper_custody(custody));
        if !exact_terminal { uncertain(entry, Some(run), "source owned terminal replaced"); return; }
        if !live.handoff_transferred {
            match live.handoff.try_result() {
                Some(Ok(())) => live.handoff_transferred = true,
                Some(Err(_)) => entry.recovery = true,
                None => {}
            }
        }
        // A freshly spawned pane has only a PENDING helper witness. The
        // authorizer requires actual acknowledged descriptor transfer; polling
        // that fact never waits on the App thread or renews its startup budget.
        if live.handoff_transferred && !live.activation_requested && !entry.recovery {
            match entry.commands.try_send(Command::Activate(live.control.helper_process_witness())) {
                Ok(()) => live.activation_requested = true,
                Err(_) => entry.recovery = true,
            }
        }
        for _ in 0..4 {
            let Some(event) = live.control.try_event() else { break; };
            match event {
                ManagedExecutionEvent::BackendReady { process } if !live.backend_ready
                    && process.matches_attempt(&entry.fence.run_id, &entry.fence.attempt_id, "offline-command-v1")
                    && !process.authenticated_start_required()
                    && process.check_current().is_ok() => { live.backend_ready = true; }
                ManagedExecutionEvent::SourceShutdown(value) if live.provisional.is_none()
                    && live.stop.as_ref().is_some_and(|stop| stop.request == *value.request()
                        && stop.budget.constraint() == value.budget().constraint()) => { live.provisional = Some(value); }
                _ => entry.recovery = true,
            }
        }
        if !live.backend_ready && entry.started.elapsed() >= STARTUP_LIMIT { entry.recovery = true; }
        if entry.stop_requested && !entry.recovery && live.stop.is_none() && live.activated && live.backend_ready {
            let request = StopAndCheckpoint { request_id: uuid::Uuid::new_v4().to_string(),
                execution_id: entry.fence.attempt_id.clone(), operation_id: entry.selection.attempt.operation_id.clone(),
                view_id: entry.selection.attempt.view_id.clone(), run_id: entry.fence.run_id.clone(),
                run_epoch: entry.fence.run_epoch, owner: entry.fence.owner_epoch() };
            match live.control.stop_and_checkpoint_v4(request.clone()) {
                Ok(budget) => {
                    // This API ALREADY queued StopV4. Off-App guest revoke and
                    // actual callback settlement follow under the SAME budget;
                    // the wire is not ordered after their completion.
                    if entry.commands.try_send(Command::SettleGuest(budget.clone())).is_err() { entry.recovery = true; }
                    live.stop = Some(Stop { request, budget, guest_settled: false });
                }
                Err(_) => entry.recovery = true,
            }
        }
        if live.stop.as_ref().is_some_and(|stop|stop.budget.remaining().is_err()) { entry.recovery = true; }
        let terminal = run.terminal.as_mut().unwrap();
        if !terminal.drain_output().is_empty() { run.last_output_at = Instant::now(); }
        let _ = terminal.take_notifications();
        if live.exit.is_none() {
            match terminal.successful_managed_helper_exit() {
                Ok(exit) => live.exit = exit,
                Err(_) => entry.recovery = true,
            }
        }
        if live.exit.is_some() && live.stop.is_none() { entry.recovery = true; }
        if entry.recovery {
            let ended = terminal.try_wait_after_output().is_ok_and(|exit|exit.is_some());
            uncertain(entry, Some(run), "source helper outcome uncertain");
            if ended { run.terminal = None; }
            return;
        }
        if live.stop.as_ref().is_some_and(|stop|stop.guest_settled)
            && live.provisional.is_some() && live.exit.is_some() {
            if live.pending_exit.is_none() {
                match ExitedSourceShutdown::new_retaining(&mut live.provisional, live.issuer.clone(),
                    &mut live.custody, &mut live.exit) {
                    Ok(exited) => live.pending_exit = Some(exited),
                    Err(_) => { entry.recovery = true;
                        diagnostic(run, true, "source exit witnesses require recovery"); return; }
                }
            }
            match entry.commands.try_send(Command::Join(live.pending_exit.take().unwrap())) {
                Ok(()) => { live.joining = true; run.terminal = None; }
                Err(error) => {
                    let command = match error { mpsc::TrySendError::Full(command)
                        | mpsc::TrySendError::Disconnected(command) => command };
                    let Command::Join(exited) = command else { unreachable!() };
                    live.pending_exit = Some(exited);
                    entry.recovery = true;
                    diagnostic(run, true, "source exit could not enter fresh join; original witnesses retained");
                }
            }
        }
    }
    pub(crate) fn poll(app: &mut App, owner: &OwnerFence) {
        let mut state = std::mem::take(&mut app.managed_source);
        state.poll(app, owner);
        app.managed_source = state;
        let Some(supervisor) = app.managed_source.supervisor.clone() else { return; };
        for entry in app.managed_source.entries.values_mut() {
            if !entry.worker.as_ref().is_some_and(|worker|worker.is_finished()) { continue; }
            let job = SourceOwnerJoinHandoff { fence: entry.fence.clone(), worker: entry.worker.take().unwrap(),
                supervision: entry.supervision.key() };
            match supervisor.try_join(job) {
                Ok(()) => entry.worker_join_pending = true,
                Err(job) => entry.worker = Some(job.worker), // Refusal returns the original handle.
            }
        }
        for _ in 0..LIMIT {
            let Some(joined) = supervisor.try_take_joined() else { break; };
            if let Err(joined) = app.adopt_managed_source_owner_join(joined, owner) {
                supervisor.retain_unadopted(joined); // Actual result, not an error-only notification.
            }
        }
    }
    pub(crate) fn stop(app: &mut App, index: usize) -> Option<bool> {
        let run = app.agents.get_mut(index)?;
        let entry = app.managed_source.entries.get_mut(&run.id)?;
        if run.managed_local.as_ref().and_then(ManagedRunSource::fence) != Some(&entry.fence)
            || run.managed_store.as_ref() != Some(&entry.location) { return Some(false); }
        entry.stop_requested = true;
        if entry.live.is_none() { entry.cancelled.store(true, Ordering::Release); }
        diagnostic(run, false, "stop requested; awaiting source activation/revocation, checkpoint and actual helper exit");
        app.dirty = true;
        Some(true)
    }
    pub(crate) fn accepts_input(app: &App, index: usize) -> Option<bool> {
        let run = app.agents.get(index)?;
        if let Some(entry) = app.managed_source.entries.get(&run.id) {
            return Some(!entry.recovery && !entry.stop_requested && entry.live.as_ref().is_some_and(|live|
                live.activated && live.backend_ready && run.managed_local.as_ref() == Some(&source_marker(&live.assignment))
                && run.terminal.as_ref().zip(live.custody.as_ref()).is_some_and(|(terminal,custody)|
                    terminal.matches_managed_helper_custody(custody))));
        }
        matches!(run.managed_local, Some(ManagedRunSource::SourceAssigned { .. })).then_some(false)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        include!("managed_source_app_tests.rs");
        include!("managed_source_activation_tests.rs");
        #[test]
        fn cancellation_and_dropped_results_cannot_release_ownership_admission() {
            let count = Arc::new(AtomicUsize::new(0));
            let first = Charge::admit(&count).unwrap();
            let second = Charge::admit(&count).unwrap();
            drop(first); // A counter drop cannot certify actual settlement.
            assert!(Charge::admit(&count).is_err());
            second.settled();
            assert_eq!(count.load(Ordering::Acquire), 1);
            Charge::admit(&count).unwrap().settled();
        }
    }
}
#[cfg(target_os = "linux")]
pub(crate) use linux::{ManagedSourceAppState, SourceJournalFactory, JoinedSourceCheckpoint,
    SourceOwnerExit, SourceOwnerJoinHandoff, SourceOwnerJoined, SourceRecoveryOwner,
    SourceRecoveryHandoff, SourceOwnerShutdownHandoff, SourceOwnerLifetimeSupervisor,
    SourceSupervisionKey, SourceSupervisorStatus, StoppedSourceSupervisor,
    poll, stop, accepts_input};
#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub(crate) struct ManagedSourceAppState;
#[cfg(not(target_os = "linux"))]
pub(crate) fn poll(_: &mut App, _: &OwnerFence) {}
#[cfg(not(target_os = "linux"))]
pub(crate) fn stop(_: &mut App, _: usize) -> Option<bool> { None }
#[cfg(not(target_os = "linux"))]
pub(crate) fn accepts_input(_: &App, _: usize) -> Option<bool> { None }

#[cfg(not(target_os = "linux"))]
impl App {
    pub(crate) fn ensure_managed_source_shutdown_ready(&self) -> Result<()> { Ok(()) }
}
