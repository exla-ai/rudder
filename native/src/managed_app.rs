//! Explicit retained-local launches. App owns reservation, result adoption and
//! PTYs; bounded workers own all materialization, credentials, grant and bind IO.
use super::*;
use crate::managed_launch::{ManagedLaunchFence, ManagedLocalLaunchInputs, ManagedRunSource};
use rudder_engine::journal::{Journal, OwnerFence};

#[cfg(target_os = "linux")]
#[path = "managed_runtime_task.rs"]
pub(crate) mod runtime_task;

#[cfg(target_os = "linux")]
mod local {
    use super::*;
    use super::runtime_task::RuntimeTaskStart;
    use crate::{agent_broker::{AgentBroker, BrokerConfig}, managed_launch::{self, PreparedManagedLaunch}};
    use rudder_engine::{execution::{handoff::{HandoffReceipt, ManagedExecutionHandoff, ManagedExecutionHandle, ManagedExecutionEvent, ValidatedManagedShutdown}, StopAndCheckpoint}, paths::FabricPaths};
    use std::sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Arc, mpsc::{self, Receiver, SyncSender}};
    use std::collections::BTreeMap;
    use crate::{engine_host::EngineSourceLease,
        managed_task_launch::{ManagedTaskAdmission, ManagedTaskLaunchPacket, PreparedManagedTask, ManagedTaskProject}};
    use rudder_engine::execution::finalization_budget::FinalizationBudget;
    use std::sync::{Mutex, atomic::{AtomicU8, AtomicU64}};
    use rudder_native::pty_terminal::{ManagedHelperCustody, SuccessfulManagedHelperExit, ManagedPtyActivationOwner, ManagedPtyDrainOwner};

    const MAX_PREPARATIONS: usize = 2;
    const PREPARATION_DEADLINE: Duration = Duration::from_secs(120);
    const STOP_DEADLINE: Duration = Duration::from_secs(30);
    const TASK_SLOTS: usize = 256;
    const TASK_WORKERS: usize = 4;
    const TASK_PENDING: u8 = 0;
    const TASK_ADOPTED: u8 = 1;
    const TASK_UNUSED: u8 = 2;
    const TASK_DRAINING: u8 = 3;
    const TASK_RECOVERY: u8 = 4;
    const TASK_SETTLED: u8 = 5;
    const TASK_JOINED: u8 = 6;
    const TASK_NEVER_STARTED:u8=7;
    include!("managed_never_started.rs");
    fn now_ms() -> Result<i64> {
        Ok(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis().try_into()?)
    }

    /// A timed-out/cancelled job retains its admission until actual settlement.
    /// The bounded result channel also retains admission if App falls behind.
    struct WorkerQueue<T> { active: Arc<AtomicUsize>, send: SyncSender<T>, receive: Receiver<T> }
    struct WorkerPermit(Arc<AtomicUsize>);
    impl Drop for WorkerPermit { fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); } }
    impl<T: Send + 'static> WorkerQueue<T> {
        fn new() -> Self { let (send, receive) = mpsc::sync_channel(MAX_PREPARATIONS);
            Self { active: Arc::new(AtomicUsize::new(0)), send, receive } }
        fn admit(&self) -> Result<WorkerPermit> {
            self.active.fetch_update(Ordering::AcqRel, Ordering::Acquire,
                |active| (active < MAX_PREPARATIONS).then_some(active + 1))
                .map_err(|_| anyhow::anyhow!("managed launch preparation is busy"))?;
            Ok(WorkerPermit(self.active.clone()))
        }
        fn spawn(&self, permit: WorkerPermit, waker: Option<PtyOutputWaker>, work: impl FnOnce(SyncSender<T>) -> T + Send + 'static,
            on_panic: impl FnOnce() -> T + Send + 'static) -> Result<()> {
            let send = self.send.clone();
            thread::Builder::new().name("rudder-managed-prepare".into()).spawn(move || {
                let _permit = permit;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work(send.clone())))
                    .unwrap_or_else(|_|on_panic());
                let _ = send.send(result);
                if let Some(waker) = waker { waker(); }
            })?;
            Ok(())
        }
        fn spawn_retaining(&self, permit: WorkerPermit, ticket: TaskWorkerTicket, waker: Option<PtyOutputWaker>,
            work: impl FnOnce(SyncSender<T>) -> T + Send + 'static, on_panic: impl FnOnce() -> T + Send + 'static) -> Result<()> {
            let send = self.send.clone();
            let slot=ticket.slot.clone();
            let worker = thread::Builder::new().name("rudder-managed-task-prepare".into()).spawn(move || {
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work(send.clone()))) {
                    Ok(result)=>result,Err(panic)=>{slot.carrier.lock().unwrap_or_else(|p|p.into_inner()).panics.push(panic);on_panic()}
                };
                if send.send(result).is_err() {
                    slot.state.store(if slot.may_have_helper.load(Ordering::Acquire) { TASK_RECOVERY } else { TASK_UNUSED },Ordering::Release);
                    slot.work.store(true,Ordering::Release);
                }
                if let Some(waker) = waker { waker(); }
            })?;
            // The ticket reserved one of this slot's four exact handle cells
            // before spawn. No callback/result receipt releases that admission.
            ticket.retain(RetainedTaskWorker { worker, permit }); Ok(())
        }
    }

    pub(crate) struct ManagedTaskJournalOwner { journal: Mutex<Journal>, lease: EngineSourceLease }
    impl ManagedTaskJournalOwner {
        fn check_original_cleanup_custody(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
            let journal=loop{task_time(budget,cancelled)?;match self.journal.try_lock(){Ok(v)=>break v,Err(std::sync::TryLockError::WouldBlock)=>thread::sleep(budget.phase_limit(Duration::from_millis(10))?),Err(_)=>anyhow::bail!("original cleanup writer poisoned")}};
            self.lease.check_journal(&journal)?;task_time(budget,cancelled)
        }
        /// Physical validation of an already-held broker writer; grants no SQL permission.
        pub(crate) fn check_broker_writer(&self,journal:&Journal)->Result<()> {self.lease.check_journal(journal)}
        pub(crate) fn managed_owner_lease(&self)->Option<&Arc<rudder_native::managed_owner_lease::ManagedOwnerLease>> {self.lease.managed_owner_lease()}
        pub(crate) fn check_shared_admission(&self)->Result<()> {self.lease.check_active_for_adoption()}
        pub(crate) fn shared_admission_closed(&self)->bool {self.lease.shared_admission_closed()}
        pub(crate) fn assert_original_held_custody_off_app(&self)->Result<()> {self.lease.check_current()}
        pub(crate) fn owner(&self)->&OwnerFence { self.lease.owner() }
        /// OFF-App only. Rotations borrow the same actual owner-bound writer;
        /// no scheduler, saved path or independently opened database is used.
        pub(crate) fn with_writer<T>(&self,budget:&FinalizationBudget,cancelled:&AtomicBool,
            work:impl FnOnce(&mut Journal)->Result<T>) -> Result<T> {
            let mut journal=loop {
                task_time(budget,cancelled)?;
                match self.journal.try_lock() { Ok(journal)=>break journal,
                    Err(std::sync::TryLockError::WouldBlock)=>thread::sleep(budget.phase_limit(Duration::from_millis(10))?),
                    Err(_)=>anyhow::bail!("managed task writer poisoned") }
            };
            self.lease.check_journal(&journal)?;self.lease.wait_shared_current(budget,cancelled)?;task_time(budget,cancelled)?;
            let result=work(&mut journal)?;
            self.lease.check_journal(&journal)?;self.lease.wait_shared_current(budget,cancelled)?;task_time(budget,cancelled)?;Ok(result)
        }
    }
    struct TaskCarrier {
        packet: Option<ManagedTaskLaunchPacket>, request: Option<managed_launch::ManagedLocalLaunchRequest>,
        native: Option<PathBuf>, budget: Option<FinalizationBudget>, cancelled: Arc<AtomicBool>,
        preparation: managed_launch::ManagedLocalPreparationOwner,
        projection: fabric_runtime::AgentToolsProjectionOwner,
        broker_journal:Option<Journal>, // Staged BEFORE fallible fresh broker handoff.
        broker: crate::agent_broker::BrokerThreadOwner,
        broker_joined: Option<crate::agent_broker::BrokerThreadsSettled>,
        drain: Option<TaskDrain>, ready_owner:Option<PreparedResult>, activation_failure:Option<TaskActivationFailure>,
        admission: Option<ManagedTaskAdmission>, preparation_joined: bool,
        preparation_proof:Option<ManagedPreparationJoined>,scoped_fork:Option<Arc<crate::managed_seed_fork::ManagedSeedForkCell>>,
        unused_budget:Option<FinalizationBudget>,
        panics: Vec<Box<dyn std::any::Any + Send>>,
    }
    struct TaskDrain { live: Live, runtime: Option<fabric_runtime::ManagedRuntimeDrain>, pty:Option<ManagedPtyDrainOwner>,
        _start:Option<Arc<RuntimeTaskStart>>,
        exit: Option<SuccessfulManagedHelperExit>,custody:Option<ManagedHelperCustody>,
        control_join:Option<rudder_engine::execution::handoff::WorkerJoined>,
        handoff_join:Option<rudder_engine::execution::handoff::WorkerJoined>,
        runtime_join:Option<fabric_runtime::ManagedRuntimeJoined> }
    struct TaskActivationFailure { ready:PreparedResult, activation:ManagedPtyActivationOwner,
        _panic:Option<Box<dyn std::any::Any+Send>> }
    enum TaskPayload { Live(TaskDrain), ReadyOwner(PreparedResult), ActivationFailure(TaskActivationFailure) }
    /// Original completion/custody for the separate checkpoint acceptor. No
    /// Serde/Clone/public constructor; callers keep/dispose it OFF-App only.
    /// Field order closes actual actor/prepared resources, then view/cache/CAS
    /// retention, and only then the final owner-bound Journal/Engine lease.
    #[must_use="retain original joined custody through checkpoint acceptance or explicit discard"]
    pub(crate) struct JoinedManagedTask { drain:TaskDrain,
        _broker:crate::agent_broker::BrokerThreadOwner,_broker_join:crate::agent_broker::BrokerThreadsSettled,
        _projection:fabric_runtime::AgentToolsProjectionOwner,
        preparation:managed_launch::ManagedLocalPreparationOwner,_project:Arc<ManagedTaskJournalOwner>,
        _slot:Arc<TaskSlot>,settled:bool }
    impl JoinedManagedTask {
        pub(crate) fn prepared(&self)->&PreparedManagedLaunch { &self.drain.live.prepared }
        pub(crate) fn shutdown(&self)->&ValidatedManagedShutdown { self.drain.live.receipt.as_ref().unwrap() }
        pub(crate) fn helper_exit(&self)->&SuccessfulManagedHelperExit { self.drain.exit.as_ref().unwrap() }
        pub(crate) fn helper_custody(&self)->&ManagedHelperCustody { self.drain.custody.as_ref().unwrap() }
        /// OFF App. Original stopped run identity remains in the genuine
        /// capsule; this checks the SAME held Journal/project owner epoch. A
        /// later run attempt is not authority to rewrite this old private fork.
        pub(crate) fn check_seed_localization_owner(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
            self._project.check_original_cleanup_custody(budget,cancelled)
        }

        /// Acceptance and immutable checkpoint pinning remain separate. Call
        /// only AFTER they used this original custody, or on explicit discard.
        /// Every cleanup error leaves this SAME mutable capsule owned by caller.
        pub(crate) fn settle_discard_off_app(&mut self,budget:&FinalizationBudget)->Result<()> {
            anyhow::ensure!(!self.settled,"joined task capsule already settled");
            budget.remaining()?;self.preparation.settle_after_runtime_join()?;budget.remaining()?;
            self.settled=true;self._slot.acceptance_pending.store(false,Ordering::Release);
            self._slot.work.store(true,Ordering::Release);Ok(())
        }
        pub(crate) fn is_settled(&self)->bool { self.settled }
    }
    /// This counter is released only after join includes the original TLS.
    struct RetainedTaskWorker { worker:thread::JoinHandle<()>, permit:WorkerPermit }
    impl RetainedTaskWorker {
        fn join(self)->thread::Result<()> {
            let Self {worker,permit}=self;
            let result=worker.join();drop(permit);result
        }
    }
    struct TaskSlot { occupied: AtomicBool, acceptance_pending:AtomicBool, generation:AtomicU64, may_have_helper:AtomicBool, activation_committed:AtomicBool, state: AtomicU8, work: AtomicBool, busy: AtomicBool,
        handles: AtomicUsize, carrier: Mutex<TaskCarrier>,
        joining:Mutex<TaskWorkerJoins>,
        workers: [SyncSender<RetainedTaskWorker>; TASK_WORKERS],
        worker_receivers: Mutex<[Receiver<RetainedTaskWorker>; TASK_WORKERS]>,
        drain_send: SyncSender<TaskPayload>, drain_receive: Mutex<Receiver<TaskPayload>>,
        project: Arc<ManagedTaskJournalOwner>, wake: Mutex<Option<PtyOutputWaker>> }
    struct TaskWorkerJoins { worker:[Option<RetainedTaskWorker>;TASK_WORKERS],
        preparation_joined:bool,preparation_proof:Option<ManagedPreparationJoined>,panics:Vec<Box<dyn std::any::Any+Send>> }
    #[derive(Clone)]
    pub(crate) struct TaskHandle { slot:Arc<TaskSlot>, generation:u64,
        budget:FinalizationBudget,cancelled:Arc<AtomicBool> }
    struct TaskWorkerTicket { slot: Arc<TaskSlot>, index: usize, sent: bool }
    impl TaskWorkerTicket {
        fn retain(mut self, worker: RetainedTaskWorker) {
            // Each non-Clone ticket exclusively owns one still-empty bounded
            // channel; only its off-App receiver can remove the original handle.
            self.slot.workers[self.index].try_send(worker).unwrap_or_else(|_| unreachable!("reserved task worker cell unavailable"));
            self.sent = true; self.slot.work.store(true, Ordering::Release);
        }
    }
    impl Drop for TaskWorkerTicket { fn drop(&mut self) {
        if !self.sent { self.slot.handles.fetch_and(!(1 << self.index), Ordering::AcqRel); }
    } }
    impl TaskHandle {
        fn current(&self) -> bool { self.slot.occupied.load(Ordering::Acquire)
            && self.slot.generation.load(Ordering::Acquire)==self.generation }
        fn time(&self) -> Result<()> { anyhow::ensure!(self.current(),"managed task lifetime replaced");task_time(&self.budget,&self.cancelled) }
        fn worker(&self) -> Result<TaskWorkerTicket> {
            anyhow::ensure!(self.current(),"managed task retention generation was replaced");
            for index in 0..TASK_WORKERS {
                let mask = 1 << index; let old = self.slot.handles.fetch_or(mask, Ordering::AcqRel);
                if old & mask == 0 { return Ok(TaskWorkerTicket { slot: self.slot.clone(), index, sent: false }); }
            }
            anyhow::bail!("managed task worker settlement is full")
        }
        fn unused(&self) { if !self.current() { return; }
            if self.slot.state.load(Ordering::Acquire)==TASK_SETTLED { return; }
            self.slot.state.store(if self.slot.may_have_helper.load(Ordering::Acquire) { TASK_RECOVERY } else { TASK_UNUSED }, Ordering::Release);
            self.slot.work.store(true, Ordering::Release); }
        fn adopted(&self) { if !self.current() { return; } self.slot.may_have_helper.store(true,Ordering::Release);
            self.slot.state.store(TASK_ADOPTED, Ordering::Release); self.slot.work.store(true, Ordering::Release); }
        fn activated(&self) { if !self.current() { return; }
            self.slot.activation_committed.store(true,Ordering::Release);self.slot.work.store(true,Ordering::Release); }
        fn reject_activation(&self,ready:PreparedResult,activation:ManagedPtyActivationOwner,panic:Option<Box<dyn std::any::Any+Send>>) {
            assert!(self.current(),"actual activation owner cannot target a reused cell");
            self.slot.drain_send.try_send(TaskPayload::ActivationFailure(TaskActivationFailure { ready,activation,_panic:panic }))
                .unwrap_or_else(|_|unreachable!("reserved task activation cell unavailable"));
            self.unused();
        }
        fn retain_drain(&self, drain: TaskDrain) {
            assert!(self.current(),"actual managed task drain cannot target a reused slot");
            self.slot.drain_send.try_send(TaskPayload::Live(drain)).unwrap_or_else(|_| unreachable!("reserved task drain cell unavailable"));
            self.slot.state.store(TASK_DRAINING, Ordering::Release); self.slot.work.store(true, Ordering::Release);
        }
        fn reject_ready(&self,ready:PreparedResult) {
            assert!(self.current(),"actual task ready owner cannot target a reused cell");
            self.slot.drain_send.try_send(TaskPayload::ReadyOwner(ready)).unwrap_or_else(|_|unreachable!("reserved unused task cell unavailable"));
            self.unused();
        }
    }
    #[must_use = "return the actual rejected owner to bounded OFF-App settlement"]
    pub(crate) enum ManagedTaskStartFailure {
        Prepared(PreparedManagedTask,anyhow::Error), Packet(ManagedTaskLaunchPacket,anyhow::Error),
        Retained(TaskHandle,anyhow::Error),
    }
    impl ManagedTaskStartFailure {
        pub(crate) fn settle_unqueued_off_app(self) -> Result<()> {
            match self { Self::Prepared(owner,_)=>drop(owner),Self::Packet(owner,_)=>drop(owner),
                Self::Retained(handle,_)=>{ handle.unused(); anyhow::bail!("queued task ownership remains in its lifetime supervisor"); } }
            Ok(())
        }
    }
    /// Cleanup custody only. Root constructs/retains this OFF-App with an actual
    /// owner-bound writer/lease and keeps it until every task and pool joins.
    /// Live storage slots are separate from the two charged prep admissions.
    pub(crate) struct ManagedTaskLifetimeSupervisor { slots: Vec<Arc<TaskSlot>>, project: Arc<ManagedTaskJournalOwner>,
        send: Option<SyncSender<Arc<TaskSlot>>>, workers: Vec<thread::JoinHandle<()>> }
    impl ManagedTaskLifetimeSupervisor {
        pub(crate) fn new_off_app(lease: EngineSourceLease, writer: crate::managed_source_app::SourceJournalFactory) -> Result<Arc<Self>> {
            let journal = writer()?; lease.check_journal(&journal)?;
            let project = Arc::new(ManagedTaskJournalOwner { journal: Mutex::new(journal), lease });
            let slots = (0..TASK_SLOTS).map(|_| {
                let channels: [(SyncSender<RetainedTaskWorker>,Receiver<RetainedTaskWorker>);TASK_WORKERS] = std::array::from_fn(|_|mpsc::sync_channel(1));
                let (workers, receivers): (Vec<_>, Vec<_>) = channels.into_iter().unzip();
                let (drain_send, drain_receive) = mpsc::sync_channel(1);
                Arc::new(TaskSlot { occupied: AtomicBool::new(false),acceptance_pending:AtomicBool::new(false), generation:AtomicU64::new(0),may_have_helper:AtomicBool::new(false),activation_committed:AtomicBool::new(false),state: AtomicU8::new(TASK_PENDING),
                    work: AtomicBool::new(false), busy: AtomicBool::new(false), handles: AtomicUsize::new(0),
                    carrier: Mutex::new(TaskCarrier { packet:None,request:None,native:None,budget:None,cancelled:Arc::new(AtomicBool::new(false)),
                        preparation:Default::default(),projection:fabric_runtime::AgentToolsProjectionOwner::new(),
                        broker_journal:None,broker:crate::agent_broker::BrokerThreadOwner::new(),broker_joined:None,drain:None,ready_owner:None,activation_failure:None,
                        admission:None,preparation_joined:false,preparation_proof:None,scoped_fork:None,unused_budget:None,panics:Vec::new() }),
                    joining:Mutex::new(TaskWorkerJoins { worker:std::array::from_fn(|_|None),preparation_joined:false,preparation_proof:None,panics:Vec::new() }),
                    workers:workers.try_into().ok().unwrap(),worker_receivers:Mutex::new(receivers.try_into().ok().unwrap()),
                    drain_send,drain_receive:Mutex::new(drain_receive),project:project.clone(),wake:Mutex::new(None) })
            }).collect();
            let (send, receive) = mpsc::sync_channel::<Arc<TaskSlot>>(MAX_PREPARATIONS);
            let receive = Arc::new(Mutex::new(receive)); let mut workers = Vec::new();
            for _ in 0..MAX_PREPARATIONS {
                let receive = receive.clone();
                let started=thread::Builder::new().name("rudder-managed-task-settle".into()).spawn(move ||loop {
                    let next = receive.lock().unwrap_or_else(|p|p.into_inner()).recv(); let Ok(slot) = next else { break; };
                    // Actual carrier remains in its pre-reserved cell outside
                    // this catch, including panic/error and failed event delivery.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||settle_task_slot(&slot)));
                    if !matches!(result, Ok(Ok(()))) { slot.state.store(TASK_RECOVERY, Ordering::Release); }
                    if let Err(payload) = result { slot.carrier.lock().unwrap_or_else(|p|p.into_inner()).panics.push(payload); }
                    slot.busy.store(false, Ordering::Release);
                    let wake = slot.wake.lock().unwrap_or_else(|p|p.into_inner()).clone();
                    if let Some(wake) = wake { wake(); }
                });
                match started { Ok(worker)=>workers.push(worker),Err(error)=>{
                    drop(send);for worker in workers { let _=worker.join(); }return Err(error.into());
                } }
            }
            Ok(Arc::new(Self { slots, project, send:Some(send), workers }))
        }
        fn reserve(&self, packet: ManagedTaskLaunchPacket, wake:Option<PtyOutputWaker>)
            -> std::result::Result<TaskHandle,(ManagedTaskLaunchPacket,anyhow::Error)> {
            for slot in &self.slots {
                // A stale result/retained error owns a real Arc. Do not recycle
                // its cell until every old handle and pool job has retired.
                if Arc::strong_count(slot)!=1 { continue; }
                if slot.occupied.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() { continue; }
                let Ok(mut carrier) = slot.carrier.try_lock() else { slot.occupied.store(false,Ordering::Release); continue; };
                let Ok(mut wake_cell) = slot.wake.try_lock() else { slot.occupied.store(false,Ordering::Release); continue; };
                let generation = match slot.generation.fetch_update(Ordering::AcqRel,Ordering::Acquire,|old|old.checked_add(1)) {
                    Ok(old)=>old+1,Err(_)=>{ slot.occupied.store(false,Ordering::Release); continue; } };
                slot.state.store(TASK_PENDING,Ordering::Release);slot.may_have_helper.store(false,Ordering::Release);slot.activation_committed.store(false,Ordering::Release);
                let budget=packet.budget.clone();let cancelled=packet.cancelled.clone();
                carrier.scoped_fork=packet.seed_fork.clone();carrier.budget=Some(budget.clone());carrier.cancelled = cancelled.clone(); carrier.packet = Some(packet);
                *wake_cell = wake;
                return Ok(TaskHandle { slot:slot.clone(), generation,budget,cancelled });
            }
            Err((packet,anyhow::anyhow!("managed task lifetime storage is full")))
        }
        pub(crate) fn poll(&self) {
            for slot in &self.slots {
                if slot.occupied.load(Ordering::Acquire) && slot.state.load(Ordering::Acquire)==TASK_SETTLED
                    && Arc::strong_count(slot)==1 { slot.work.store(true,Ordering::Release); }
                if !slot.occupied.load(Ordering::Acquire) || !slot.work.load(Ordering::Acquire)
                    || slot.busy.compare_exchange(false,true,Ordering::AcqRel,Ordering::Acquire).is_err() { continue; }
                slot.work.store(false,Ordering::Release);
                if self.send.as_ref().unwrap().try_send(slot.clone()).is_err() {
                    slot.work.store(true,Ordering::Release); slot.busy.store(false,Ordering::Release);
                }
            }
        }
        pub(crate) fn joined_fences_off_app(&self)->Vec<ManagedLaunchFence> {
            self.slots.iter().filter(|slot|slot.state.load(Ordering::Acquire)==TASK_JOINED)
                .filter_map(|slot|slot.carrier.try_lock().ok().and_then(|carrier|carrier.drain.as_ref()
                    .map(|drain|drain.live.prepared.assignment.fence.clone()))).collect()
        }
        pub(crate) fn take_joined_off_app(&self,fence:&ManagedLaunchFence)->Result<Option<JoinedManagedTask>> {
            for slot in &self.slots {
                if slot.state.load(Ordering::Acquire)!=TASK_JOINED { continue; }
                let Ok(mut carrier)=slot.carrier.try_lock() else { continue; };
                if carrier.drain.as_ref().is_none_or(|drain|drain.live.prepared.assignment.fence!=*fence) { continue; }
                anyhow::ensure!(carrier.broker_joined.is_some(),"actual task broker join proof absent");
                anyhow::ensure!(carrier.broker_journal.is_none(),"original broker connection was not transferred; retain recovery");
                anyhow::ensure!(!slot.acceptance_pending.swap(true,Ordering::AcqRel),"task acceptance already transferred");
                let joined=JoinedManagedTask { drain:carrier.drain.take().unwrap(),
                    _broker:std::mem::replace(&mut carrier.broker,crate::agent_broker::BrokerThreadOwner::new()),
                    _broker_join:carrier.broker_joined.take().unwrap(),
                    _projection:std::mem::replace(&mut carrier.projection,fabric_runtime::AgentToolsProjectionOwner::new()),
                    preparation:std::mem::take(&mut carrier.preparation),_project:slot.project.clone(),_slot:slot.clone(),settled:false };
                carrier.native=None;carrier.request=None;carrier.packet=None;carrier.broker_joined=None;
                carrier.broker=crate::agent_broker::BrokerThreadOwner::new();carrier.projection=fabric_runtime::AgentToolsProjectionOwner::new();
                carrier.preparation_joined=false;carrier.preparation_proof=None;carrier.scoped_fork=None;carrier.unused_budget=None;carrier.panics.clear();
                slot.state.store(TASK_SETTLED,Ordering::Release);slot.work.store(true,Ordering::Release);return Ok(Some(joined));
            }
            Ok(None)
        }
        pub(crate) fn retry_recovery_off_app(&self)->usize {
            self.slots.iter().filter(|slot|slot.occupied.load(Ordering::Acquire)
                && slot.state.load(Ordering::Acquire)==TASK_RECOVERY).map(|slot|{
                    slot.work.store(true,Ordering::Release);1 }).sum()
        }
        pub(crate) fn shutdown_ready(&self) -> bool { self.slots.iter().all(|slot|!slot.occupied.load(Ordering::Acquire)
            && !slot.acceptance_pending.load(Ordering::Acquire) && !slot.busy.load(Ordering::Acquire) && Arc::strong_count(slot)==1) }
        /// Root first detaches its App Arc. Refusal returns this SAME actual
        /// supervisor; Engine ownership cannot be released while it is occupied.
        pub(crate) fn stop_off_app(owner:Arc<Self>) -> std::result::Result<(),(Arc<Self>,anyhow::Error)> {
            if !owner.shutdown_ready() { return Err((owner,anyhow::anyhow!("managed task resources still require recovery"))); }
            let mut owner = match Arc::try_unwrap(owner) { Ok(owner)=>owner,Err(owner)=>return Err((owner,anyhow::anyhow!("managed task supervisor still has App owners"))) };
            drop(owner.send.take()); for worker in owner.workers.drain(..) { let _ = worker.join(); } Ok(())
        }
    }
    fn settle_task_slot(slot:&Arc<TaskSlot>) -> Result<()> {
        // A dropped/unknown acceptance capsule cannot convert into cleanup ACK.
        // Its original checkpoint/resources still require explicit recovery.
        if slot.acceptance_pending.load(Ordering::Acquire) { return Ok(()); }

        // Do not hold the preparation carrier mutex while joining its callback;
        // the actual callback still needs that carrier until it returns.
        let mut joining=slot.joining.lock().unwrap_or_else(|p|p.into_inner());
        let receivers = slot.worker_receivers.lock().unwrap_or_else(|p|p.into_inner());
        for index in 0..TASK_WORKERS {
            if joining.worker[index].is_none() { joining.worker[index] = receivers[index].try_recv().ok(); }
            if let Some(worker) = joining.worker[index].take() {
                // OFF-App actual join, including TLS; no timeout/is_finished
                // shortcut can release either charged preparation admission.
                let actual_thread=worker.worker.thread().clone();
                let result=worker.join();
                if index == 0 { joining.preparation_joined = true;
                    if result.is_ok()&&joining.preparation_proof.is_none() {joining.preparation_proof=Some(ManagedPreparationJoined {_thread:actual_thread});}
                }
                slot.handles.fetch_and(!(1 << index),Ordering::AcqRel);
                if let Err(payload) = result { joining.panics.push(payload); anyhow::bail!("managed task worker panicked"); }
            }
        }
        drop(receivers);let preparation_joined=joining.preparation_joined;let preparation_proof=joining.preparation_proof.take();drop(joining);
        let mut carrier = slot.carrier.lock().unwrap_or_else(|p|p.into_inner());
        carrier.preparation_joined |= preparation_joined;
        if preparation_proof.is_some(){carrier.preparation_proof=preparation_proof;}
        if let Ok(payload) = slot.drain_receive.lock().unwrap_or_else(|p|p.into_inner()).try_recv() {
            match payload { TaskPayload::Live(drain)=>carrier.drain=Some(drain),TaskPayload::ReadyOwner(ready)=>carrier.ready_owner=Some(ready),
                TaskPayload::ActivationFailure(failure)=>carrier.activation_failure=Some(failure) }
        }
        let state = slot.state.load(Ordering::Acquire);
        if matches!(state,TASK_JOINED|TASK_NEVER_STARTED) { return Ok(()); }
        if state==TASK_SETTLED {
            carrier.ready_owner=None;
            // Keep the original budget's actual clock owner off-App until no
            // stale App/result handle can become its last owner on App.
            if Arc::strong_count(slot)>2 { return Ok(()); }
            carrier.budget=None;carrier.cancelled=Arc::new(AtomicBool::new(false));
            *slot.wake.lock().unwrap_or_else(|p|p.into_inner())=None;
            slot.occupied.store(false,Ordering::Release);return Ok(());
        }
        // Successful adoption separates preparation admission from live view
        // lifetime. A failed/in-flight activation retains its admission until
        // original child/reader/control and unused cleanup actually settle.
        if slot.activation_committed.load(Ordering::Acquire) && carrier.preparation_joined { drop(carrier.admission.take()); }
        if state == TASK_PENDING { return Ok(()); }
        let budget=if let Some(fork)=&carrier.scoped_fork {
            if carrier.drain.is_none()&&carrier.activation_failure.is_none() {
                // No cleanup budget is minted while the original task gate is
                // open. The caller first seals actual unspent admission permits.
                let Some(original)=fork.start_gate().cleanup_budget() else {return Ok(());};
                carrier.unused_budget=Some(original.clone());original
            } else {FinalizationBudget::begin(STOP_DEADLINE)?}
        } else {FinalizationBudget::begin(STOP_DEADLINE)?}; // only scoped unused cleanup uses its original captured budget
        if let Some(failure) = &mut carrier.activation_failure {
            failure.activation.settle_unused_off_app(&budget)?;
            anyhow::ensure!(failure.activation.is_settled(),"actual PTY activation cleanup remains uncertain");
            // This proof requires no descriptor worker/possible backend launch,
            // actual child reaping and original reader TLS join, not a timer.
            slot.may_have_helper.store(false,Ordering::Release);
        }
        if let Some(drain) = &mut carrier.drain {
            drain.live.control.close(); drain.live.handoff.close();
            let pty=drain.pty.as_mut().context("original managed PTY cleanup custody was not transferred")?;
            pty.settle_off_app(&budget)?;anyhow::ensure!(pty.is_settled(),"original managed PTY/reader remains unsettled");
            if drain.control_join.is_none() { drain.control_join=Some(drain.live.control.await_closed(budget.remaining()?)?); } budget.remaining()?;
            if drain.handoff_join.is_none() { drain.handoff_join=Some(drain.live.handoff.await_closed(budget.remaining()?)?); } budget.remaining()?;
            anyhow::ensure!(drain.control_join.as_ref().unwrap().same_worker(drain.handoff_join.as_ref().unwrap()), "managed actor join carriers differ");
            let runtime = drain.runtime.as_mut().context("exact managed runtime owner was not transferred")?;
            if drain.runtime_join.is_none() { drain.runtime_join=Some(runtime.await_closed(budget.remaining()?)?); } budget.remaining()?;
            anyhow::ensure!(drain.exit.is_some() && drain.custody.is_some() && drain.live.receipt.is_some(), "actual successful helper exit/shutdown absent; owners retained");
            drain.exit.as_ref().unwrap().matches_checkpoint(drain.custody.as_ref().unwrap(),drain.live.receipt.as_ref().unwrap())?;
        } else if slot.may_have_helper.load(Ordering::Acquire) { return Ok(()); }
        anyhow::ensure!(slot.handles.load(Ordering::Acquire)==0,"managed task callbacks are not actually joined");
        carrier.broker.close();
        if carrier.broker_joined.is_none() { carrier.broker_joined = Some(carrier.broker.await_closed(budget.remaining()?)?); }
        let mut journal = slot.project.journal.lock().map_err(|_|anyhow::anyhow!("managed task writer poisoned"))?;
        slot.project.lease.check_journal(&journal)?;
        carrier.projection.settle_unused(&mut journal, &budget)?;
        anyhow::ensure!(carrier.projection.is_settled(), "managed task grant settlement remains uncertain");
        drop(journal); budget.remaining()?;
        if carrier.drain.is_some() { slot.state.store(TASK_JOINED,Ordering::Release);return Ok(()); }
        if let Some(fork)=&carrier.scoped_fork {
            // A scoped task may still own durable shared alternates/history.
            // Keep its WHOLE original carrier, even before the caller seals it.
            // Neither missing Child nor may_have_helper issues this receipt.
            if let Some(seal)=fork.start_gate().original_seal() {
                seal.check_original(fork.start_gate())?;
                anyhow::ensure!(carrier.preparation_proof.is_some()&&carrier.panics.is_empty(),"original preparation/TLS join receipt absent or callback panicked");
                slot.state.store(TASK_NEVER_STARTED,Ordering::Release);
            }
            return Ok(());
        }
        // No API worker/thread remains; a never-transferred original writer is
        // disposed only on this existing off-App unused-settlement path.
        carrier.broker_journal=None;
        carrier.preparation.settle_unused()?;
        carrier.preparation=Default::default();
        carrier.drain = None; carrier.ready_owner=None;carrier.activation_failure=None;carrier.request = None; carrier.packet = None; carrier.native = None;
        carrier.broker_joined = None; carrier.panics.clear();
        drop(carrier.admission.take());
        carrier.broker=crate::agent_broker::BrokerThreadOwner::new();carrier.projection=fabric_runtime::AgentToolsProjectionOwner::new();
        carrier.preparation_joined=false;carrier.preparation_proof=None;carrier.unused_budget=None;
        let mut joining=slot.joining.lock().unwrap_or_else(|p|p.into_inner());joining.panics.clear();joining.preparation_joined=false;joining.preparation_proof=None;
        slot.state.store(TASK_SETTLED,Ordering::Release); Ok(())
    }
    fn task_time(budget:&FinalizationBudget,cancelled:&AtomicBool) -> Result<()> {
        anyhow::ensure!(!cancelled.load(Ordering::Acquire),"managed task cancelled"); budget.remaining()?; Ok(())
    }
    fn prepare_task_worker(task:&TaskHandle,fence:&ManagedLaunchFence,location:&crate::managed_store::ManagedRunLocation,
        reservation:&ManagedRunSource,projection_record:&serde_json::Value,events:SyncSender<WorkerResult>,wake:Option<PtyOutputWaker>) -> Result<PreparedResult> {
        // This actual carrier outlives the worker catch and is never locked by
        // App after dispatch. Every error leaves its cleanup-bearing fields.
        let mut carrier=task.slot.carrier.lock().unwrap_or_else(|p|p.into_inner());
        let budget=carrier.budget.as_ref().context("original task budget absent")?.clone();
        let cancelled=carrier.cancelled.clone();task_time(&budget,&cancelled)?;task.slot.project.lease.wait_shared_current(&budget,&cancelled)?;
        let mut worker_journal=task.slot.project.lease.open_source_writer()?;task_time(&budget,&cancelled)?;
        location.persist_reservation(reservation)?;location.write_projection(reservation,projection_record)?;
        task_time(&budget,&cancelled)?;
        let request=carrier.request.take().context("managed task request absent")?;
        let home=request.home.clone();let journal_path=request.journal.clone();let backend=request.backend.backend();
        let prepared=managed_launch::prepare_local_task_retained(request,cancelled.clone(),&worker_journal,&budget,&mut carrier.preparation)?;
        task_time(&budget,&cancelled)?;location.persist_assignment(&prepared.assignment)?;task_time(&budget,&cancelled)?;
        let (proceed,receive)=mpsc::sync_channel(1);
        events.send(WorkerResult { fence:fence.clone(),result:Ok(WorkerStep::TaskWorkspace { prepared,proceed,task:task.clone() }) })
            .map_err(|_|anyhow::anyhow!("managed App retired before task workspace adoption"))?;
        if let Some(wake)=wake { wake(); }
        let prepared=receive.recv_timeout(budget.remaining()?).context("original managed task assignment deadline")?;
        task_time(&budget,&cancelled)?;
        task.slot.project.lease.check_journal(&worker_journal)?;task.slot.project.lease.wait_shared_current(&budget,&cancelled)?;
        let projection=fabric_runtime::prepare_agent_tools_for_location_retaining(&mut carrier.projection,&mut worker_journal,backend,&home,
            fence,Some(&prepared.assignment),location,&budget,&cancelled)?;
        task_time(&budget,&cancelled)?;task.slot.project.lease.check_journal(&worker_journal)?;task.slot.project.lease.wait_shared_current(&budget,&cancelled)?;
        let projection_owner=carrier.projection.share(task.slot.project.clone())?;task_time(&budget,&cancelled)?;
        let config=BrokerConfig{journal_path,identity_file:projection.identity_file,socket_path:projection.broker_socket};
        let broker=if task.slot.project.managed_owner_lease().is_some(){
            carrier.broker_journal=Some(worker_journal);
            let TaskCarrier{broker_journal,broker,..}=&mut *carrier;
            AgentBroker::spawn_fresh_retaining(config,task.slot.project.clone(),location.clone(),projection_owner.clone(),
                prepared.assignment.clone(),broker_journal,&budget,&cancelled,broker)?
        }else{AgentBroker::spawn_retaining(config,&mut carrier.broker)?};
        task_time(&budget,&cancelled)?;
        let handoff=prepared.prepare_handoff()?;
        let launch_json=serde_json::to_string(&prepared.link_launch())?;task_time(&budget,&cancelled)?;
        Ok(PreparedResult { prepared,broker:Some(broker),projection_owner:Some(projection_owner),projection_hash:projection.projection_hash,handoff:Some(handoff),
            native_program:carrier.native.take().context("managed task native program absent")?,launch_json })
    }

    struct Pending { fence: ManagedLaunchFence, cancelled: Arc<AtomicBool>, started: Instant,
        budget: Option<FinalizationBudget> }
    struct PreparedResult { prepared: PreparedManagedLaunch, broker: Option<AgentBroker>, projection_hash: [u8; 32],
        projection_owner:Option<fabric_runtime::SharedAgentToolsProjectionOwner>,
        handoff: Option<ManagedExecutionHandoff>, native_program: PathBuf, launch_json: String }
    type BackendProcess = Arc<rudder_engine::execution::handoff::BackendProcessWitness>;
    enum WorkerStep { Workspace { prepared: PreparedManagedLaunch, proceed: SyncSender<PreparedManagedLaunch> }, Ready(PreparedResult), Reports(Result<CandidateReports>),
        TaskWorkspace { prepared:PreparedManagedLaunch, proceed:SyncSender<PreparedManagedLaunch>, task:TaskHandle },
        TaskReady { ready:PreparedResult, task:TaskHandle }, TaskFailed { task:TaskHandle },
        Authenticated { process:BackendProcess, result:std::result::Result<(),agent_link::ManagedCodexAuthError> } }
    struct WorkerResult { fence: ManagedLaunchFence, result: Result<WorkerStep> }
    struct PendingAuth { process:BackendProcess, cancelled:Arc<AtomicBool>, deadline:Instant, running:bool }
    struct Live { prepared: PreparedManagedLaunch, handoff: HandoffReceipt, control: ManagedExecutionHandle,
        receipt: Option<ValidatedManagedShutdown>, stop: Option<(StopAndCheckpoint, Instant)>, uncertain: bool,
        orphan_reported: bool, backend_ready: bool, auth:Option<PendingAuth>, report_pending: bool, last_report: Option<Instant>, report_digest: Option<[u8;32]>,
        task:Option<TaskLive> }
    struct TaskLive { handle:TaskHandle, custody:Option<ManagedHelperCustody>, start:Option<Arc<RuntimeTaskStart>> }
    /// Actual pre-dispatch helper custody for a bounded trusted Root IPC worker.
    /// No Serde/Clone/raw constructor. Retain on failed queue/event delivery;
    /// the original slot alias also keeps Engine ownership through that worker.
    pub(crate) struct PendingManagedTaskHelper {
        fence:ManagedLaunchFence,
        assignment:managed_launch::WorkspaceAssignment,
        managed_run:crate::managed_store::ManagedRunLocation,
        seed_fork:Option<Arc<crate::managed_seed_fork::ManagedSeedForkCell>>,
        helper:Arc<rudder_engine::execution::handoff::helper_process::HelperProcessWitness>,
        task:TaskHandle,
        start:Arc<RuntimeTaskStart>, // SAME task barrier/grant retained through TaskDrain.
    }
    impl PendingManagedTaskHelper {
        pub(crate) fn fence(&self)->&ManagedLaunchFence { &self.fence }
        pub(crate) fn assignment(&self)->&managed_launch::WorkspaceAssignment { &self.assignment }
        pub(crate) fn managed_run(&self)->&crate::managed_store::ManagedRunLocation { &self.managed_run }
        pub(crate) fn journal_owner(&self)->&Arc<ManagedTaskJournalOwner> { &self.task.slot.project }
        pub(crate) fn seed_fork(&self)->Option<&Arc<crate::managed_seed_fork::ManagedSeedForkCell>> {self.seed_fork.as_ref()}
        pub(crate) fn helper(&self)->&Arc<rudder_engine::execution::handoff::helper_process::HelperProcessWitness> { &self.helper }
        pub(crate) fn original_budget(&self)->&FinalizationBudget { &self.task.budget }
        pub(crate) fn cancellation(&self)->&AtomicBool { &self.task.cancelled }
        pub(crate) fn cancellation_owner(&self)->&Arc<AtomicBool> { &self.task.cancelled }
        pub(crate) fn runtime_start(&self)->&Arc<RuntimeTaskStart> { &self.start }
    }
    impl Drop for Live { fn drop(&mut self) { if let Some(auth) = &self.auth { auth.cancelled.store(true, Ordering::Release); } } }
    struct CandidateReports { summary: Option<String>, signal: Option<signals::SignalState>, digest: [u8;32] }

    fn read_reports(paths: &managed_launch::ManagedReportMapping) -> Result<CandidateReports> {
        use std::{fs::{File, OpenOptions}, io::Read, os::{fd::{AsRawFd, FromRawFd}, unix::fs::OpenOptionsExt}, ffi::CStr};
        use sha2::{Digest, Sha256};
        fn open_at(parent: &File, name: &CStr, flags: i32) -> Result<Option<File>> {
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | flags) };
            if fd < 0 { let error = io::Error::last_os_error(); return if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error.into()) }; }
            Ok(Some(unsafe { File::from_raw_fd(fd) }))
        }
        fn read(file: Option<File>) -> Result<Option<Vec<u8>>> {
            let Some(file) = file else { return Ok(None); };
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file() && metadata.len() <= 64 * 1024, "managed report exceeds regular file bound");
            let mut bytes = Vec::new(); file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 64 * 1024, "managed report exceeds bound"); Ok(Some(bytes))
        }
        let directory = paths.host_done_file.parent().context("managed report directory absent")?;
        anyhow::ensure!(paths.host_signal_file.parent() == Some(directory)
            && directory.file_name().is_some_and(|name|name == ".rudder-reports")
            && paths.host_done_file.file_name().is_some_and(|name|name == "done.json")
            && paths.host_signal_file.file_name().is_some_and(|name|name == "signal.json"), "managed report mapping changed");
        let home = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.parent().context("managed execution home absent")?)?;
        // The guest may rename entries inside its home. Traverse every writable
        // component by held dirfd and refuse symlinks, including the parent dir.
        let Some(directory) = open_at(&home, c".rudder-reports", libc::O_DIRECTORY)? else {
            return Ok(CandidateReports { summary: None, signal: None, digest: Sha256::digest([]).into() });
        };
        let done = read(open_at(&directory, c"done.json", 0)?)?;
        let signal = read(open_at(&directory, c"signal.json", 0)?)?;
        let digest = Sha256::digest(rudder_engine::protocol::canonical_json(&(done.as_deref(), signal.as_deref()))?).into();
        let summary = done.and_then(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value|value.get("summary").and_then(serde_json::Value::as_str).map(str::to_owned))
            .map(|value|value.chars().filter(|ch|!ch.is_control() || matches!(*ch, '\n' | '\t')).take(4096).collect::<String>())
            .map(|value|value.trim().to_owned()).filter(|value|!value.is_empty());
        let signal = signal.and_then(|bytes|String::from_utf8(bytes).ok()).and_then(|body|signals::parse_signal_state(&body));
        Ok(CandidateReports { summary, signal, digest })
    }
    fn apply_reports(run: &mut AgentRun, reports: CandidateReports, attention_allowed: bool) {
        if let Some(summary) = reports.summary { run.done_summary = Some(summary); }
        if !attention_allowed { return; }
        match reports.signal {
            Some(signals::SignalState::Permission) => { run.wait_signal = Some(WaitSignal::Permission); run.needs_permission = true; run.needs_user_input = false; }
            Some(signals::SignalState::Input) => { run.wait_signal = Some(WaitSignal::Input); run.needs_user_input = true; }
            Some(signals::SignalState::Working) => { run.wait_signal = None; run.needs_permission = false; run.needs_user_input = false; }
            // Writable reports cannot complete/fail a model turn or certify a
            // delivered ACK, deployment, readiness or retained checkpoint.
            _ => {}
        }
    }

    pub(crate) struct ManagedAppState { queue: WorkerQueue<WorkerResult>, pending: BTreeMap<String, Pending>, live: BTreeMap<String, Live>,
        task_supervisor:Option<Arc<ManagedTaskLifetimeSupervisor>> }
    impl Default for ManagedAppState { fn default() -> Self { Self { queue: WorkerQueue::new(), pending: BTreeMap::new(), live: BTreeMap::new(),task_supervisor:None } } }
    impl Drop for ManagedAppState { fn drop(&mut self) {
        for pending in self.pending.values() { pending.cancelled.store(true, Ordering::Release); }
        for (_,mut live) in std::mem::take(&mut self.live) {
            if let Some(task)=live.task.take() {
                // Emergency custody transfer is not helper/runtime drain.
                // Core shutdown must normally use the App-aware method below.
                task.handle.retain_drain(TaskDrain { live,runtime:None,pty:None,_start:task.start,exit:None,custody:task.custody,control_join:None,handoff_join:None,runtime_join:None });
            }
        }
        if let Some(supervisor)=&self.task_supervisor { supervisor.poll(); }
    } }
    fn retain_task_live(app:&mut App,mut live:Live,exit:Option<SuccessfulManagedHelperExit>) {
        let Some(task)=live.task.take() else { return; };
        let runtime=app.fabric_runtime.take_managed_drain(&live.prepared.assignment).ok();
        // Move only the original pane. A replacement row or matching saved PID
        // cannot authorize taking another terminal or certify helper drain.
        let pty=app.agents.iter_mut().find(|run|run.id==live.prepared.assignment.fence.run_id).and_then(|run| {
            let custody=task.custody.as_ref()?;
            if !run.terminal.as_ref().is_some_and(|terminal|terminal.matches_managed_helper_custody(custody)) { return None; }
            match ManagedPtyDrainOwner::retire(run.terminal.take()?,custody) {
                Ok(retired)=>Some(retired),Err((pane,_))=>{ run.terminal=Some(pane);None }
            }
        });
        task.handle.retain_drain(TaskDrain { live,runtime,pty,_start:task.start,exit,custody:task.custody,control_join:None,handoff_join:None,runtime_join:None });
    }

    fn still_pending(app: &App, fence: &ManagedLaunchFence, owner: &OwnerFence) -> bool {
        owner == &fence.owner && app.agents.iter().any(|run| run.id == fence.run_id
            && run.terminal.is_none() && run.status == AgentStatus::Running
            && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(fence))
    }
    fn matches_workspace(source: Option<&ManagedRunSource>, assignment: &managed_launch::WorkspaceAssignment, reserved: bool) -> bool {
        match source {
            Some(ManagedRunSource::Reserved { fence, runnable_root }) if reserved => fence == &assignment.fence && runnable_root == &assignment.runnable_root,
            Some(ManagedRunSource::Assigned { assignment: expected }) => expected.as_ref() == assignment,
            _ => false,
        }
    }

    impl App {
        /// App-only observation DATA. It does not issue a process witness,
        /// readiness, checkpoint, join, acceptance or retirement receipt. No
        /// live carrier means unknown, never evidence that a task stopped.
        pub(crate) fn managed_task_ingress_observation(&self,fence:&ManagedLaunchFence)->Option<serde_json::Value> {
            let row=self.agents.iter().find(|run|run.id==fence.run_id)?;
            let row_matches=match row.managed_local.as_ref()? {
                ManagedRunSource::Reserved {fence:actual,..}=>actual==fence,
                ManagedRunSource::Assigned {assignment}=>&assignment.fence==fence,
                _=>false,
            };
            if !row_matches{return None;}
            let live=self.managed_local.live.get(&fence.run_id).filter(|live|live.task.is_some()&&live.prepared.assignment.fence==*fence
                &&matches_workspace(row.managed_local.as_ref(),&live.prepared.assignment,false));
            let preparing=self.managed_local.pending.get(&fence.run_id).is_some_and(|pending|pending.fence==*fence);
            Some(serde_json::json!({"run_id":fence.run_id,"run_epoch":fence.run_epoch.to_string(),"attempt_id":fence.attempt_id,
                "row_status":crate::gitio::run_record_status(row.status),"preparing_observed":preparing,
                "backend_ready_last_observed":live.map(|live|live.backend_ready),"stop_requested_observed":live.map(|live|live.stop.is_some()),
                "uncertain_observed":live.map(|live|live.uncertain),"validated_checkpoint_observed":live.map(|live|live.receipt.is_some())}))
        }
        pub(crate) fn configure_managed_task_supervisor(&mut self, supervisor:Arc<ManagedTaskLifetimeSupervisor>) -> Result<()> {
            anyhow::ensure!(self.managed_local.task_supervisor.is_none(), "managed task lifetime supervisor already configured");
            self.managed_local.task_supervisor = Some(supervisor); Ok(())
        }
        /// Root must use this actual witness (then its held-pidfd exporter),
        /// original budget/cancel and exact assignment for IPC enrollment.
        pub(crate) fn managed_task_pending_helper(&self,fence:&ManagedLaunchFence)->Result<PendingManagedTaskHelper> {
            let live=self.managed_local.live.get(&fence.run_id).context("managed task absent")?;
            anyhow::ensure!(live.prepared.assignment.fence==*fence && !live.uncertain && live.stop.is_none(),"managed task start scope retired");
            let task=live.task.as_ref().context("managed task start custody absent")?;task.handle.time()?;
            let start=task.start.as_ref().context("original pending start barrier absent")?;
            let helper=start.pending_on_app()?;
            anyhow::ensure!(self.agents.iter().any(|run|run.id==fence.run_id && matches_workspace(run.managed_local.as_ref(),&live.prepared.assignment,false)
                && task.custody.as_ref().is_some_and(|custody|run.terminal.as_ref().is_some_and(|pane|pane.matches_managed_helper_custody(custody)))),"original managed task pane replaced");
            Ok(PendingManagedTaskHelper { fence:fence.clone(),assignment:live.prepared.assignment.clone(),
                managed_run:live.prepared.managed_run.clone(),seed_fork:live.prepared.seed_fork.clone(),helper,task:task.handle.clone(),start:Arc::clone(start) })
        }
        /// Trusted Root IPC adapter ONLY: invoke after verifying the actual
        /// original enrollment ACK for this pending helper + exact seed/source.
        /// This hook checks App/owner identity, not the external ACK's contents;
        /// production startup stays gated until Root supplies that typed adapter.
        pub(crate) fn release_managed_task_start_after_enrollment(&mut self,pending:PendingManagedTaskHelper)
            -> std::result::Result<(),(PendingManagedTaskHelper,anyhow::Error)> {
            match self.release_managed_task_start_after_enrollment_retaining(&pending) {
                Ok(())=>Ok(()),Err(error)=>Err((pending,error))
            }
        }
        /// The trusted IPC adapter retains pending custody outside its catch.
        /// No unwind in App may consume that original helper/slot owner.
        pub(crate) fn release_managed_task_start_after_enrollment_retaining(&mut self,pending:&PendingManagedTaskHelper)
            -> Result<()> {
                pending.task.time()?;pending.task.slot.project.lease.check_active_for_adoption()?;
                let live=self.managed_local.live.get_mut(&pending.fence.run_id).context("managed task absent after enrollment")?;
                anyhow::ensure!(live.prepared.assignment.fence==pending.fence && !live.uncertain && live.stop.is_none(),"managed task enrollment result stale");
                let task=live.task.as_mut().context("managed task ownership absent")?;
                anyhow::ensure!(task.handle.generation==pending.task.generation && Arc::ptr_eq(&task.handle.slot,&pending.task.slot),"managed task enrollment owner replaced");
                anyhow::ensure!(self.agents.iter().any(|run|run.id==pending.fence.run_id && matches_workspace(run.managed_local.as_ref(),&live.prepared.assignment,false)
                    && task.custody.as_ref().is_some_and(|custody|run.terminal.as_ref().is_some_and(|pane|pane.matches_managed_helper_custody(custody)))),"managed task enrollment pane replaced");
                let start=task.start.as_ref().context("managed task start barrier absent")?;
                anyhow::ensure!(Arc::ptr_eq(start,&pending.start),"Root enrollment belongs to another original task barrier");
                start.release_after_seed_ack_on_app(&pending.helper)?;task.handle.activated();Ok(())
        }
        pub(crate) fn ensure_managed_task_shutdown_ready(&self) -> Result<()> {
            anyhow::ensure!(self.managed_local.task_supervisor.as_ref().is_none_or(|owner|owner.shutdown_ready()),
                "managed task helper/actor/view ownership remains live or requires recovery"); Ok(())
        }
        pub(crate) fn take_managed_task_supervisor(&mut self) -> Result<Option<Arc<ManagedTaskLifetimeSupervisor>>> {
            self.ensure_managed_task_shutdown_ready()?; Ok(self.managed_local.task_supervisor.take())
        }
        /// App-only emergency handoff before App/RuntimeState destruction. It
        /// requests PTY termination but cannot certify reap, shutdown or joins;
        /// supervisor/Engine shutdown stays blocked until explicit recovery.
        pub(crate) fn retain_managed_task_shutdown(&mut self) {
            let ids:Vec<_>=self.managed_local.live.iter().filter(|(_,live)|live.task.is_some()).map(|(id,_)|id.clone()).collect();
            for id in ids {
                if let Some(run)=self.agents.iter_mut().find(|run|run.id==id) {
                    if let Some(terminal)=run.terminal.as_mut() { terminal.request_termination(); }
                }
                // Keep the live pane/control on App until its actual wait
                // observation. Poll then transfers a retired cleanup capsule.
                if let Some(live)=self.managed_local.live.get_mut(&id) { live.uncertain=true; }
            }
            for pending in self.managed_local.pending.values() { if pending.budget.is_some() { pending.cancelled.store(true,Ordering::Release); } }
            if let Some(supervisor)=&self.managed_local.task_supervisor { supervisor.poll(); }
        }
        pub(crate) fn start_managed_task(&mut self, index:usize, prepared:PreparedManagedTask,
            project:&Arc<ManagedTaskProject>, journal:&mut Journal, owner:&OwnerFence)
            -> std::result::Result<ManagedLaunchFence,ManagedTaskStartFailure> {
            let packet = prepared.adopt_retaining(self,index,project,journal,owner)
                .map_err(|(prepared,error)|ManagedTaskStartFailure::Prepared(prepared,error))?;
            self.start_managed_task_packet(index,packet,journal,owner)
        }
        pub(crate) fn start_managed_task_packet(&mut self, index:usize, packet:ManagedTaskLaunchPacket,
            journal:&mut Journal, owner:&OwnerFence) -> std::result::Result<ManagedLaunchFence,ManagedTaskStartFailure> {
            let validation = (|| -> Result<()> {
                packet.matches_target(self,index)?;
                let input = &packet.inputs;
                anyhow::ensure!(packet.native_program.is_absolute() && self.agents[index].backend == input.backend.backend()
                    && input.journal == FabricPaths::new(&input.home).journal && journal.database_path()? == input.journal
                    && input.managed_store.binding().repository == self.cwd && input.managed_store.binding().home == input.home
                    && input.managed_store.binding().journal == input.journal && input.managed_store.binding().project_id == owner.project_id
                    && input.preparation_parent == input.managed_store.binding().directory.join("preparations")
                    && journal.managed_project(&owner.project_id)?.as_ref() == Some(input.managed_store.receipt()),
                    "managed task selected namespace/backend/program changed");
                let supervisor = self.managed_local.task_supervisor.as_ref().context("managed task needs a retained lifetime supervisor")?;
                anyhow::ensure!(supervisor.project.lease.owner() == owner, "managed task supervisor belongs to another owner");
                supervisor.project.lease.check_active_for_adoption()?;
                Ok(())
            })();
            if let Err(error) = validation { return Err(ManagedTaskStartFailure::Packet(packet,error)); }
            let permit = match self.managed_local.queue.admit() { Ok(permit)=>permit,Err(error)=>return Err(ManagedTaskStartFailure::Packet(packet,error)) };
            if let Some(fork)=&packet.seed_fork {if let Err(error)=fork.start_gate().claim_dispatch(){return Err(ManagedTaskStartFailure::Packet(packet,error));}}
            let task = self.managed_local.task_supervisor.as_ref().unwrap().reserve(packet,self.pty_output_waker.clone())
                .map_err(|(packet,error)|ManagedTaskStartFailure::Packet(packet,error))?;
            let result = (|| -> Result<ManagedLaunchFence> {
                // Fresh, not yet queued cell: try_lock cannot wait on worker IO.
                let mut carrier = task.slot.carrier.try_lock().map_err(|_|anyhow::anyhow!("new managed task cell is busy"))?;
                let packet = carrier.packet.as_ref().unwrap();
                let run_id = self.agents[index].id.clone();
                let attempt_id = uuid::Uuid::new_v4().to_string();
                let run_epoch = journal.observe_run_attempt(owner,&run_id,&attempt_id,now_ms()?)?;
                let fence = ManagedLaunchFence { owner:owner.clone(),run_id:run_id.clone(),run_epoch,attempt_id,
                    request_id:uuid::Uuid::new_v4().to_string() };
                let reservation = ManagedRunSource::Reserved { fence:fence.clone(),runnable_root:packet.inputs.runnable_root.clone() };
                let location = packet.inputs.managed_store.run(&run_id)?;
                let run = &mut self.agents[index]; run.managed_local=Some(reservation.clone());run.managed_store=Some(location.clone());
                run.status=AgentStatus::Running;run.execution_profile=Some(packet.inputs.backend.execution_profile().into());
                run.completed_at=None;run.last_error=Some("preparing explicit managed task".into());
                save_native_run_record(&self.cwd,run)?;
                let projection = serde_json::json!({"id":run_id,"task":run.task,"backend":run.backend.as_str(),"model":run.model,
                    "status":"orphaned","mode":"execute","executionProfile":run.execution_profile});
                let packet = carrier.packet.take().unwrap();
                carrier.budget=Some(packet.budget); carrier.cancelled=packet.cancelled; carrier.admission=Some(packet.retention);
                carrier.native=Some(packet.native_program); carrier.preparation.set_cache(packet.preparation_cache)?;
                carrier.preparation.set_seed_fork(packet.seed_fork)?;
                carrier.request=Some(packet.inputs.bind(fence.clone(),uuid::Uuid::new_v4().to_string(),uuid::Uuid::new_v4().to_string()));
                let budget = carrier.budget.as_ref().unwrap().clone(); let cancelled=carrier.cancelled.clone(); drop(carrier);
                self.managed_local.pending.insert(run_id,Pending { fence:fence.clone(),cancelled,started:Instant::now(),budget:Some(budget) });
                let ticket = task.worker()?; let worker = task.clone(); let failed=task.clone();
                let scope=fence.clone();let panic_scope=fence.clone();let wake=self.pty_output_waker.clone();
                self.managed_local.queue.spawn_retaining(permit,ticket,self.pty_output_waker.clone(),move |events| {
                    let result = prepare_task_worker(&worker,&scope,&location,&reservation,&projection,events,wake);
                    WorkerResult { fence:scope,result:Ok(match result { Ok(ready)=>WorkerStep::TaskReady { ready,task:worker },
                        Err(_)=>WorkerStep::TaskFailed { task:worker } }) }
                },move ||WorkerResult { fence:panic_scope,result:Ok(WorkerStep::TaskFailed { task:failed }) })?;
                Ok(fence)
            })();
            match result { Ok(fence)=>{ self.dirty=true;Ok(fence) },Err(error)=>{
                task.unused(); if let Some(pending)=self.managed_local.pending.get(&self.agents[index].id) { pending.cancelled.store(true,Ordering::Release); }
                orphan(&self.cwd,&mut self.agents[index],"managed task preparation was not queued; retained unused cleanup is pending");
                Err(ManagedTaskStartFailure::Retained(task,error)) } }
        }
        /// Explicit opt-in only. The caller supplies an existing stopped row and
        /// a retained local selection. No default/profile/cwd fallback exists.
        pub(crate) fn start_managed_local(&mut self, index: usize, inputs: ManagedLocalLaunchInputs,
            native_program: PathBuf, journal: &mut Journal, owner: &OwnerFence) -> Result<ManagedLaunchFence> {
            let run = self.agents.get(index).context("managed launch row is absent")?;
            anyhow::ensure!(run.terminal.is_none() && !matches!(run.status, AgentStatus::Running | AgentStatus::Merged | AgentStatus::Migrated)
                && run.managed_local.is_none() && run.session_id.is_none() && !run.plain_process
                && !run.is_main() && !run.is_orchestrator() && !is_cloud_owned_agent(run)
                && run.workspace_path.is_none() && run.jj_change_id.is_none(),
                "managed local launch requires a new explicitly selected row");
            anyhow::ensure!(run.backend == inputs.backend.backend() && native_program.is_absolute(), "managed backend/program selection differs from the row");
            anyhow::ensure!(inputs.journal == FabricPaths::new(&inputs.home).journal,
                "managed launch journal differs from its explicit identity home");
            anyhow::ensure!(journal.database_path()? == inputs.journal,
                "managed launch worker journal differs from App ownership");
            anyhow::ensure!(inputs.managed_store.binding().home == inputs.home
                && inputs.managed_store.binding().journal == inputs.journal
                && inputs.managed_store.binding().repository == self.cwd
                && inputs.managed_store.binding().project_id == owner.project_id
                && journal.managed_project(&owner.project_id)?.as_ref() == Some(inputs.managed_store.receipt())
                && inputs.preparation_parent == inputs.managed_store.binding().directory.join("preparations"),
                "managed launch requires its exact explicitly prepared private project store");
            let location = inputs.managed_store.run(&run.id)?;
            let permit = self.managed_local.queue.admit()?;
            let attempt_id = uuid::Uuid::new_v4().to_string();
            let request_id = uuid::Uuid::new_v4().to_string();
            let operation_id = uuid::Uuid::new_v4().to_string();
            let view_id = uuid::Uuid::new_v4().to_string();
            let run_id = run.id.clone();
            let epoch = journal.observe_run_attempt(owner, &run_id, &attempt_id, now_ms()?)?;
            let fence = ManagedLaunchFence { owner: owner.clone(), run_id: run_id.clone(), run_epoch: epoch, attempt_id, request_id };
            let source = ManagedRunSource::Reserved { fence: fence.clone(), runnable_root: inputs.runnable_root.clone() };
            source.validate(&run_id)?;
            let run = &mut self.agents[index];
            run.managed_local = Some(source); run.status = AgentStatus::Running;
            run.managed_store = Some(location.clone());
            run.execution_profile = Some(inputs.backend.execution_profile().into());
            run.completed_at = None; run.last_error = Some("preparing managed local source".into());
            // This marker contains no credentials and must precede worker effects.
            // Managed persistence performs no Git/history queries.
            if let Err(error) = save_native_run_record(&self.cwd, run) {
                run.status = AgentStatus::Orphaned;
                return Err(error.context("persist managed reservation before preparation"));
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            self.managed_local.pending.insert(run_id.clone(), Pending { fence: fence.clone(), cancelled: cancelled.clone(), started: Instant::now(),budget:None });
            let request = inputs.bind(fence.clone(), operation_id, view_id);
            let worker_fence = fence.clone();
            let reservation = self.agents[index].managed_local.clone().unwrap();
            let reservation_projection = serde_json::json!({"id":run_id,"task":self.agents[index].task,
                "backend":self.agents[index].backend.as_str(),"model":self.agents[index].model,
                "status":"orphaned","mode":"execute","executionProfile":self.agents[index].execution_profile});
            let stage_waker = self.pty_output_waker.clone();
            let panic_fence = worker_fence.clone();
            let spawn = self.managed_local.queue.spawn(permit, self.pty_output_waker.clone(), move |events| {
                let result = (|| -> Result<PreparedResult> {
                    let home = request.home.clone(); let journal_path = request.journal.clone(); let backend = request.backend.backend();
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before durable reservation");
                    location.persist_reservation(&reservation)?;
                    location.write_projection(&reservation, &reservation_projection)?;
                    let prepared = managed_launch::prepare_local(request, cancelled.clone())?;
                    // Immutable private evidence precedes the App's assignment
                    // acknowledgment and every scoped grant/publication.
                    location.persist_assignment(&prepared.assignment)?;
                    let (proceed, receive) = mpsc::sync_channel(1);
                    events.send(WorkerResult { fence: worker_fence.clone(), result: Ok(WorkerStep::Workspace { prepared, proceed }) })
                        .map_err(|_|anyhow::anyhow!("managed App retired before workspace assignment"))?;
                    if let Some(waker) = &stage_waker { waker(); }
                    let prepared = receive.recv_timeout(PREPARATION_DEADLINE).context("managed assignment adoption did not settle")?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before grant");
                    let mut unused_command = TerminalCommand::with_args("", Vec::<String>::new());
                    let projection = fabric_runtime::prepare_agent_tools_for_attempt(&mut unused_command, backend, &home,
                        &worker_fence, Some(&prepared.assignment))?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before broker bind");
                    let broker = AgentBroker::spawn(BrokerConfig { journal_path: journal_path.clone(),
                        identity_file: projection.identity_file, socket_path: projection.broker_socket })?;
                    let handoff = prepared.prepare_handoff()?;
                    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "managed launch cancelled before result");
                    worker_fence.check_current(&Journal::open(&journal_path)?)?;
                    let launch_json = serde_json::to_string(&prepared.link_launch())?;
                    Ok(PreparedResult { prepared, broker:Some(broker),projection_owner:None,projection_hash: projection.projection_hash, handoff:Some(handoff), native_program, launch_json })
                })();
                WorkerResult { fence: worker_fence, result: result.map(WorkerStep::Ready) }
            }, move ||WorkerResult { fence:panic_fence, result:Err(anyhow::anyhow!("managed preparation worker failed without a result")) });
            if let Err(error) = spawn {
                self.managed_local.pending.remove(&run_id);
                let run = &mut self.agents[index]; run.status = AgentStatus::Orphaned;
                run.last_error = Some("managed preparation worker could not start; reservation needs recovery".into());
                let _ = save_native_run_record(&self.cwd, run);
                return Err(error);
            }
            self.dirty = true;
            Ok(fence)
        }
    }

    pub(crate) fn poll(app: &mut App, journal: &mut Journal, owner: &OwnerFence) -> Result<()> {
        let mut state = std::mem::take(&mut app.managed_local);
        let result = state.poll(app, journal, owner);
        app.managed_local = state; result
    }
    /// Task-only concrete restriction from the SAME actual machine startup
    /// slot. Legacy callers keep poll(); live work/receipts/reaping always run.
    pub(crate) fn poll_with_startup_gate(app:&mut App,journal:&mut Journal,owner:&OwnerFence,
        gate:&crate::managed_machine_entry::ManagedMachineStartupGate)->Result<()> {
        let mut state=std::mem::take(&mut app.managed_local);
        let result=state.poll_gated(app,journal,owner,Some(gate));app.managed_local=state;result
    }
    impl ManagedAppState {
        fn poll(&mut self,app:&mut App,journal:&mut Journal,owner:&OwnerFence)->Result<()> {self.poll_gated(app,journal,owner,None)}
        fn poll_gated(&mut self, app: &mut App, journal: &mut Journal, owner: &OwnerFence,
            startup:Option<&crate::managed_machine_entry::ManagedMachineStartupGate>) -> Result<()> {
            if let Some(supervisor)=&self.task_supervisor { supervisor.poll(); }
            for pending in self.pending.values() {
                if pending.budget.as_ref().map_or_else(||pending.started.elapsed() >= PREPARATION_DEADLINE,|budget|budget.remaining().is_err())
                    || !still_pending(app, &pending.fence, owner)
                    || pending.fence.check_current(journal).is_err() {
                    pending.cancelled.store(true, Ordering::Release);
                    if let Some(run) = app.agents.iter_mut().find(|run|run.id == pending.fence.run_id
                        && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(&pending.fence)
                        && run.status == AgentStatus::Running) {
                        run.status = AgentStatus::Orphaned; run.last_error = Some("managed preparation cancelled/stale/timed out; no replacement launched".into());
                        let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
                    }
                }
            }
            while let Ok(result) = self.queue.receive.try_recv() {
                let (result, task)=match result {
                    WorkerResult { fence,result:Ok(WorkerStep::TaskWorkspace { prepared,proceed,task }) }=>
                        (WorkerResult { fence,result:Ok(WorkerStep::Workspace { prepared,proceed }) },Some(task)),
                    WorkerResult { fence,result:Ok(WorkerStep::TaskReady { ready,task }) }=>
                        (WorkerResult { fence,result:Ok(WorkerStep::Ready(ready)) },Some(task)),
                    WorkerResult { fence,result:Ok(WorkerStep::TaskFailed { task }) }=>{
                        task.unused();(WorkerResult { fence,result:Err(anyhow::anyhow!("explicit managed task preparation failed; unused settlement remains owned")) },Some(task)) },
                    result=>(result,None),
                };
                if let Ok(WorkerStep::Authenticated { process, result:authenticated }) = result.result {
                    if let Some(live) = self.live.get_mut(&result.fence.run_id).filter(|live|live.prepared.assignment.fence == result.fence) {
                        if live.stop.is_some() || live.uncertain { continue; }
                        let current = !live.uncertain && live.stop.is_none() && result.fence.owner == *owner
                            && result.fence.check_current(journal).is_ok() && live.auth.as_ref().is_some_and(|auth|
                                auth.running && Arc::ptr_eq(&auth.process, &process) && !auth.cancelled.load(Ordering::Acquire) && Instant::now() < auth.deadline);
                        if current && authenticated.is_ok() {
                            if let Some(index) = app.agents.iter().position(|run|run.id == result.fence.run_id
                                && matches_workspace(run.managed_local.as_ref(), &live.prepared.assignment, false)) {
                                let expected = app.agents[index].session_id.clone();
                                let attached = app.agents[index].terminal.as_mut().context("managed authenticated terminal absent").and_then(|terminal|
                                    app.fabric_runtime.attach_managed_backend(owner, &live.prepared.assignment, terminal, live.prepared.link_launch(),
                                        live.prepared.transport.guest_cwd.clone(), expected, process));
                                if attached.is_ok() && live.control.backend_authenticated().is_ok() { live.backend_ready = true; live.auth = None; }
                                else { live.uncertain = true; }
                            } else { live.uncertain = true; }
                        } else { live.uncertain = true; }
                    }
                    continue;
                }
                if let Ok(WorkerStep::Reports(reports)) = result.result {
                    if let Some(live) = self.live.get_mut(&result.fence.run_id).filter(|live|live.prepared.assignment.fence == result.fence) { live.report_pending = false; }
                    if result.fence.owner == *owner && result.fence.check_current(journal).is_ok() {
                        if let (Ok(reports), Some(run)) = (reports, app.agents.iter_mut().find(|run|run.id == result.fence.run_id
                            && run.managed_local.as_ref().and_then(ManagedRunSource::fence) == Some(&result.fence))) {
                            let changed = self.live.get(&result.fence.run_id).is_none_or(|live|live.report_digest != Some(reports.digest));
                            if changed {
                                if let Some(live) = self.live.get_mut(&result.fence.run_id) { live.report_digest = Some(reports.digest); }
                                let attention_allowed = run.terminal.is_some() && self.live.get(&result.fence.run_id)
                                    .is_some_and(|live|live.backend_ready && live.stop.is_none() && !live.uncertain);
                                apply_reports(run, reports, attention_allowed); let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
                            }
                        }
                    }
                    continue;
                }
                let workspace_stage = matches!(&result.result, Ok(WorkerStep::Workspace { .. }));
                let exact = self.pending.get(&result.fence.run_id).is_some_and(|pending|pending.fence == result.fence
                    && !pending.cancelled.load(Ordering::Acquire));
                if !workspace_stage && self.pending.get(&result.fence.run_id).is_some_and(|pending|pending.fence == result.fence) {
                    self.pending.remove(&result.fence.run_id);
                }
                if !exact || !still_pending(app, &result.fence, owner) || result.fence.check_current(journal).is_err()
                    ||task.as_ref().is_some_and(|task|task.slot.project.lease.check_active_for_adoption().is_err())
                    ||task.is_some()&&startup.is_some_and(|gate|gate.check_on_app().is_err()) {
                    if let Some(task)=&task {
                        if let Ok(WorkerStep::Ready(ready))=result.result { task.reject_ready(ready); }
                        else { task.unused(); }
                    } continue;
                }
                let index = app.agents.iter().position(|run|run.id == result.fence.run_id).unwrap();
                match result.result {
                    Ok(WorkerStep::Workspace { prepared, proceed }) => {
                        if journal.database_path()? != prepared.identity_journal || prepared.assignment.validate(&result.fence).is_err()
                            || app.agents[index].managed_store.as_ref() != Some(&prepared.managed_run)
                            || !matches_workspace(app.agents[index].managed_local.as_ref(), &prepared.assignment, true) {
                            orphan(&app.cwd, &mut app.agents[index], "managed workspace result changed its reserved source/journal; no capability granted");
                            if let Some(task)=&task { task.unused(); }
                            continue;
                        }
                        app.agents[index].managed_local = Some(ManagedRunSource::Assigned { assignment: Box::new(prepared.assignment.clone()) });
                        if save_native_run_record(&app.cwd, &app.agents[index]).is_err() {
                            if let Some(pending) = self.pending.get(&result.fence.run_id) { pending.cancelled.store(true, Ordering::Release); }
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment persistence failed; no capability was granted");
                            if let Some(task)=&task { task.unused(); }
                        } else if proceed.try_send(prepared).is_err() {
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment continuation retired; no helper was spawned");
                            if let Some(task)=&task { task.unused(); }
                        }
                    }
                    Ok(WorkerStep::Ready(mut ready)) => {
                        if journal.database_path()? != ready.prepared.identity_journal || ready.prepared.assignment.validate(&result.fence).is_err()
                            || app.agents[index].managed_store.as_ref() != Some(&ready.prepared.managed_run)
                            || !matches_workspace(app.agents[index].managed_local.as_ref(), &ready.prepared.assignment, false) {
                            orphan(&app.cwd, &mut app.agents[index], "managed helper result changed its assigned scope/journal; no helper spawned");
                            if let Some(task)=&task { task.reject_ready(ready); }
                            continue;
                        }
                        let assignment = ready.prepared.assignment.clone();
                        app.agents[index].managed_local = Some(ManagedRunSource::Assigned { assignment: Box::new(assignment.clone()) });
                        if save_native_run_record(&app.cwd, &app.agents[index]).is_err() {
                            orphan(&app.cwd, &mut app.agents[index], "managed assignment could not be persisted; helper was not spawned");
                            if let Some(task)=&task { task.reject_ready(ready); }
                            continue;
                        }
                        // A failed spawn/activation may still have created a child.
                        // From this point Drop cannot unpin/remove the live source.
                        if let Some(task)=&task { if task.time().is_err()||task.slot.project.lease.check_active_for_adoption().is_err() {
                            task.reject_ready(ready);orphan(&app.cwd,&mut app.agents[index],"managed task deadline or project permission ended before helper activation; original preparation retained");continue;
                        } }
                        if task.is_some()&&startup.is_some_and(|gate|gate.check_on_app().is_err()) {
                            task.as_ref().unwrap().reject_ready(ready);orphan(&app.cwd,&mut app.agents[index],"machine startup admission closed before helper activation; original preparation retained");continue;
                        }
                        if task.is_none() {
                            if let Some(fork)=&ready.prepared.seed_fork {if let Err(_)=fork.start_gate().claim_helper_start() {
                                orphan(&app.cwd,&mut app.agents[index],"original helper admission sealed/refused; unused owner retained");continue;
                            }}
                            ready.prepared.adopt();
                        }
                        let options = TerminalPaneOptions { size: app.agents[index].terminal_size.unwrap_or_default(),
                            cwd: None, ..TerminalPaneOptions::default() };
                        // Keep the resource carrier OUTSIDE catch_unwind. The
                        // reserved mailbox owns it on both ordinary Err and panic.
                        let mut activation=if task.is_some() { Some(ManagedPtyActivationOwner::new(ready.handoff.take().expect("prepared handoff absent"))) } else { None };
                        let mut activation_panic=None;
                        let spawned=if let Some(activation)=&mut activation {
                            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                // Final pure validation linearization BEFORE the
                                // retaining activation's first PTY/child effect.
                                // Refusal uses the existing retained activation
                                // mailbox, preserving handoff/no-spawn custody.
                                if let Some(gate)=startup {gate.check_on_app()?;}
                                // Claim the original helper permit only AFTER
                                // that final check. A gate refusal preserves
                                // genuine never-started custody for settlement.
                                if let Some(fork)=&ready.prepared.seed_fork {fork.start_gate().claim_helper_start()?;}
                                ready.prepared.adopt();task.as_ref().unwrap().adopted();
                                activation.activate(ready.native_program.display().to_string(),&assignment.fence.attempt_id,
                                    &ready.prepared.execution_profile,Some(&ready.launch_json),options,
                                    &task.as_ref().unwrap().budget,task.as_ref().unwrap().cancelled.clone())
                            })) {
                                Ok(result)=>result.map(|(pane,handoff,control,barrier)|(pane,handoff,control,Some(barrier))),
                                Err(panic)=>{ activation_panic=Some(panic);Err(anyhow::anyhow!("managed PTY activation unwound; original owner retained")) }
                            }
                        } else { TerminalPane::spawn_managed(ready.native_program.display().to_string(),ready.handoff.take().expect("prepared handoff absent"),
                            &assignment.fence.attempt_id,&ready.prepared.execution_profile,Some(&ready.launch_json),options)
                                .map(|(pane,handoff,control)|(pane,handoff,control,None)) };
                        match spawned {
                            Ok((mut terminal, handoff, control, start)) => {
                                let task_live=match task { Some(handle)=>Some(TaskLive { handle,custody:terminal.retain_managed_helper_custody().ok(),start:start.map(RuntimeTaskStart::capture) }),None=>None };
                                let task_uncertain=task_live.as_ref().is_some_and(|task|task.custody.is_none());
                                App::attach_output_waker(&app.pty_output_waker, &terminal);
                                let adoption = if let Some(projection)=&ready.projection_owner {
                                    app.fabric_runtime.adopt_managed_broker_retaining(owner,&assignment,&ready.prepared.identity_home,
                                        &mut terminal,&mut ready.broker,ready.projection_hash,projection)
                                } else {
                                    app.fabric_runtime.adopt_managed_broker(owner, &assignment, &ready.prepared.identity_home,
                                        &mut terminal,ready.broker.take().expect("prepared broker absent"),ready.projection_hash)
                                };
                                if adoption.is_err() || task_uncertain {
                                    terminal.request_termination();
                                    orphan(&app.cwd, &mut app.agents[index], "managed broker adoption failed after helper spawn; source retained for recovery");
                                }
                                if adoption.is_ok() || task_live.is_some() {
                                    app.agents[index].terminal = Some(terminal);
                                    if adoption.is_ok() { app.agents[index].last_error = None; }
                                    self.live.insert(assignment.fence.run_id.clone(), Live { prepared: ready.prepared,
                                        handoff, control, receipt: None, stop: None, uncertain: adoption.is_err() || task_uncertain, orphan_reported: adoption.is_err() || task_uncertain,
                                        backend_ready: false, auth:None, report_pending: false, last_report: None, report_digest: None,task:task_live });
                                    let _ = save_native_run_record(&app.cwd, &app.agents[index]);
                                }
                            }
                            Err(_) => { if let Some(task)=&task {
                                    task.reject_activation(ready,activation.take().expect("task activation carrier absent"),activation_panic);
                                }
                                orphan(&app.cwd, &mut app.agents[index], "managed helper activation failed; retained activation ownership requires actual cleanup"); },
                        }
                    }
                    Err(_) => orphan(&app.cwd, &mut app.agents[index], "managed local preparation failed; no checkout fallback was launched"),
                    Ok(WorkerStep::Reports(_)) => unreachable!(),
                    Ok(WorkerStep::Authenticated { .. }) => unreachable!(),
                    Ok(WorkerStep::TaskWorkspace { .. } | WorkerStep::TaskReady { .. } | WorkerStep::TaskFailed { .. })=>unreachable!(),
                }
                app.dirty = true;
            }
            let ids: Vec<_> = self.live.keys().cloned().collect();
            for id in ids {
                let pending_auth = self.live.values().filter(|live|live.auth.is_some()).count();
                let live = self.live.get_mut(&id).unwrap();
                'reports: {
                if pending_auth == 0 && !live.report_pending && live.last_report.is_none_or(|last|last.elapsed() >= Duration::from_millis(500)) {
                    if let Ok(permit) = self.queue.admit() {
                        let task_worker=if let Some(task)=&live.task {
                            match task.handle.worker() { Ok(ticket)=>Some(ticket),Err(_)=>break 'reports }
                        } else { None };
                        let paths = live.prepared.reports.clone(); let fence = live.prepared.assignment.fence.clone();
                        let panic_fence = fence.clone();
                        let work=move |_|WorkerResult { fence,result:Ok(WorkerStep::Reports(read_reports(&paths))) };
                        let panic=move ||WorkerResult { fence:panic_fence,result:Ok(WorkerStep::Reports(Err(anyhow::anyhow!("managed report worker failed")))) };
                        let spawn=if let Some(ticket)=task_worker { self.queue.spawn_retaining(permit,ticket,app.pty_output_waker.clone(),work,panic) }
                            else { self.queue.spawn(permit,app.pty_output_waker.clone(),work,panic) };
                        if spawn.is_ok() {
                            live.report_pending = true; live.last_report = Some(Instant::now());
                        }
                    }
                }
                }
                let assignment = &live.prepared.assignment;
                let Some(index) = app.agents.iter().position(|run|run.id == id
                    && matches_workspace(run.managed_local.as_ref(), assignment, false)) else {
                    if live.task.is_none() { app.fabric_runtime.close_managed_assignment(assignment); }
                    if let Some(run) = app.agents.iter_mut().find(|run|run.id == id && run.terminal.as_ref()
                        .is_some_and(|terminal|terminal.instance_id() == assignment.fence.attempt_id)) {
                        if let Some(terminal) = run.terminal.as_mut() { terminal.request_termination(); }
                        if live.task.is_none() { run.terminal = None; }
                        orphan(&app.cwd, run, "managed helper assignment was replaced; old source retained without checkpoint acknowledgment");
                    }
                    if let Some(task)=&live.task {
                        if let Some(terminal)=app.agents.iter_mut().find(|run|run.id==id).and_then(|run|run.terminal.as_mut()) {
                            if task.custody.as_ref().is_some_and(|custody|terminal.matches_managed_helper_custody(custody))
                                && !matches!(terminal.try_wait(),Ok(Some(_))) { live.uncertain=true;continue; }
                        }
                    }
                    if let Some(live)=self.live.remove(&id) { retain_task_live(app,live,None); } continue;
                };
                if assignment.fence.owner != *owner || assignment.fence.check_current(journal).is_err() {
                    live.uncertain = true;
                }
                if let Some(Err(_)) = live.handoff.try_result() { live.uncertain = true; }
                while let Some(event) = live.control.try_event() {
                    match event {
                        ManagedExecutionEvent::BackendReady { process } if !live.uncertain && live.stop.is_none() => {
                            if live.task.is_some() && process.check_current().is_err() { live.uncertain=true;continue; }
                            if live.prepared.execution_profile == "codex-app-server" {
                                if process.authenticated_start_required() && live.auth.is_none() && pending_auth < 8 {
                                    live.auth = Some(PendingAuth { process, cancelled:Arc::new(AtomicBool::new(false)),
                                        deadline:Instant::now()+Duration::from_secs(20), running:false });
                                } else { live.uncertain = true; }
                                continue;
                            }
                            let expected = app.agents[index].session_id.clone();
                            if let Some(terminal) = app.agents[index].terminal.as_mut() {
                                if app.fabric_runtime.attach_managed_backend(owner, assignment, terminal, live.prepared.link_launch(),
                                    live.prepared.transport.guest_cwd.clone(), expected, process).is_err() { live.uncertain = true; }
                                else { live.backend_ready = true; }
                            } else { live.uncertain = true; }
                        }
                        ManagedExecutionEvent::Shutdown(receipt) => {
                            if live.stop.as_ref().is_some_and(|(request, _)| request == receipt.request()) {
                                live.receipt = Some(receipt);
                            } else { live.uncertain = true; }
                        }
                        ManagedExecutionEvent::BackendReady { .. } => {}
                        // Source-only launches require their explicit process/
                        // issuer join. Legacy Live cannot adopt this receipt.
                        ManagedExecutionEvent::SourceShutdown(_) => live.uncertain = true,
                        ManagedExecutionEvent::Uncertain => live.uncertain = true,
                    }
                }
                if live.stop.as_ref().is_some_and(|(_, at)| at.elapsed() > STOP_DEADLINE) { live.uncertain = true; }
                if live.stop.is_none() && live.auth.as_ref().is_some_and(|auth|Instant::now() >= auth.deadline) { live.uncertain = true; }
                if live.uncertain { if let Some(auth) = &live.auth { auth.cancelled.store(true, Ordering::Release); } }
                let run = &mut app.agents[index];
                let exit = if let Some(terminal) = run.terminal.as_mut() {
                    if !terminal.drain_output().is_empty() { run.last_output_at = Instant::now(); app.dirty = true; }
                    let _ = terminal.take_notifications();
                    if live.uncertain && !live.orphan_reported { terminal.request_termination(); }
                    terminal.try_wait_after_output()
                } else { Ok(None) };
                let task_exit=if let (Some(task),Some(terminal))=(&live.task,run.terminal.as_mut()) {
                    if task.custody.as_ref().is_some_and(|held|terminal.matches_managed_helper_custody(held)) {
                        terminal.successful_managed_helper_exit().ok().flatten()
                    } else { None }
                } else { None };
                if live.uncertain {
                    let first = !live.orphan_reported;
                    let ended = if live.task.is_some() { matches!(exit,Ok(Some(_))) || run.terminal.is_none() }
                        else { !matches!(exit,Ok(None)) || run.terminal.is_none() };
                    if first {
                        if live.task.is_none() { app.fabric_runtime.close_managed_assignment(assignment); }
                        run.status = AgentStatus::Orphaned; run.completed_at = None;
                        run.last_error = Some("managed helper outcome is uncertain; no checkpoint acknowledged".into());
                        live.orphan_reported = true;
                    }
                    if ended && live.task.is_none() { run.terminal = None; }
                    if first || ended { let _ = save_native_run_record(&app.cwd, run); app.dirty = true; }
                    if ended { if let Some(live)=self.live.remove(&id) { retain_task_live(app,live,task_exit); } }
                } else if let Ok(Some(_)) = exit {
                    // The process reaper and private FD worker race to deliver
                    // their final events. Keep the owned terminal until the
                    // bounded stop deadline lets the receipt worker settle.
                    if live.stop.is_some() && live.receipt.is_none() { continue; }
                    // Physical exit alone never certifies final bytes. Even an
                    // observed completed model turn still needs the FD receipt.
                    let proved = live.receipt.is_some() && (live.task.is_none() || task_exit.is_some());
                    if live.task.is_none() { app.fabric_runtime.close_managed_assignment(assignment); run.terminal = None; } run.completed_at = None;
                    if proved { live.prepared.confirm_transient_settlement(); }
                    run.status = if proved { AgentStatus::Paused } else { AgentStatus::Orphaned };
                    run.last_error = Some(if proved { "managed helper stopped with retained checkpoint; source acceptance remains pending".into() }
                        else { "managed helper exited without a matching shutdown receipt; recovery required".into() });
                    let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
                    if let Some(live)=self.live.remove(&id) { retain_task_live(app,live,task_exit); }
                } else if exit.is_err() {
                    live.uncertain = true;
                }
            }
            // Oldest first, with one aggregate admission budget shared with
            // preparation. Waiting keys stay in their host-owned prepared value.
            let mut waiting: Vec<_> = self.live.iter().filter_map(|(id,live)|live.auth.as_ref()
                .filter(|auth|!auth.running && !live.uncertain && live.stop.is_none()).map(|auth|(auth.deadline,id.clone()))).collect();
            waiting.sort_unstable();
            for (_,id) in waiting {
                let Ok(permit) = self.queue.admit() else { break; };
                let live = self.live.get_mut(&id).unwrap();
                let task_worker=if let Some(task)=&live.task {
                    match task.handle.worker() { Ok(ticket)=>Some(ticket),Err(_)=>continue }
                } else { None };
                let auth = live.auth.as_mut().unwrap();
                if Instant::now() >= auth.deadline || auth.cancelled.load(Ordering::Acquire) { live.uncertain = true; continue; }
                let Some(key) = live.prepared.take_codex_api_key() else { live.uncertain = true; continue; };
                let fence = live.prepared.assignment.fence.clone(); let panic_fence = fence.clone();
                let process = auth.process.clone(); let panic_process = process.clone(); let cancelled = auth.cancelled.clone();
                let deadline = auth.deadline; let launch = live.prepared.link_launch();
                auth.running = true;
                let work=move |_| {
                    let result = agent_link::authenticate_managed_codex_api_key(&launch, process.clone(), key, &cancelled, deadline);
                    WorkerResult { fence, result:Ok(WorkerStep::Authenticated { process, result }) }
                };
                let panic=move ||WorkerResult { fence:panic_fence,result:Ok(WorkerStep::Authenticated { process:panic_process,
                    result:Err(agent_link::ManagedCodexAuthError::Transport) }) };
                let spawn=if let Some(ticket)=task_worker { self.queue.spawn_retaining(permit,ticket,app.pty_output_waker.clone(),work,panic) }
                    else { self.queue.spawn(permit,app.pty_output_waker.clone(),work,panic) };
                if spawn.is_err() { live.uncertain=true; }
            }
            Ok(())
        }
    }

    fn orphan(repo: &Path, run: &mut AgentRun, reason: &str) {
        run.status = AgentStatus::Orphaned; run.completed_at = None; run.last_error = Some(reason.into());
        let _ = save_native_run_record(repo, run);
    }

    pub(crate) fn stop(app: &mut App, index: usize) -> Option<bool> {
        if let Some(stopped) = crate::managed_source_app::stop(app, index) { return Some(stopped); }
        let run = app.agents.get_mut(index)?;
        run.managed_local.as_ref()?;
        app.pending_enters.retain(|pending|pending.run_id != run.id);
        if let Some(pending) = app.managed_local.pending.get(&run.id) {
            pending.cancelled.store(true, Ordering::Release);
            run.status = if pending.budget.is_some() { AgentStatus::Orphaned } else { AgentStatus::Stopped }; run.completed_at = None;
            run.last_error = Some(if pending.budget.is_some() { "managed task cancellation requested; awaiting actual preparation and unused cleanup settlement".into() }
                else { "managed launch cancelled before helper adoption".into() });
        } else if let Some(live) = app.managed_local.live.get_mut(&run.id) {
            if let Some(auth) = &live.auth { auth.cancelled.store(true, Ordering::Release); }
            if live.stop.is_none() && !live.uncertain {
                let assignment = &live.prepared.assignment; let fence = &assignment.fence;
                let request = StopAndCheckpoint { request_id: uuid::Uuid::new_v4().to_string(), execution_id: fence.attempt_id.clone(),
                    operation_id: assignment.observation.attempt.operation_id.clone(), view_id: assignment.observation.view_id.clone(),
                    run_id: fence.run_id.clone(), run_epoch: fence.run_epoch, owner: fence.owner_epoch() };
                let result = run.terminal.as_ref().context("managed terminal is absent").and_then(|terminal|
                    app.fabric_runtime.begin_managed_shutdown(&fence.owner, assignment, terminal))
                    .and_then(|_|live.control.stop_and_checkpoint(request.clone()));
                if result.is_ok() { live.stop = Some((request, Instant::now())); }
                else { live.uncertain = true; }
            }
            run.last_error = Some("stopping exact managed helper; awaiting checkpoint and physical exit".into());
        } else {
            orphan(&app.cwd, run, "managed source requires explicit owned recovery; no legacy stop/resume path");
        }
        let _ = save_native_run_record(&app.cwd, run); app.dirty = true;
        Some(true)
    }

    pub(crate) fn abandon(app: &mut App, run_id: &str) {
        if let Some(pending) = app.managed_local.pending.get(run_id) { pending.cancelled.store(true, Ordering::Release); }
        if let Some(live)=app.managed_local.live.get_mut(run_id).filter(|live|live.task.is_some()) {
            live.uncertain=true;
            if let Some(terminal)=app.agents.iter_mut().find(|run|run.id==run_id).and_then(|run|run.terminal.as_mut()) {
                if live.task.as_ref().and_then(|task|task.custody.as_ref()).is_some_and(|custody|terminal.matches_managed_helper_custody(custody)) {
                    terminal.request_termination();
                }
            }
            return; // original live pane/reap remains App-owned until poll
        }
        app.fabric_runtime.close(run_id);
        // Dropping the handle closes the private control channel. The helper
        // must withhold checkpoint authority and retains its orphan resources.
        if let Some(live)=app.managed_local.live.remove(run_id) { retain_task_live(app,live,None); }
    }

    pub(crate) fn accepts_input(app: &App, index: usize) -> bool {
        if let Some(accepts) = crate::managed_source_app::accepts_input(app, index) { return accepts; }
        let Some(run) = app.agents.get(index) else { return false; };
        if run.managed_local.is_none() { return true; }
        app.managed_local.live.get(&run.id).is_some_and(|live|live.backend_ready && !live.uncertain && live.stop.is_none()
            && matches_workspace(run.managed_local.as_ref(), &live.prepared.assignment, false))
    }

    #[cfg(test)]
    mod machine_startup_gate_tests {use super::*;include!("managed_app_machine_startup_gate_tests.rs");}
    #[cfg(test)]
    mod ingress_observation_tests {use super::*;include!("managed_ingress_observation_tests.rs");}
    #[cfg(test)]
    mod task_retained_activation_tests { use super::*;include!("managed_task_retained_activation_tests.rs"); }
    #[cfg(test)]
    mod task_wiring_tests { use super::*;include!("managed_task_launch_wiring_tests.rs");include!("managed_never_started_worker_tests.rs"); }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn cancelled_or_timed_out_workers_hold_admission_until_actual_settlement() {
            let queue = WorkerQueue::new();
            let (release, receive) = mpsc::sync_channel(1);
            let cancelled = Arc::new(AtomicBool::new(false));
            let observed = cancelled.clone();
            let first = queue.admit().unwrap();
            queue.spawn(first, None, move |_| { receive.recv().unwrap(); observed.load(Ordering::Acquire) }, ||false).unwrap();
            let second = queue.admit().unwrap();
            cancelled.store(true, Ordering::Release);
            assert!(queue.admit().is_err(), "cancellation cannot release an executing worker's permit");
            drop(second);
            let second = queue.admit().unwrap();
            assert!(queue.admit().is_err());
            release.send(()).unwrap();
            assert!(queue.receive.recv_timeout(Duration::from_secs(2)).unwrap());
            let deadline = Instant::now() + Duration::from_secs(2);
            while queue.active.load(Ordering::Acquire) == MAX_PREPARATIONS && Instant::now() < deadline { thread::yield_now(); }
            let recovered = queue.admit().unwrap();
            drop(recovered); drop(second);
            assert_eq!(queue.active.load(Ordering::Acquire), 0);
        }
        #[test]
        fn actual_worker_panic_retires_exact_pending_state_and_preserves_cancellation() {
            for cancel in [false, true] {
                let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
                let mut journal = Journal::open(app.cwd.join("journal.sqlite")).unwrap();
                let owner = journal.create_project("p", "owner", "base", 0).unwrap();
                let attempt_id = uuid::Uuid::new_v4().to_string();
                let run_epoch = journal.observe_run_attempt(&owner, "run", &attempt_id, 1).unwrap();
                let fence = ManagedLaunchFence { owner:owner.clone(), run_id:"run".into(), run_epoch,
                    attempt_id, request_id:uuid::Uuid::new_v4().to_string() };
                let marker = ManagedRunSource::Reserved { fence:fence.clone(), runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") };
                let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"run","task":"owned","managedLocal":marker})).unwrap();
                run.managed_local = Some(marker); run.status = AgentStatus::Running; app.agents.push(run);
                let mut state = ManagedAppState::default();
                let cancelled = Arc::new(AtomicBool::new(false));
                state.pending.insert("run".into(), Pending { fence:fence.clone(), cancelled:cancelled.clone(), started:Instant::now(),budget:None });
                let permit = state.queue.admit().unwrap();
                let (release, receive) = mpsc::sync_channel(1);
                state.queue.spawn(permit, None, move |_| { receive.recv().unwrap(); panic!("fixture panic payload must not enter App notices"); },
                    move ||WorkerResult { fence, result:Err(anyhow::anyhow!("managed preparation worker failed without a result")) }).unwrap();
                if cancel { cancelled.store(true, Ordering::Release); app.agents[0].status = AgentStatus::Stopped; }
                assert_eq!(state.queue.active.load(Ordering::Acquire), 1);
                release.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while (!state.pending.is_empty() || state.queue.active.load(Ordering::Acquire) != 0) && Instant::now() < deadline {
                    state.poll(&mut app, &mut journal, &owner).unwrap(); thread::yield_now();
                }
                assert!(state.pending.is_empty(), "settled panics must not leave phantom reservations");
                assert_eq!(state.queue.active.load(Ordering::Acquire), 0);
                assert_eq!(app.agents[0].status, if cancel { AgentStatus::Stopped } else { AgentStatus::Orphaned });
                assert!(!app.agents[0].last_error.as_deref().unwrap_or("").contains("fixture panic payload"));
                assert!(state.queue.admit().is_ok(), "actual settlement restores bounded admission");
                let _ = fs::remove_dir_all(&app.cwd);
            }
        }
        #[test]
        fn late_results_cannot_revive_cancelled_removed_or_replaced_reservations() {
            for kind in ["stopped", "removed", "replacement", "new-request"] {
                let mut app = App::new(); fs::create_dir_all(&app.cwd).unwrap();
                let mut journal = Journal::open(app.cwd.join("journal.sqlite")).unwrap();
                let owner = journal.create_project("p", "owner", "base", 0).unwrap();
                let attempt = uuid::Uuid::new_v4().to_string();
                let epoch = journal.observe_run_attempt(&owner, "run", &attempt, 1).unwrap();
                let fence = ManagedLaunchFence { owner:owner.clone(), run_id:"run".into(), run_epoch:epoch,
                    attempt_id:attempt, request_id:uuid::Uuid::new_v4().to_string() };
                let marker = ManagedRunSource::Reserved { fence:fence.clone(), runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") };
                let mut run = crate::gitio::agent_from_run_record(&app.cwd, serde_json::json!({"id":"run","task":"owned","managedLocal":marker})).unwrap();
                run.managed_local = Some(marker); run.status = AgentStatus::Running; app.agents.push(run);
                assert!(still_pending(&app, &fence, &owner));
                let cancelled = Arc::new(AtomicBool::new(false));
                let mut state = ManagedAppState::default();
                state.pending.insert("run".into(), Pending { fence:fence.clone(), cancelled:cancelled.clone(), started:Instant::now(),budget:None });
                match kind {
                    "stopped" => app.agents[0].status = AgentStatus::Stopped,
                    "removed" => app.agents.clear(),
                    _ => {
                        let mut next = fence.clone(); next.request_id = uuid::Uuid::new_v4().to_string();
                        if kind == "replacement" { next.attempt_id = uuid::Uuid::new_v4().to_string();
                            next.run_epoch = journal.observe_run_attempt(&owner, "run", &next.attempt_id, 2).unwrap(); }
                        app.agents[0].managed_local = Some(ManagedRunSource::Reserved { fence:next, runnable_root:rudder_engine::protocol::ObjectRef::from_bytes(rudder_engine::protocol::ObjectKind::RunnableRoot, b"fixture") });
                    }
                }
                assert!(!still_pending(&app, &fence, &owner));
                assert!(state.queue.send.send(WorkerResult { fence, result:Err(anyhow::anyhow!("late fixture result")) }).is_ok());
                state.poll(&mut app, &mut journal, &owner).unwrap();
                assert!(cancelled.load(Ordering::Acquire));
                assert!(state.pending.is_empty());
                assert!(state.live.is_empty());
                if kind == "removed" { assert!(app.agents.is_empty()); }
                else { assert_eq!(app.agents[0].status, if kind == "stopped" { AgentStatus::Stopped } else { AgentStatus::Running }); }
                let _ = fs::remove_dir_all(&app.cwd);
            }
        }
        #[test]
        fn report_reads_are_bounded_dirfd_scoped_and_never_promote_guest_claims() {
            use std::os::unix::fs::symlink;
            let root = env::temp_dir().join(format!("rudder-managed-report-{}", uuid::Uuid::new_v4()));
            let home = root.join("home"); let reports = home.join(".rudder-reports");
            fs::create_dir_all(&reports).unwrap();
            let paths = managed_launch::ManagedReportMapping { host_done_file:reports.join("done.json"), host_signal_file:reports.join("signal.json"),
                guest_done_file:"/home/agent/.rudder-reports/done.json".into(), guest_signal_file:"/home/agent/.rudder-reports/signal.json".into() };
            fs::write(&paths.host_done_file, br#"{"node":"peer","run_id":"peer","summary":"candidate only","delivery":{"status":"verified"},"checkpoint":"forged"}"#).unwrap();
            fs::write(&paths.host_signal_file, br#"{"state":"done","backend_ready":true,"ack":true}"#).unwrap();
            let mut run = crate::gitio::agent_from_run_record(&root, serde_json::json!({"id":"owned","task":"owned","status":"running"})).unwrap();
            let delivery = run.delivery.clone();
            apply_reports(&mut run, read_reports(&paths).unwrap(), true);
            assert_eq!(run.id, "owned"); assert_eq!(run.status, AgentStatus::Running);
            assert_eq!(run.done_summary.as_deref(), Some("candidate only")); assert_eq!(run.delivery, delivery);
            assert!(run.completed_at.is_none());
            fs::write(&paths.host_signal_file, br#"{"state":"permission"}"#).unwrap();
            apply_reports(&mut run, read_reports(&paths).unwrap(), true); assert!(run.needs_permission);
            run.status = AgentStatus::Paused; run.needs_permission = false; run.wait_signal = None;
            apply_reports(&mut run, read_reports(&paths).unwrap(), false);
            assert!(!run.needs_permission && run.wait_signal.is_none(), "late writable attention hints cannot revive a stopped pane");
            assert_eq!(run.status, AgentStatus::Paused);
            fs::write(&paths.host_done_file, vec![b'x';64*1024+1]).unwrap(); assert!(read_reports(&paths).is_err());
            fs::remove_file(&paths.host_done_file).unwrap();
            let outside = root.join("outside"); fs::create_dir(&outside).unwrap(); fs::write(outside.join("done.json"), b"private fixture").unwrap();
            symlink(outside.join("done.json"), &paths.host_done_file).unwrap(); assert!(read_reports(&paths).is_err());
            fs::remove_file(&paths.host_done_file).unwrap(); fs::remove_file(&paths.host_signal_file).unwrap(); fs::remove_dir(&reports).unwrap();
            symlink(&outside, &reports).unwrap(); assert!(read_reports(&paths).is_err(), "a writable parent symlink cannot expose host files");
            fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use local::{NeverStartedManagedTask,ManagedAppState, JoinedManagedTask, ManagedTaskJournalOwner, ManagedTaskLifetimeSupervisor, ManagedTaskStartFailure, PendingManagedTaskHelper, poll, poll_with_startup_gate, stop, abandon, accepts_input};

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub(crate) struct ManagedAppState;
#[cfg(not(target_os = "linux"))]
pub(crate) fn poll(_: &mut App, _: &mut Journal, _: &OwnerFence) -> Result<()> { Ok(()) }
#[cfg(not(target_os = "linux"))]
pub(crate) fn stop(app: &mut App, index: usize) -> Option<bool> {
    let run = app.agents.get_mut(index)?;
    run.managed_local.as_ref()?;
    run.status = AgentStatus::Orphaned;
    run.last_error = Some("managed local execution requires a qualified Linux worker".into());
    Some(false)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn abandon(_: &mut App, _: &str) {}
#[cfg(not(target_os = "linux"))]
pub(crate) fn accepts_input(app: &App, index: usize) -> bool { app.agents.get(index).is_some_and(|run|run.managed_local.is_none()) }
#[cfg(not(target_os = "linux"))]
impl App {
    pub(crate) fn start_managed_local(&mut self, _: usize, _: ManagedLocalLaunchInputs, _: PathBuf,
        _: &mut Journal, _: &OwnerFence) -> Result<ManagedLaunchFence> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "managed local execution requires a qualified Linux worker").into())
    }
}

#[cfg(test)]
#[path = "managed_app_tests.rs"]
mod integration_tests;
