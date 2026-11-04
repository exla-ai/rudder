//! Connected explicit managed ingress. Host transport/SQLite/Git/profile work
//! stays on retained off-App workers; the actual App thread alone owns rows,
//! adoption and PTYs. Machine receipts never mint project/source authority.
#![cfg(target_os="linux")]
use crate::{Backend,AgentMode,AgentStatus,
    engine_host::RetainedManagedAcquisition,managed_app::{ManagedTaskLifetimeSupervisor,ManagedTaskStartFailure,JoinedManagedTask},
    managed_initial_source::ManagedInitialSourceOwner,managed_launch::ManagedLaunchFence,
    managed_machine_entry::{ManagedMachineRuntime,MachineControl,ProjectTicket,ManagedProjectDisplay,SeedTicket,SeedTaskTicket,SeedCacheTicket,EnrollmentTicket,NeverStartedProgress},
    managed_task_launch::{ManagedTaskController,ManagedTaskAdmission,ManagedTaskFactoryRetention,PreparedManagedTask,ManagedTaskProject,
        ManagedTaskOptIn,SelectedTaskCredentialKind,ManagedTaskNetworkSelection},
    managed_profile_loader::{inspect_selected_managed_task_profile,ManagedProfileCredentialKind},
    managed_machine_receipts::{MachineReceipts,AcceptedHostCommand,DuplicateReceipt},managed_machine_runtime_profile::{ManagedHostRuntime,ManagedProfileInspection}};
use anyhow::{ensure,Context,Result};
use rudder_engine::execution::{finalization_budget::FinalizationBudget,handoff::{OwnedWorkerJoin,WorkerJoined}};
use rudder_native::host_ingress::{HostIngress,HostIngressConfigOwner,HostCommand,HostOperation,HostBackend,HostReply,HostReplyError};
use std::{any::Any,collections::{BTreeMap,VecDeque},panic::{catch_unwind,AssertUnwindSafe},sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,AtomicU8,Ordering},mpsc::{self,SyncSender,Receiver},OnceLock},thread,time::Duration};
const JOB_LIMIT:usize=256;
const PROJECT_LIMIT:usize=2;
const WORKERS:usize=2;
const APP_EVENTS:usize=8;
const OPENING:u8=0;const READY:u8=1;const RECOVERY:u8=2;
const STEP_NONE:u8=0;const STEP_ADMIT:u8=1;const STEP_ADMITTED:u8=2;const STEP_LAUNCH:u8=3;const STEP_LAUNCHED:u8=4;const STEP_REFUSED:u8=5;
const STEP_STOP:u8=6;const STEP_STOP_RECORDED:u8=7;
enum AppEvent {Work(Arc<Job>),Observe(Arc<ObservationRequest>)}
struct ObservationRequest {job:Arc<Job>,original:FinalizationBudget,result:Mutex<Option<serde_json::Value>>,done:AtomicBool}
struct FactoryPermit; // private linear capability, never reconstructed from an absence flag.
struct FactoryCarrier {admission:Option<ManagedTaskAdmission>,retention:Option<ManagedTaskFactoryRetention>,
    prepared:Option<PreparedManagedTask>,outcome:Option<Result<PreparedManagedTask>>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>,recorded:bool,ready:bool}
/// Strong original selected source only. Never stores its Job or a TaskScope;
/// TaskScope's original command continuation remains weak in the reverse direction.
#[derive(Clone)]
pub(crate) struct OriginalTaskSelectedSource {project:ProjectTicket,seed:SeedTicket}
impl OriginalTaskSelectedSource {
    pub(crate) fn originals(&self)->(&ProjectTicket,&SeedTicket){(&self.project,&self.seed)}
}
struct TaskWork {selected_source:Option<OriginalTaskSelectedSource>,refused_before_factory:Option<&'static str>,admission:Option<ManagedTaskAdmission>,factory_permit:Option<FactoryPermit>,factory:Option<Arc<Mutex<FactoryCarrier>>>,
    factory_handle:Option<thread::JoinHandle<()>>,scope:Option<SeedTaskTicket>,started:Option<ManagedLaunchFence>,refusal:Option<ManagedTaskStartFailure>,
    pending_helper:Option<crate::managed_app::PendingManagedTaskHelper>,enrollment:Option<EnrollmentTicket>,enrollment_submitted:bool,enrollment_submission_observed:bool,released:bool,cleanup:Option<(FinalizationBudget,Arc<AtomicBool>)>,joined:Option<JoinedManagedTask>}
impl TaskWork {fn new()->Self {Self {selected_source:None,refused_before_factory:None,admission:None,factory_permit:None,factory:None,factory_handle:None,scope:None,started:None,refusal:None,
    pending_helper:None,enrollment:None,enrollment_submitted:false,enrollment_submission_observed:false,released:false,cleanup:None,joined:None}}}
struct JobCarrier {accepted:Option<AcceptedHostCommand>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    stop_target:Option<Arc<Job>>,
    node:Option<Arc<ProjectNode>>,profile_inspection:Option<ManagedProfileInspection>,task:TaskWork,result:Option<serde_json::Value>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>,
    worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>,charged:bool,settled:bool,sdk:Option<Arc<sdk_driver::Slot>>,canonical:Option<Arc<canonical_driver::Campaign>>,canonical_wait:Option<&'static str>}
#[cfg(feature="managed-ingress-genuine-fixture")]
use crate::managed_machine_entry::ingress_genuine_fixture::FixtureGate;
pub(crate) struct IngressJob {
    #[cfg(feature="managed-ingress-genuine-fixture")]
    fixture_gate:OnceLock<std::sync::Weak<FixtureGate>>,
    command:HostCommand,machine:MachineControl,root:OnceLock<Arc<crate::managed_machine_entry::IngressCommandRoot>>,carrier:Mutex<JobCarrier>,step:AtomicU8,worker_owned:AtomicBool}
type Job=IngressJob;
impl IngressJob {
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_is_open_prepared_project(&self)->bool {matches!(&self.command.operation,HostOperation::OpenPreparedProjectV1{..})}
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_original_budget_and_cancel(&self)->Result<(FinalizationBudget,Arc<AtomicBool>)> {
        let held=self.carrier.try_lock().map_err(|_|anyhow::anyhow!("fixture original command busy/poisoned"))?;
        Ok((held.original.clone(),held.cancelled.clone()))
    }
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_attach_gate(&self,actual:&Arc<FixtureGate>)->Result<()> {
        self.fixture_gate.set(Arc::downgrade(actual)).map_err(|_|anyhow::anyhow!("fixture original command gate occupied"))
    }
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_gate(&self)->Option<Arc<FixtureGate>> {self.fixture_gate.get().and_then(std::sync::Weak::upgrade)}
    pub(crate) fn covered_root(&self)->Option<&Arc<crate::managed_machine_entry::IngressCommandRoot>>{self.root.get()}
    pub(crate) fn retain_covered_root(&self,root:Arc<crate::managed_machine_entry::IngressCommandRoot>)->Result<()>{self.root.set(root).map_err(|_|anyhow::anyhow!("original command root already attached"))}
    pub(crate) fn is_original_task_command(&self)->bool{matches!(&self.command.operation,HostOperation::StartTaskV1{..})}
    pub(crate) fn belongs_to_machine(&self,machine:&MachineControl)->bool{self.machine.same_original_machine(machine)}
    /// Caller already retains this SAME Job. Pin partial descendants before
    /// validating them; busy remains Pending. Release Job before seed/task locks.
    pub(crate) fn retain_original_task_descendants(&self,root:&Arc<crate::managed_machine_entry::IngressCommandRoot>,
        selected:&mut Option<OriginalTaskSelectedSource>,scope:&mut Option<SeedTaskTicket>)->Result<bool> {
        let (held,poisoned)=match self.carrier.try_lock() {
            Ok(held)=>(held,false),Err(std::sync::TryLockError::WouldBlock)=>return Ok(false),
            Err(std::sync::TryLockError::Poisoned(error))=>(error.into_inner(),true),
        };
        if selected.is_none(){*selected=held.task.selected_source.clone();}
        if scope.is_none(){*scope=held.task.scope.clone();}
        ensure!(!poisoned,"original command carrier poisoned; Job and partial descendants retained");
        ensure!(self.covered_root().is_some_and(|actual|Arc::ptr_eq(actual,root)),"foreign original command root; Job retained");
        if !self.is_original_task_command(){
            ensure!(selected.is_none()&&scope.is_none(),"non-task original command has task descendants");return Ok(true);
        }
        let (Some(selected),Some(actual_selected))=(selected.as_ref(),held.task.selected_source.as_ref()) else{return Ok(false)};
        ensure!(root.same_selected_source(selected,actual_selected),"original command selected source changed; owners retained");
        let (Some(scope),Some(actual_scope))=(scope.as_ref(),held.task.scope.as_ref()) else{return Ok(false)};
        ensure!(scope.same_original_task(actual_scope),"original command task changed; owners retained");
        drop(held);
        scope.observe_original_command_association(root,selected)
    }
}
/// Constructed only at this actual StartTask's selected-source boundary. This
/// temporary call owner is never stored in its TaskScope/root registry.
pub(crate) struct OriginalTaskCommandSelection {actual:Arc<Job>,_node:Arc<ProjectNode>,source:OriginalTaskSelectedSource,root:Arc<crate::managed_machine_entry::IngressCommandRoot>}
impl OriginalTaskCommandSelection {
    pub(crate) fn originals(&self)->(&Arc<crate::managed_machine_entry::IngressCommandRoot>,&Arc<Job>,&ProjectTicket,&SeedTicket){let(project,seed)=self.source.originals();(&self.root,&self.actual,project,seed)}
    pub(crate) fn selected_source(&self)->(&ProjectTicket,&SeedTicket){self.source.originals()}
}
struct FreshAttemptState {attempted:bool,original:Option<FinalizationBudget>,cancelled:Arc<AtomicBool>,kernel:Option<Arc<crate::managed_machine_entry::ManagedKernelStartup>>,
    registration:Option<Arc<rudder_native::engine_registration::RegisteredEngineOwner>>,client:Option<Arc<rudder_native::root_storage_bootstrap::runtime_image::RuntimeImageClient>>,
    coordinator:Option<Arc<crate::managed_fresh_machine::ManagedFreshMachine>>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
pub(crate) struct FreshInitializationAttempt {config:Arc<HostIngressConfigOwner>,machine:MachineControl,root:OnceLock<Arc<crate::managed_machine_entry::FreshInitializationRoot>>,coordinator:OnceLock<std::sync::Weak<crate::managed_fresh_machine::ManagedFreshMachine>>,state:Mutex<FreshAttemptState>}
impl FreshInitializationAttempt {
    pub(crate) fn original_coordinator(&self)->Option<Arc<crate::managed_fresh_machine::ManagedFreshMachine>>{self.coordinator.get().and_then(std::sync::Weak::upgrade)}
    fn capture(config:Arc<HostIngressConfigOwner>,machine:MachineControl)->Arc<Self>{Arc::new(Self{config,machine,root:OnceLock::new(),coordinator:OnceLock::new(),state:Mutex::new(FreshAttemptState{attempted:false,original:None,cancelled:Arc::new(AtomicBool::new(false)),kernel:None,registration:None,client:None,coordinator:None,error:None,panic:None})})}
    pub(crate) fn covered_root(&self)->Option<&Arc<crate::managed_machine_entry::FreshInitializationRoot>>{self.root.get()}
    pub(crate) fn retain_covered_root(&self,root:Arc<crate::managed_machine_entry::FreshInitializationRoot>)->Result<()>{self.root.set(root).map_err(|_|anyhow::anyhow!("original fresh root already attached"))}
    pub(crate) fn belongs_to_machine(&self,machine:&MachineControl)->bool{self.machine.same_original_machine(machine)}
}
struct CacheOutcome {result:Option<Result<()>>,panic:Option<Box<dyn Any+Send>>}
struct ProjectWork {fresh:Option<Arc<Mutex<crate::managed_fresh_assigned::AssignedFreshProject>>>,fresh_engine:Option<crate::engine_host::EngineOwnership>,acquisition:Option<RetainedManagedAcquisition>,runtime_capture:Option<ManagedHostRuntime>,runtime:Option<Arc<ManagedHostRuntime>>,
    source:Option<ManagedInitialSourceOwner>,ticket:Option<ProjectTicket>,seed:Option<SeedTicket>,cache:Option<SeedCacheTicket>,
    cache_handle:Option<thread::JoinHandle<()>>,cache_outcome:Option<Arc<Mutex<CacheOutcome>>>,supervisor:Option<Arc<ManagedTaskLifetimeSupervisor>>,project:Option<Arc<ManagedTaskProject>>}
struct ProjectNode {alias:String,selection:Option<(HostBackend,String,String)>,phase:AtomicU8,controller:Arc<ManagedTaskController>,work:Mutex<ProjectWork>}
struct Shared {
    #[cfg(feature="managed-ingress-genuine-fixture")]
    fixture_gate:OnceLock<Arc<FixtureGate>>,
    config:Arc<HostIngressConfigOwner>,control:MachineControl,fresh_attempt:Mutex<Option<Arc<FreshInitializationAttempt>>>,fresh:Mutex<Option<Arc<crate::managed_fresh_machine::ManagedFreshMachine>>>,fresh_error:Mutex<Option<anyhow::Error>>,transport:Mutex<Option<HostIngress>>,receipts:Mutex<Option<MachineReceipts>>,
    jobs:Mutex<BTreeMap<String,Arc<Job>>>,projects:Mutex<BTreeMap<String,Arc<ProjectNode>>>,queue:Mutex<VecDeque<Arc<Job>>>,
    sdk_projects:Mutex<BTreeMap<String,Arc<sdk_driver::Slot>>>,canonical_sdk:Mutex<Option<Arc<rudder_native::root_storage_bootstrap::runtime_image::CompilerSdkClient>>>,canonical_projects:Mutex<BTreeMap<String,Arc<canonical_driver::Campaign>>>,canonical_previous:Mutex<BTreeMap<String,Arc<crate::managed_checkpoint_accept::canonical::CanonicalGateEvidence>>>,app_thread:std::thread::ThreadId,
    active:AtomicUsize,fenced:AtomicBool,worker_owned:AtomicBool,panic:Mutex<Option<Box<dyn Any+Send>>>,send:SyncSender<AppEvent>}
/// Keep this SAME object alongside ManagedMachineRuntime through quarantine.
/// It retains every actual transport/config/worker/job/resource owner. No Drop
/// cleanup, forgotten/global owner or join on App is introduced.
pub(crate) struct ManagedMachineIngress {shared:Arc<Shared>,worker:Option<OwnedWorkerJoin>,receive:Receiver<AppEvent>,pending:Vec<AppEvent>,app_thread:std::thread::ThreadId}
#[derive(Clone)]
pub(crate) struct ManagedIngressControl {shared:Arc<Shared>,app_thread:std::thread::ThreadId}
#[cfg(feature="managed-ingress-genuine-fixture")]
pub(crate) struct FixtureJobData {
    pub(crate) original_job_retained:bool,pub(crate) root_present:bool,pub(crate) root_error_retained:bool,
    pub(crate) no_effects:bool,pub(crate) no_descendants:bool,
    pub(crate) command_sha256:String,pub(crate) session_generation_sha256:String,
}
impl ManagedIngressControl {
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_attach_gate(&self,actual:&Arc<FixtureGate>)->Result<()> {
        self.shared.fixture_gate.set(actual.clone()).map_err(|_|anyhow::anyhow!("original ingress fixture gate occupied"))
    }
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_gate(&self)->Option<&Arc<FixtureGate>> {self.shared.fixture_gate.get()}
    /// Fixed SAME-Job DATA; never inspect transport/receipt guards held by receive_one.
    /// Each successful try-lock is released before the next payload lock.
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(crate) fn fixture_job_data(&self,actual:&Arc<IngressJob>)->Result<Option<FixtureJobData>> {
        ensure!(actual.belongs_to_machine(&self.shared.control),"fixture foreign original Job");
        let retained=match self.shared.jobs.try_lock(){
            Ok(jobs)=>jobs.get(&actual.command.command_key).is_some_and(|job|Arc::ptr_eq(job,actual)),
            Err(std::sync::TryLockError::WouldBlock)=>return Ok(None),Err(_)=>anyhow::bail!("fixture original jobs poisoned")};
        let (no_effects,root_error_retained)=match actual.carrier.try_lock(){
            Ok(held)=>{let task=&held.task;
                (held.accepted.is_none()&&held.node.is_none()&&held.profile_inspection.is_none()
                    &&held.result.is_none()&&held.panic.is_none()&&held.worker.is_none()&&held.joined.is_none()
                    &&!held.charged&&!held.settled&&held.sdk.is_none()&&held.canonical.is_none()&&held.canonical_wait.is_none()
                    &&task.selected_source.is_none()&&task.refused_before_factory.is_none()&&task.admission.is_none()&&task.factory_permit.is_none()
                    &&task.factory.is_none()&&task.factory_handle.is_none()&&task.scope.is_none()&&task.started.is_none()
                    &&task.refusal.is_none()&&task.pending_helper.is_none()&&task.enrollment.is_none()
                    &&!task.enrollment_submitted&&!task.released&&task.cleanup.is_none()&&task.joined.is_none()
                    &&actual.step.load(Ordering::Acquire)==STEP_NONE&&!actual.worker_owned.load(Ordering::Acquire),held.error.is_some())},
            Err(std::sync::TryLockError::WouldBlock)=>return Ok(None),Err(_)=>anyhow::bail!("fixture original Job carrier poisoned")};
        let not_queued=match self.shared.queue.try_lock(){
            Ok(queue)=>!queue.iter().any(|job|Arc::ptr_eq(job,actual)),
            Err(std::sync::TryLockError::WouldBlock)=>return Ok(None),Err(_)=>anyhow::bail!("fixture original queue poisoned")};
        use sha2::Digest;
        // Same existing protocol canonicalization, for DATA only.
        // No command body/session is emitted or reconstructed as authority.
        let canonical=rudder_engine::protocol::canonical_json(&actual.command)?;
        let command_sha256=format!("{:x}",sha2::Sha256::digest(&canonical));
        let session_generation_sha256=format!("{:x}",sha2::Sha256::digest(actual.command.session_generation.as_bytes()));
        Ok(Some(FixtureJobData{original_job_retained:retained,root_present:actual.root.get().is_some(),root_error_retained,
            no_effects:no_effects&&not_queued,no_descendants:actual.root.get().is_none_or(|root|root.fixture_no_descendants()),
            command_sha256,session_generation_sha256}))
    }

    pub(crate) fn belongs_to_machine(&self,machine:&MachineControl)->bool{self.shared.control.same_original_machine(machine)}
    pub(crate) fn retains_original_command(&self,actual:&Arc<IngressJob>)->Result<bool>{match self.shared.jobs.try_lock(){
        Ok(jobs)=>Ok(jobs.get(&actual.command.command_key).is_some_and(|j|Arc::ptr_eq(j,actual))),Err(std::sync::TryLockError::WouldBlock)=>Ok(false),Err(_)=>anyhow::bail!("original ingress jobs poisoned; retain close owner")}}
    pub(crate) fn retains_original_fresh(&self,actual:&Arc<FreshInitializationAttempt>)->Result<bool>{match self.shared.fresh_attempt.try_lock(){
        Ok(slot)=>Ok(slot.as_ref().is_some_and(|a|Arc::ptr_eq(a,actual))),Err(std::sync::TryLockError::WouldBlock)=>Ok(false),Err(_)=>anyhow::bail!("original fresh attempt poisoned; retain close owner")}}

    pub(crate) fn take_joined_for_acceptance_retaining(&self,key:&str,out:&mut Option<crate::managed_checkpoint_accept::ManagedCheckpointAcceptance>,original:&FinalizationBudget)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"acceptance transfer requires off App");let job=self.shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).get(key).cloned().context("original task command absent")?;
        let mut held=original_lock(&job.carrier,original)?;ensure!(held.joined.is_some(),"original command callback/TLS not joined");let node=held.node.as_ref().context("actual project absent")?.clone();
        let ticket=original_lock(&node.work,original)?.ticket.as_ref().context("actual project ticket absent")?.clone();
        self.shared.control.stage_joined_acceptance_retaining(out,&mut held.task.joined,&ticket,original)
    }
    pub(crate) fn offer_published_acceptance_retaining(&self,key:&str,original:&mut Option<crate::managed_checkpoint_accept::ManagedCheckpointAcceptance>,budget:&FinalizationBudget)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"Published source transfer requires off App");let job=self.shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).get(key).cloned().context("original task command absent")?;
        let held=original_lock(&job.carrier,budget)?;held.task.scope.as_ref().context("original task source scope absent")?.offer_published_retaining(original,budget)
    }
    pub(crate) fn reconcile_completion_off_app(&self,key:&str)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"receipt reconciliation requires off App");
        let job=self.shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).get(key).cloned().context("original command carrier absent")?;
        let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.joined.is_some(),"original command worker/TLS not joined");
        let mut store=self.shared.receipts.lock().unwrap_or_else(|p|p.into_inner());
        store.as_mut().context("actual receipt store absent")?.reconcile_completion_retaining(held.accepted.as_mut().context("original accepted owner absent")?)?;
        held.settled=true;Ok(())
    }
}
impl ManagedMachineIngress {
    pub(crate) fn capture(host:HostIngress,control:MachineControl)->Self {
        let config=host.config().clone();let (send,receive)=mpsc::sync_channel(APP_EVENTS);
        Self {shared:Arc::new(Shared {
            #[cfg(feature="managed-ingress-genuine-fixture")]
            fixture_gate:OnceLock::new(),
            config,control,fresh_attempt:Mutex::new(None),fresh:Mutex::new(None),fresh_error:Mutex::new(None),transport:Mutex::new(Some(host)),receipts:Mutex::new(None),jobs:Mutex::new(BTreeMap::new()),
            projects:Mutex::new(BTreeMap::new()),sdk_projects:Mutex::new(BTreeMap::new()),canonical_sdk:Mutex::new(None),canonical_projects:Mutex::new(BTreeMap::new()),canonical_previous:Mutex::new(BTreeMap::new()),app_thread:std::thread::current().id(),queue:Mutex::new(VecDeque::with_capacity(APP_EVENTS)),active:AtomicUsize::new(0),
            fenced:AtomicBool::new(false),worker_owned:AtomicBool::new(false),panic:Mutex::new(None),send}),worker:None,receive,pending:Vec::with_capacity(APP_EVENTS),app_thread:std::thread::current().id()}
    }
    pub(crate) fn start_worker_retaining(&mut self)->Result<()> {
        ensure!(self.worker.is_none(),"original ingress worker already attempted");let shared=self.shared.clone();
        let worker=thread::Builder::new().name("rudder-host-ingress".into()).spawn(move||{
            while !shared.worker_owned.load(Ordering::Acquire){thread::yield_now();}
            let result=catch_unwind(AssertUnwindSafe(||io_loop(&shared)));
            if let Err(panic)=result {*shared.panic.lock().unwrap_or_else(|p|p.into_inner())=Some(panic);}
            shared.fenced.store(true,Ordering::Release);
            // This machine owner remains retained on failure. Accepted work and
            // existing Apps/storage are independent; no legacy fallback/kill.
            loop {poll_actual_joins(&shared);sdk_driver::poll(&shared);canonical_driver::poll(&shared);thread::sleep(Duration::from_millis(10));}
        })?;
        self.worker=Some(OwnedWorkerJoin::new(worker));self.shared.worker_owned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn control(&self)->ManagedIngressControl {ManagedIngressControl{shared:self.shared.clone(),app_thread:self.app_thread}}
    pub(crate) fn fence_new_requests(&self) {self.shared.fenced.store(true,Ordering::Release);}
    pub(crate) fn poll_on_app(&mut self,runtime:&mut ManagedMachineRuntime)->Result<()> {
        ensure!(self.app_thread==std::thread::current().id(),"managed ingress App thread changed");
        while self.pending.len()<APP_EVENTS {let Ok(job)=self.receive.try_recv() else {break;};self.pending.push(job);}
        let mut index=0;
        while index<self.pending.len() {
            let event=&self.pending[index];
            let polled=match event {AppEvent::Work(job)=>poll_app_job(job,runtime),AppEvent::Observe(request)=>poll_app_observation(request,runtime)};
            match polled {
                Ok(true)=>{self.pending.swap_remove(index);},Ok(false)=>index+=1,
                Err(error)=>{match &self.pending[index] {
                    AppEvent::Work(job)=>{if let Ok(mut held)=job.carrier.try_lock(){held.error=Some(error);}job.step.store(STEP_REFUSED,Ordering::Release);},
                    AppEvent::Observe(request)=>{request.done.store(true,Ordering::Release);}
                }self.pending.swap_remove(index);}
            }
        }
        // Only pure try-lock/typed adoption and App-only PTY paths here. Every
        // Root enrollment request is executed by existing MachineControl worker.
        let jobs:Vec<_>=match self.shared.jobs.try_lock(){Ok(jobs)=>jobs.values().cloned().collect(),Err(_)=>return Ok(())};
        for job in jobs {poll_app_enrollment(&self.shared,&job,runtime)?;}Ok(())
    }
}
fn io_loop(shared:&Arc<Shared>) {
    let opened=(||->Result<FinalizationBudget>{let original=FinalizationBudget::begin(Duration::from_secs(30))?;
        MachineReceipts::open_retaining(&mut shared.receipts.lock().unwrap_or_else(|p|p.into_inner()),shared.config.clone(),&original)?;Ok(original)})();
    let Ok(_initialization)=opened else{shared.fenced.store(true,Ordering::Release);return;};
    // DB FIRST. This original open ACK precedes any automatic LocalStore/CAS
    // creation/import, preserving InitializeEmpty's actual provenance checks.
    // This new probe has its OWN original30, before its one-slot queue. It
    // cannot precede the genuine DB open ACK because its private child would
    // otherwise violate InitializeEmpty's unknown-prior-data refusal.
    let kernel_request=(||->Result<()>{let original=FinalizationBudget::begin(Duration::from_secs(30))?;
        shared.control.request_kernel_startup_after_receipts(shared.config.clone(),original)})();
    if let Err(error)=kernel_request{*shared.fresh_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error);}
    loop {
        poll_actual_joins(shared);sdk_driver::poll(shared);start_queued_work(shared);
        if shared.control.kernel_startup_status()==crate::managed_machine_entry::KernelStartupStatus::Ready&&crate::managed_fresh_machine::fresh_config(&shared.config){poll_fresh_initialization(shared);}
        if let Some(fresh)=shared.fresh.lock().unwrap_or_else(|p|p.into_inner()).as_ref(){let _=fresh.poll_worker_join_off_app();}
        if shared.control.ingress_worker_serving()&&!shared.fenced.load(Ordering::Acquire) {if receive_one(shared).is_err(){shared.fenced.store(true,Ordering::Release);}}
        retain_joined_completions(shared);canonical_driver::poll(shared);thread::sleep(Duration::from_millis(2));
    }
}
// Single initializer retains its candidate BEFORE admission, original30, Root
// client lookup or coordinator construction. Busy reuses this exact candidate.
fn poll_fresh_initialization(shared:&Arc<Shared>) {
    let actual={let mut slot=shared.fresh_attempt.lock().unwrap_or_else(|p|p.into_inner());
        slot.get_or_insert_with(||FreshInitializationAttempt::capture(shared.config.clone(),shared.control.clone())).clone()};
    {let held=actual.state.lock().unwrap_or_else(|p|p.into_inner());if held.attempted{return;}}
    match shared.control.reserve_fresh_root_retaining(&actual){Ok(false)=>return,Ok(true)=>{},Err(error)=>{
        let mut held=actual.state.lock().unwrap_or_else(|p|p.into_inner());held.attempted=true;held.error=Some(error);return;}}
    let mut held=actual.state.lock().unwrap_or_else(|p|p.into_inner());if held.attempted{return;}held.attempted=true;
    let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
        held.original=Some(FinalizationBudget::begin(Duration::from_secs(30))?);
        let original=held.original.as_ref().unwrap().clone();let cancel=held.cancelled.clone();
        held.kernel=Some(shared.control.kernel_startup_off_app(&original,&cancel)?);
        held.registration=Some(shared.control.ingress_registered_engine(&original)?);
        if actual.config.data().image_preseed.as_ref().and_then(|p|p.runtime_mount.as_ref()).is_some(){loop{time(&original,&cancel)?;
            if let Some(client)=shared.control.runtime_environment_client_off_app(&original,&cancel)?{held.client=Some(client);break;}thread::sleep(original.phase_limit(Duration::from_millis(2))?);}}
        held.coordinator=Some(crate::managed_fresh_machine::ManagedFreshMachine::capture_with_runtime(actual.config.clone(),held.registration.as_ref().unwrap().clone(),held.kernel.as_ref().unwrap().clone(),held.client.clone(),original,cancel,shared.control.clone(),actual.covered_root().context("original fresh root absent")?.clone()));
        let fresh=held.coordinator.as_ref().unwrap().clone();
        actual.coordinator.set(Arc::downgrade(&fresh)).map_err(|_|anyhow::anyhow!("original fresh coordinator association occupied"))?;
        *shared.fresh.lock().unwrap_or_else(|p|p.into_inner())=Some(fresh.clone());fresh.start_retaining()
    }));
    match result{Ok(Ok(()))=>{},Ok(Err(error))=>{held.error=Some(error);eprintln!("MANAGED_FRESH_SOURCE_RECOVERY");},Err(panic)=>held.panic=Some(panic)}
}
fn receive_one(shared:&Arc<Shared>)->Result<()> {
    let mut transport=shared.transport.lock().unwrap_or_else(|p|p.into_inner());let host=transport.as_mut().context("actual host transport absent")?;
    let Some(command)=host.try_receive()? else {return Ok(());};let original=FinalizationBudget::from_constraint(&command.constraint)?;
    let outcome:std::result::Result<serde_json::Value,HostReplyError>=if let HostOperation::ReceiptV1 {lookup_command_key}=&command.operation {
        let actual_job={shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).get(lookup_command_key).cloned()};
        let sdk_observation=actual_job.as_ref().and_then(sdk_driver::observation);
        let observation=if let Some(job)=actual_job.filter(|j|!sdk_driver::is_sdk(&j.command.operation)) {
            let request=Arc::new(ObservationRequest {job,original:original.clone(),result:Mutex::new(None),done:AtomicBool::new(false)});
            shared.send.try_send(AppEvent::Observe(request.clone())).map_err(|_|anyhow::anyhow!("bounded App observation queue busy"))?;
            loop {original.remaining()?;if request.done.load(Ordering::Acquire){break;}thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
            let value=request.result.lock().unwrap_or_else(|p|p.into_inner()).clone();value
        } else {None};
        let row=shared.receipts.lock().unwrap_or_else(|p|p.into_inner()).as_ref().unwrap().receipt(lookup_command_key,&original)?;
        let mut data=serde_json::json!({"receipt":row,"task_observation":observation});if let Some(sdk)=sdk_observation{data["sdk_observation"]=sdk;}Ok(data)
    } else if validate_policy(&shared.config,&command).is_err() {Err(HostReplyError::Rejected)} else {
        let mut receipts=shared.receipts.lock().unwrap_or_else(|p|p.into_inner());let receipts=receipts.as_mut().unwrap();
        match receipts.duplicate_receipt(&command,&original)? {
            DuplicateReceipt::Same(receipt)=>Ok(serde_json::json!({"receipt":receipt})),
            DuplicateReceipt::Conflict=>Err(HostReplyError::Rejected),
            DuplicateReceipt::Missing if shared.control.kernel_startup_status()==crate::managed_machine_entry::KernelStartupStatus::Pending=>Err(HostReplyError::Busy),
            DuplicateReceipt::Missing if shared.control.kernel_startup_status()==crate::managed_machine_entry::KernelStartupStatus::Closed=>Err(HostReplyError::Rejected),
            DuplicateReceipt::Missing if !sdk_driver::available(shared,&command.operation)=>Err(HostReplyError::Busy),
            DuplicateReceipt::Missing if shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).len()>=JOB_LIMIT||shared.queue.lock().unwrap_or_else(|p|p.into_inner()).len()>=APP_EVENTS=>Err(HostReplyError::Busy),
            DuplicateReceipt::Missing=>{
            // Pre-reserve and register the WHOLE actual owner cell BEFORE the
            // acceptance transaction. Every insert/start/ACK error retains it.
            let job=Arc::new(Job {
                #[cfg(feature="managed-ingress-genuine-fixture")]
                fixture_gate:OnceLock::new(),
                command:command.clone(),machine:shared.control.clone(),root:OnceLock::new(),step:AtomicU8::new(STEP_NONE),worker_owned:AtomicBool::new(false),carrier:Mutex::new(JobCarrier {
                accepted:None,original:original.clone(),cancelled:Arc::new(AtomicBool::new(false)),stop_target:None,node:None,profile_inspection:None,task:TaskWork::new(),result:None,error:None,panic:None,worker:None,joined:None,charged:false,settled:false,sdk:None,canonical:None,canonical_wait:None})});
            let mut jobs=shared.jobs.lock().unwrap_or_else(|p|p.into_inner());ensure!(!jobs.contains_key(&command.command_key),"original acceptance uncertain/missing; same carrier retained");
            jobs.insert(command.command_key.clone(),job.clone());drop(jobs);
            #[cfg(feature="managed-ingress-genuine-fixture")]
            if let Some(gate)=shared.fixture_gate.get(){if let Err(error)=gate.after_insert(&job){
                gate.abort();job.carrier.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);
                anyhow::bail!("fixture original insertion refused; candidate retained");}}

            if let Err(error)=shared.control.reserve_ingress_root_retaining(&job,&original){
                job.carrier.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);
                #[cfg(feature="managed-ingress-genuine-fixture")]
                if let Some(gate)=job.fixture_gate(){gate.root_refused();}
                anyhow::bail!("original command root refused; same candidate retained");}
            #[cfg(feature="managed-ingress-genuine-fixture")]
            if let Some(gate)=job.fixture_gate(){if let Err(error)=gate.after_root_pre_effect(){
                gate.abort();job.carrier.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);
                anyhow::bail!("fixture original pre-effect candidate retained");}}

            shared.control.kernel_startup_off_app(&original,&AtomicBool::new(false))?;
            sdk_driver::reserve(shared,&job)?;
            let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
            let receipt=receipts.accept_retaining(&mut held.accepted,&command,&original)?;
            let (work,cancelled)=receipts.reserve_work_retaining(held.accepted.as_mut().context("new original acceptance owner absent")?)?;
            held.original=work;held.cancelled=cancelled;drop(held);
            shared.queue.lock().unwrap_or_else(|p|p.into_inner()).push_back(job);
            Ok(serde_json::json!({"receipt":receipt}))
            }
        }
    };
    let (result,error)=match outcome {Ok(value)=>(Some(value),None),Err(error)=>(None,Some(error))};
    let reply=HostReply {version:1,session_generation:command.session_generation.clone(),rpc_id:command.rpc_id.clone(),command_key:command.command_key.clone(),result,error};
    loop {original.remaining()?;if host.try_reply(&reply)? {return Ok(());}thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
}
fn validate_policy(config:&HostIngressConfigOwner,command:&HostCommand)->Result<()> {
    config.assert_current()?;
    if let HostOperation::StopTaskV1 {project_alias,..}=&command.operation {
        ensure!(config.data().runtime_qualification.is_none()&&config.data().compiler_qualification.is_none(),"qualification scope refuses ordinary stop");
        ensure!(config.data().projects.iter().any(|p|p.alias==*project_alias),"stop project outside actual host config");return Ok(());
    }
    if let HostOperation::BuildCurrentSourceSdkV1{project_alias,..}=&command.operation {ensure!(config.data().projects.iter().any(|p|p.alias==*project_alias)&&config.data().compiler_sdk.is_some()&&config.data().runtime_qualification.is_none()&&config.data().compiler_qualification.is_none(),"SDK request is outside actual ordinary configured project");return Ok(());}
    let (alias,backend,model,profile)=operation_selection(&command.operation)?;
    ensure!(config.data().projects.iter().any(|p|p.alias==alias),"project outside actual host config");
    ensure!(!matches!(backend,HostBackend::Claude),"Claude guest/profile qualification is not admitted");
    ensure!(!model.trim().is_empty()&&!profile.trim().is_empty(),"explicit model/API profile required");
    let artifact=std::path::Path::new("/opt/rudder-managed-runtime/v1").join(match backend {HostBackend::Codex=>"codex",HostBackend::Claude=>"claude"});
    ensure!(config.data().runtime_manifest.entries.iter().any(|e|e.path==artifact),"selected backend is unsupported in declared image");Ok(())
}
fn operation_selection(op:&HostOperation)->Result<(&str,HostBackend,&str,&str)> {match op {
    HostOperation::OpenPreparedProjectV1 {project_alias,backend,model,profile_id}|HostOperation::StartTaskV1 {project_alias,backend,model,profile_id,..}=>Ok((project_alias,*backend,model,profile_id)),_=>anyhow::bail!("query has no effect selection")}}
fn selected(op:&HostOperation)->Result<ManagedTaskOptIn> {let (_,backend,_,profile)=operation_selection(op)?;
    ManagedTaskOptIn::selected(profile.to_owned(),native_backend(backend),if matches!(backend,HostBackend::Codex) {SelectedTaskCredentialKind::SelectedProfileV1}else{SelectedTaskCredentialKind::ApiKeyV1},ManagedTaskNetworkSelection::OrdinaryNetworkV1)}
fn native_backend(backend:HostBackend)->Backend {match backend {HostBackend::Codex=>Backend::Codex,HostBackend::Claude=>Backend::Claude}}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original accepted preparation cancelled");}original.remaining()?;Ok(())}
fn start_queued_work(shared:&Arc<Shared>) {
    if shared.active.load(Ordering::Acquire)>=WORKERS{return;}
    let Some(job)=shared.queue.lock().unwrap_or_else(|p|p.into_inner()).pop_front() else {return;};
    let mut held=job.carrier.lock().unwrap_or_else(|p|p.into_inner());
    if held.worker.is_some(){return;}shared.active.fetch_add(1,Ordering::AcqRel);held.charged=true;
    let actual=job.clone();let service=shared.clone();
    let started=thread::Builder::new().name("rudder-host-command".into()).spawn(move||{
        while !actual.worker_owned.load(Ordering::Acquire){thread::yield_now();}
        let result=catch_unwind(AssertUnwindSafe(||run_command(&service,&actual)));
        let mut held=actual.carrier.lock().unwrap_or_else(|p|p.into_inner());
        match result {Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
    });
    match started {Ok(worker)=>{held.worker=Some(OwnedWorkerJoin::new(worker));job.worker_owned.store(true,Ordering::Release);},Err(error)=>{
        held.error=Some(error.into());held.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);
    }}
}
fn poll_actual_joins(shared:&Arc<Shared>) {
    let jobs:Vec<_>=shared.jobs.lock().unwrap_or_else(|p|p.into_inner()).values().cloned().collect();
    for job in jobs {let mut held=match job.carrier.try_lock(){Ok(h)=>h,Err(_)=>continue};if held.joined.is_some(){continue;}
        if let Some(worker)=&held.worker {let _=worker.start_join_observer();match worker.try_joined() {Ok(Some(joined))=>{
            held.joined=Some(joined);if held.charged {held.charged=false;shared.active.fetch_sub(1,Ordering::AcqRel);}
            // Pure original admission disposal only with actual job/TLS join AND
            // its unspent factory capability. Attempted/unknown states remain.
            if held.task.factory_permit.is_some(){drop(held.task.admission.take());}
        },Ok(None)=>{},Err(error)=>{if held.error.is_none(){held.error=Some(error);}}}}
    }
}
include!("managed_machine_ingress_work.rs");
include!("managed_machine_ingress_app.rs");
#[cfg(test)]
#[path="managed_machine_ingress_lifecycle_tests.rs"]
mod lifecycle_tests;

fn original_lock<'a,T>(owner:&'a Mutex<T>,original:&FinalizationBudget)->Result<std::sync::MutexGuard<'a,T>>{loop{original.remaining()?;match owner.try_lock(){
    Ok(held)=>return Ok(held),Err(std::sync::TryLockError::WouldBlock)=>thread::sleep(original.phase_limit(Duration::from_millis(1))?),Err(_)=>anyhow::bail!("original canonical carrier poisoned")}}}
#[path="managed_canonical_driver.rs"]mod canonical_driver;

#[path="managed_ingress_sdk.rs"]mod sdk_driver;
