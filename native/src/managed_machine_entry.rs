//! FIRST binary-only machine route and retained Root worker/App host. No public
//! configuration ingress, default project/profile/model or scheduler. Explicit
//! retained seed retirement lives in the adjacent ownership module.
#![cfg(target_os="linux")]
use crate::{App, Backend, EffortLevel, InitialSelection, ManagedAppInitialState,
    engine_host::EngineOwnership, managed_app::ManagedTaskLifetimeSupervisor,
    managed_initial_source::ManagedInitialSourceOwner, managed_seed_enrollment::ManagedSeedEnrollment,
    managed_task_launch::{ManagedTaskOptIn,ManagedTaskProject}};
use anyhow::{ensure,Context,Result};
use rudder_engine::{execution::{finalization_budget::FinalizationBudget,handoff::OwnedWorkerJoin},
    journal::{Journal,OwnerFence},store::LocalStore};
use rudder_native::{root_storage_bootstrap::{self,ManagedMachineBootstrap},root_storage_client::ClientSeedOwner,
    engine_registration::RegisteredEngineOwner,host_ingress::HostIngress};
use rudder_vcs_bridge::seed::VerifiedMetadataTemplate;
use crate::managed_app::runtime_task::MachineRuntimeImage;
use std::{any::Any,collections::BTreeMap,ffi::OsString,panic::{catch_unwind,AssertUnwindSafe},
    sync::{Arc,Mutex,TryLockError,atomic::{AtomicBool,AtomicU8,Ordering},mpsc::{self,Receiver,SyncSender}},time::Duration};

const MAX_PROJECTS:usize=2;
const MAX_SEEDS:usize=2;
const MAX_ENROLLMENTS:usize=512;
const QUEUE:usize=8;
const PENDING:u8=0;
const READY:u8=1;
const RECOVERY:u8=2;
const SHARED_FENCED:u8=3;
const STARTUP_CLOSED:u8=4;
const STARTING:u8=0;
const SERVING:u8=1;
const QUARANTINED:u8=2;

fn route_selected(args:&[OsString],bridge:bool,sealed:bool)->bool {
    bridge || sealed || (args.first().is_some_and(|s|s=="engine") && args.get(1).is_some_and(|s|s=="managed-machine"))
}
fn inherited_sealed_data()->bool {
    let seals=unsafe {libc::fcntl(7,libc::F_GET_SEALS)};
    let flags=unsafe {libc::fcntl(7,libc::F_GETFL)};
    let mut stat:libc::stat=unsafe {std::mem::zeroed()};
    let required=libc::F_SEAL_WRITE|libc::F_SEAL_GROW|libc::F_SEAL_SHRINK|libc::F_SEAL_SEAL;
    seals>=0 && seals & required==required && flags>=0 && flags & libc::O_ACCMODE==libc::O_RDONLY
        && unsafe {libc::fstat(7,&mut stat)}==0 && stat.st_mode & libc::S_IFMT==libc::S_IFREG
        && stat.st_uid==0 && stat.st_gid==0 && stat.st_nlink==0 && stat.st_size>0 && stat.st_size<=4096
}
fn inherited_launcher_witness()->bool {
    // Routing DATA only: even a malformed sealed-data FD cannot send an
    // already received launcher/pidfd family into the legacy/default branch.
    let mut held:libc::stat=unsafe {std::mem::zeroed()};
    if unsafe {libc::fstat(6,&mut held)}!=0 || held.st_uid!=0 || held.st_gid!=0 {return false;}
    use std::os::unix::fs::MetadataExt;
    let Ok(named)=std::fs::metadata(root_storage_bootstrap::LAUNCHER_PATH) else {return false;};
    named.is_file() && named.dev()==held.st_dev && named.ino()==held.st_ino
        && std::fs::read_link("/proc/self/fd/5").is_ok_and(|link|link==std::path::Path::new("anon_inode:[pidfd]"))
}
/// Raw argv/kernel inspection ONLY. Call before --smoke, env/config and all
/// legacy routes. Finding DATA only claims quarantine routing, never authority.
pub(crate) fn selected_early()->bool {
    if root_storage_bootstrap::claim_managed_machine_root_entry_first() {return true;}
    let args=std::env::args_os().skip(1).take(2).collect::<Vec<_>>();
    route_selected(&args,false,inherited_sealed_data()||inherited_launcher_witness())
}
fn time(budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    if cancelled.load(Ordering::Acquire){budget.cancel();anyhow::bail!("managed machine request cancelled");}budget.remaining()?;Ok(())
}
struct Cell<T> {phase:AtomicU8,owner:Mutex<T>}
impl<T> Cell<T> {fn new(owner:T)->Arc<Self> {Arc::new(Self {phase:AtomicU8::new(PENDING),owner:Mutex::new(owner)})}}

/// Explicit display DATA only. No implicit selection or credential lookup.
pub(crate) struct ManagedProjectDisplay {pub backend:Backend,pub model:String,pub effort:Option<EffortLevel>,pub session_started_iso:String}
impl ManagedProjectDisplay {
    fn check(&self,selected:&ManagedTaskOptIn)->Result<()> {
        ensure!(self.backend==selected.backend() && !self.model.trim().is_empty() && self.model.len()<=512
            && !self.model.chars().any(char::is_control) && !self.session_started_iso.is_empty() && self.session_started_iso.len()<=64
            && !self.session_started_iso.chars().any(char::is_control),"managed project display/selection invalid");Ok(())
    }
}
struct ProjectCarrier {
    owner:EngineOwnership,selected:ManagedTaskOptIn,display:Option<ManagedProjectDisplay>,initial:Option<ManagedAppInitialState>,
    project:Option<Arc<ManagedTaskProject>>,supervisor:Option<Arc<ManagedTaskLifetimeSupervisor>>,current_source:Option<source_generations::CurrentSource>,
    budget:FinalizationBudget,cancelled:Arc<AtomicBool>,panic:Option<Box<dyn Any+Send>>,
}
#[derive(Clone)]
pub(crate) struct ProjectTicket {id:String,cell:Arc<Cell<ProjectCarrier>>}
impl ProjectTicket {
    /// Pure transfer of an ACTUAL already prepared owner. Construct on the
    /// native host worker; saved paths/JSON cannot construct EngineOwnership.
    pub(crate) fn capture(owner:EngineOwnership,selected:ManagedTaskOptIn,display:ManagedProjectDisplay,
        budget:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self {
        Self {id:uuid::Uuid::new_v4().to_string(),cell:Cell::new(ProjectCarrier {owner,selected,display:Some(display),initial:None,
            project:None,supervisor:None,current_source:None,budget,cancelled,panic:None})}
    }
    /// OFF App retaining overload. Complete all fallible DATA/clock checks and
    /// nonce creation before moving the actual owner from caller-held storage.
    pub(crate) fn capture_retaining(out:&mut Option<Self>,owner:&mut Option<EngineOwnership>,selected:ManagedTaskOptIn,display:ManagedProjectDisplay,
        budget:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none()&&owner.is_some(),"original project capture output/owner unavailable");time(&budget,&cancelled)?;display.check(&selected)?;
        let id=uuid::Uuid::new_v4().to_string();loop{time(&budget,&cancelled)?;match owner.as_ref().unwrap().check_shared_for_app(){Ok(())=>break,Err(error) if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?),Err(error)=>return Err(error)}}time(&budget,&cancelled)?;
        *out=Some(Self{id,cell:Cell::new(ProjectCarrier{owner:owner.take().unwrap(),selected,display:Some(display),initial:None,project:None,supervisor:None,current_source:None,budget,cancelled,panic:None})});Ok(())
    }
    pub(crate) fn id(&self)->&str {&self.id}
}
#[path="managed_machine_seed.rs"]
mod seeds;
pub(crate) use seeds::{ManagedSeedCowFactory,SeedTicket,SeedTaskTicket,SeedCacheTicket,ManagedSeedTaskSource,SeedRetirementProgress,NeverStartedProgress,EnrollmentTicket};
enum Job {Project(ProjectTicket),Seed(SeedTicket),Enroll(EnrollmentTicket)}
struct WorkerCarrier {bootstrap:ManagedMachineBootstrap,projects:BTreeMap<String,ProjectTicket>,seeds:BTreeMap<String,SeedTicket>,retired_seeds:BTreeMap<String,SeedTicket>,
    enrollments:BTreeMap<String,EnrollmentTicket>,root_rpc_held:bool,root_private_held:bool,registered_engine:Option<Arc<RegisteredEngineOwner>>,runtime_image:MachineRuntimeImage,
    host_offered:bool,host_error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>,
    generations:BTreeMap<String,Arc<source_generations::SourceGeneration>>,
    source_reservations:BTreeMap<String,Arc<source_generations::RootSourceReservation>>}
struct Shared {phase:AtomicU8,worker_owned:AtomicBool,app_thread:std::thread::ThreadId,carrier:Mutex<WorkerCarrier>,receive:Mutex<Receiver<Job>>,
    host_offer:Mutex<Option<HostIngress>>,send:SyncSender<Job>,kernel:Arc<kernel_startup::KernelSlot>}

fn prepare_project(ticket:&ProjectTicket)->Result<()> {
    let mut c=ticket.cell.owner.lock().map_err(|_|anyhow::anyhow!("managed project carrier poisoned"))?;
    let result=catch_unwind(AssertUnwindSafe(||->Result<()> {loop{let step=(||->Result<()> {
        time(&c.budget,&c.cancelled)?;c.owner.assert_current_bounded(&c.budget,&c.cancelled)?;ensure!(c.owner.is_fabric(),"managed machine requires actual Fabric owner");
        if let Some(display)=&c.display{display.check(&c.selected)?;}
        let namespace=c.owner.managed_recovery_store().context("explicit managed project preparation required")?.binding().clone();
        let selected=c.selected.clone();let original=c.budget.clone();let cancel=c.cancelled.clone();if c.project.is_none(){c.project=Some(ManagedTaskProject::capture_bounded(&mut c.owner,&selected,&original,&cancel)?);}
        time(&c.budget,&c.cancelled)?;let fence=c.owner.journal_parts().1.clone();
        if c.supervisor.is_none(){let lease=c.owner.source_lease(&fence)?;let writer=c.owner.source_journal_factory(&fence)?;
        c.supervisor=Some(ManagedTaskLifetimeSupervisor::new_off_app(lease,writer)?);}
        time(&c.budget,&c.cancelled)?;c.owner.assert_current_bounded(&c.budget,&c.cancelled)?;
        let head=c.owner.journal_parts().0.project_state(&fence.project_id)?.integration_head;
        if c.initial.is_none(){let display=c.display.take().unwrap();c.initial=Some(ManagedAppInitialState {repository:namespace.repository,
            selection:InitialSelection {backend:display.backend,model:display.model,effort:display.effort},branch:head,session_started_iso:display.session_started_iso});}
        time(&c.budget,&c.cancelled)?;Ok(())
        })();match step{Err(error) if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>{time(&c.budget,&c.cancelled)?;std::thread::sleep(c.budget.phase_limit(Duration::from_millis(2))?);},other=>return other}
    }}));
    match result {Ok(Ok(()))=>{ticket.cell.phase.store(READY,Ordering::Release);Ok(())},Ok(Err(_))=>{if c.owner.shared_admission_closed()&&c.owner.assert_original_local_custody().is_ok(){ticket.cell.phase.store(SHARED_FENCED,Ordering::Release);Ok(())}else{ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed project preparation failed; original owner retained"))}},
        Err(panic)=>{c.panic=Some(panic);ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed project preparation panicked; original owner retained"))}}
}
fn worker(shared:&Arc<Shared>) {
    let initialized=(||->Result<()> {let mut c=shared.carrier.lock().map_err(|_|anyhow::anyhow!("machine carrier poisoned"))?;c.bootstrap.check_current_retaining()})();
    if initialized.is_err() {shared.phase.store(QUARANTINED,Ordering::Release);return;}
    while !shared.worker_owned.load(Ordering::Acquire) {
        if shared.phase.load(Ordering::Acquire)==QUARANTINED {return;}
        std::thread::sleep(Duration::from_millis(1));
    }
    let acknowledged=(||->Result<()> {let mut c=shared.carrier.lock().map_err(|_|anyhow::anyhow!("machine carrier poisoned"))?;
        c.bootstrap.check_current_retaining()?;c.bootstrap.acknowledge_host_serving_retaining()})();
    if acknowledged.is_err(){shared.phase.store(QUARANTINED,Ordering::Release);return;}
    shared.phase.store(SERVING,Ordering::Release);
    loop {
        if shared.phase.load(Ordering::Acquire)==QUARANTINED {return;}
        let job=shared.receive.lock().unwrap_or_else(|p|p.into_inner()).recv_timeout(Duration::from_millis(100));
        let mut carrier=shared.carrier.lock().unwrap_or_else(|p|p.into_inner());
        let outcome=catch_unwind(AssertUnwindSafe(||->Result<()> {
            carrier.bootstrap.check_current_retaining()?;
            // Registration follows the original worker's HostServing ACK. An
            // early None is pending, never permanent absence. Keep every actual
            // owner on this worker before offering it to the App loop.
            if !carrier.host_offered && carrier.host_error.is_none() {
                let registration=(||->Result<()> {
                    if carrier.registered_engine.is_none() {
                        if let Some(owner)=carrier.bootstrap.poll_registered_engine_retaining()? {
                            carrier.registered_engine=Some(owner);
                        }
                    }
                    if let Some(owner)=carrier.registered_engine.as_ref() {
                        owner.assert_current()?;
                        if let Some(host)=carrier.bootstrap.take_host_ingress_retaining()? {
                            let mut offer=shared.host_offer.lock().unwrap_or_else(|p|p.into_inner());
                            *offer=Some(host); // Stage actual HostIngress BEFORE fallible policy capture.
                            carrier.runtime_image.observe_config_retaining(offer.as_ref().unwrap().config().clone())?;
                            carrier.host_offered=true;
                        }
                    }
                    Ok(())
                })();
                if let Err(error)=registration {
                    carrier.host_error=Some(error);
                    eprintln!("MANAGED_MACHINE_INGRESS_FENCED");
                }
            }
            {let WorkerCarrier {bootstrap,runtime_image,..}=&mut *carrier;runtime_image.poll_retaining(bootstrap)?;}
            shared.kernel.poll_retaining(carrier.registered_engine.as_ref());
            for ticket in carrier.projects.values() {
                if matches!(ticket.cell.phase.load(Ordering::Acquire),READY|SHARED_FENCED) {
                    match ticket.cell.owner.try_lock() {
                        Ok(c)=>if c.owner.shared_admission_busy(){/* original replacement barrier; no permanent fence */}else if c.owner.shared_admission_closed(){c.owner.assert_original_local_custody()?;ticket.cell.phase.store(SHARED_FENCED,Ordering::Release);}else if let Err(error)=c.owner.assert_current(){if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_none(){if c.owner.shared_admission_closed(){c.owner.assert_original_local_custody()?;ticket.cell.phase.store(SHARED_FENCED,Ordering::Release);}else{ticket.cell.phase.store(RECOVERY,Ordering::Release);anyhow::bail!("managed project original authority changed");}}},
                        Err(TryLockError::WouldBlock)=>{},Err(_)=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);anyhow::bail!("managed project actual carrier poisoned");}
                    }
                }
            }
            // Source maintenance errors retain their exact carriers and block
            // new baseline admission; existing original App/Root polling lives.
            if source_generations::poll(&mut carrier,shared).is_err(){eprintln!("MANAGED_SOURCE_GENERATION_RECOVERY");}
            match job {
                Ok(Job::Seed(ticket)) if ticket.cell.phase.load(Ordering::Acquire)!=PENDING=>Ok(()),
                Ok(Job::Seed(ticket)) if carrier.root_rpc_held||carrier.root_private_held=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Ok(())},
                Ok(Job::Enroll(ticket)) if seeds::enrollment_closed(&ticket)=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Ok(())},
                Ok(Job::Enroll(ticket)) if carrier.root_rpc_held||carrier.root_private_held=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Ok(())},
                Ok(Job::Project(ticket))=>if shared.kernel.ticket().and_then(|ready|{let held=ticket.cell.owner.lock().unwrap_or_else(|p|p.into_inner());ready.assert_new_startup_off_app(&held.budget,&held.cancelled)}).is_err(){ticket.cell.phase.store(STARTUP_CLOSED,Ordering::Release);Ok(())}else{prepare_project(&ticket)},
                Ok(Job::Seed(ticket)) if seeds::kernel_startup_for_seed(&shared.kernel,&ticket).is_err()=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Ok(())},
                Ok(Job::Seed(ticket))=>match seeds::prepare_seed(&mut carrier.bootstrap,&ticket){Ok(())=>Ok(()),Err(error)=>{
                    let private_held={let client=carrier.bootstrap.client_mut_retaining()?;
                        if client.assert_authenticated_current().is_err(){client.retain_original_v2_private_io_custody()?;true}else{false}};
                    if private_held {seeds::known_generation_source_custody(&ticket)?;carrier.root_private_held=true;return Ok(());}
                    carrier.bootstrap.client_mut_retaining()?.assert_authenticated_current()?;
                    if seeds::only_seed_shared_fence_recovery(&ticket)?
                        ||source_generations::is_generation_seed(&carrier,&ticket)&&seeds::known_generation_source_custody(&ticket)?{Ok(())}else{Err(error)}}},
                Ok(Job::Enroll(ticket)) if shared.kernel.status()!=KernelStartupStatus::Ready=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Ok(())},
                Ok(Job::Enroll(ticket))=>match {let WorkerCarrier {bootstrap,runtime_image,..}=&mut *carrier;seeds::enroll(bootstrap,runtime_image,&ticket)}{Ok(())=>Ok(()),Err(error)=>{
                    let private_held={let client=carrier.bootstrap.client_mut_retaining()?;
                        if client.assert_authenticated_current().is_err(){client.retain_original_v2_private_io_custody()?;true}else{false}};
                    if private_held {carrier.root_private_held=true;return Ok(());}
                    carrier.bootstrap.client_mut_retaining()?.assert_authenticated_current()?;
                    if seeds::only_enrollment_shared_fence_recovery(&ticket)?{Ok(())}else{Err(error)}}},
                Err(mpsc::RecvTimeoutError::Timeout)=>Ok(()),
                Err(_)=>anyhow::bail!("managed machine host channel closed")}
        }));
        if !matches!(outcome,Ok(Ok(()))) {if let Err(panic)=outcome {carrier.panic=Some(panic);}
            shared.phase.store(QUARANTINED,Ordering::Release);return;}
    }
}

#[derive(Clone)]
pub(crate) struct MachineControl {shared:Arc<Shared>,send:SyncSender<Job>}
impl MachineControl {
    /// Nonblocking host transfer. Err returns the SAME ticket; a registered
    /// failed queue also stays in the machine map and cannot be retried.
    pub(crate) fn submit_project(&self,ticket:ProjectTicket)->std::result::Result<ProjectTicket,(ProjectTicket,anyhow::Error)> {
        let result=(||->Result<()> {
            ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"machine worker unavailable");self.shared.kernel.ticket()?.check_on_app()?;
            let mut c=self.shared.carrier.try_lock().map_err(|_|anyhow::anyhow!("machine worker busy; caller retains project"))?;
            ensure!(c.projects.len()<MAX_PROJECTS && !c.projects.contains_key(&ticket.id)
                && ticket.cell.phase.load(Ordering::Acquire)==PENDING,"managed project admission full/already attempted");
            c.projects.insert(ticket.id.clone(),ticket.clone());
            if self.send.try_send(Job::Project(ticket.clone())).is_err() {ticket.cell.phase.store(RECOVERY,Ordering::Release);anyhow::bail!("managed project queue failed; original retained");}Ok(())
        })();match result {Ok(())=>Ok(ticket),Err(error)=>Err((ticket,error))}
    }

}
struct ProjectApp {ticket:ProjectTicket,app:Option<App>}
/// Borrow exists only on the original App thread; obtaining it consumes no
/// caller packet/secret/helper owner. Use the existing typed task APIs directly.
pub(crate) struct ManagedProjectApp<'a> {app:&'a mut App,carrier:std::sync::MutexGuard<'a,ProjectCarrier>,kernel:Arc<kernel_startup::KernelSlot>}
impl ManagedProjectApp<'_> {
    pub(crate) fn check_new_startup_admission(&self)->Result<()>{self.kernel.ticket()?.check_on_app()}
    pub(crate) fn startup_gate_on_app(&self)->ManagedMachineStartupGate{self.kernel.gate()}
    pub(crate) fn task_parts(&mut self)->(&mut App,Arc<ManagedTaskProject>,&mut Journal,&OwnerFence) {
        let project=self.carrier.project.as_ref().unwrap().clone();let (journal,owner)=self.carrier.owner.journal_parts();
        (&mut *self.app,project,journal,owner)
    }
    fn poll(&mut self)->Result<()> {
        let (journal,owner)=self.carrier.owner.journal_parts();
        crate::managed_source_app::poll(self.app,owner);crate::managed_app::poll_with_startup_gate(self.app,journal,owner,&self.kernel.gate())?;
        crate::fabric_runtime::poll(self.app,journal,owner)?;self.app.poll_agents();Ok(())
    }
}

/// Main owns Apps and the ORIGINAL Root worker join handle; worker owns all
/// Root IO, initial-source/seed/helper owners and project bootstrap IO. No Drop
/// cleanup/join. Run/failure retains this whole actual runtime until recovery.
pub(crate) struct ManagedMachineRuntime {
    shared:Arc<Shared>,control:MachineControl,worker:Option<OwnedWorkerJoin>,apps:BTreeMap<String,ProjectApp>,app_thread:std::thread::ThreadId,
    panic:Option<Box<dyn Any+Send>>,
}
impl ManagedMachineRuntime {
    pub(crate) fn capture(bootstrap:ManagedMachineBootstrap)->Self {
        let (send,receive)=mpsc::sync_channel(QUEUE);
        let shared=Arc::new(Shared {phase:AtomicU8::new(STARTING),worker_owned:AtomicBool::new(false),app_thread:std::thread::current().id(),receive:Mutex::new(receive),host_offer:Mutex::new(None),send:send.clone(),kernel:kernel_startup::KernelSlot::new(),carrier:Mutex::new(WorkerCarrier {
            bootstrap,projects:BTreeMap::new(),seeds:BTreeMap::new(),retired_seeds:BTreeMap::new(),enrollments:BTreeMap::new(),root_rpc_held:false,root_private_held:false,
            registered_engine:None,runtime_image:MachineRuntimeImage::new(std::thread::current().id()),host_offered:false,host_error:None,panic:None,generations:BTreeMap::new(),source_reservations:BTreeMap::new()})});
        Self {control:MachineControl {shared:shared.clone(),send},shared,worker:None,apps:BTreeMap::new(),app_thread:std::thread::current().id(),panic:None}
    }
    /// Spawn borrows an already retained carrier. Ordinary returned Err cannot
    /// drop the actual bootstrap; actual JoinHandle is stored before any App use.
    pub(crate) fn start_worker_retaining(&mut self)->Result<()> {
        ensure!(self.worker.is_none() && self.shared.phase.load(Ordering::Acquire)==STARTING,"managed machine worker already attempted");
        let shared=self.shared.clone();
        let started=std::thread::Builder::new().name("rudder-machine-root-client".into()).spawn(move || {
            if let Err(panic)=catch_unwind(AssertUnwindSafe(||worker(&shared))) {
                shared.carrier.lock().unwrap_or_else(|p|p.into_inner()).panic=Some(panic);
                shared.phase.store(QUARANTINED,Ordering::Release);
            }
            // Main and this thread keep the SAME actual carrier, including on
            // lost input/results. This is not a worker/backend drain receipt.
            loop {std::hint::black_box(&shared);std::thread::sleep(Duration::from_secs(1));}
        });
        match started {Ok(worker)=>{self.worker=Some(OwnedWorkerJoin::new(worker));self.shared.worker_owned.store(true,Ordering::Release);Ok(())},Err(_)=>{
            self.shared.phase.store(QUARANTINED,Ordering::Release);Err(anyhow::anyhow!("managed machine worker spawn failed; bootstrap retained"))}}
    }
    pub(crate) fn control(&self)->MachineControl {self.control.clone()}
    fn take_host_offer_on_app(&self)->Result<Option<HostIngress>> {
        self.app_thread()?;
        match self.shared.host_offer.try_lock() {
            Ok(mut offer)=>Ok(offer.take()),Err(TryLockError::WouldBlock)=>Ok(None),
            Err(_)=>anyhow::bail!("managed machine host offer poisoned"),
        }
    }
    fn app_thread(&self)->Result<()> {ensure!(self.app_thread==std::thread::current().id(),"managed App used from another thread");Ok(())}
    pub(crate) fn project_adapter(&mut self,ticket:&ProjectTicket)->Result<Option<ManagedProjectApp<'_>>> {
        self.app_thread()?;ensure!(self.worker.is_some() && self.shared.phase.load(Ordering::Acquire)==SERVING,"managed machine worker unavailable/quarantined");
        let Some(slot)=self.apps.get_mut(&ticket.id) else {return Ok(None);};
        ensure!(Arc::ptr_eq(&slot.ticket.cell,&ticket.cell),"managed project ticket belongs to another actual owner");
        if slot.ticket.cell.phase.load(Ordering::Acquire)!=READY || slot.app.is_none() {return Ok(None);}
        let carrier=match slot.ticket.cell.owner.try_lock() {Ok(c)=>c,Err(TryLockError::WouldBlock)=>return Ok(None),
            Err(_)=>anyhow::bail!("managed project carrier poisoned")};
        if let Err(error)=carrier.owner.check_shared_for_app(){if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some(){return Ok(None);}if carrier.owner.shared_admission_closed(){slot.ticket.cell.phase.store(SHARED_FENCED,Ordering::Release);return Ok(None);}return Err(error);}
        Ok(Some(ManagedProjectApp {app:slot.app.as_mut().context("managed App not installed")?,carrier,kernel:self.shared.kernel.clone()}))
    }
    pub(crate) fn poll_apps_on_app(&mut self)->Result<()> {
        self.app_thread()?;ensure!(self.worker.is_some(),"managed machine worker handle absent");
        if self.shared.phase.load(Ordering::Acquire)==STARTING {return Ok(());}
        ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"managed machine quarantined; actual Apps retained");
        let tickets=match self.shared.carrier.try_lock() {Ok(c)=>c.projects.values().cloned().collect::<Vec<_>>(),
            Err(TryLockError::WouldBlock)=>Vec::new(),Err(_)=>anyhow::bail!("managed machine carrier poisoned")};
        for ticket in tickets {self.apps.entry(ticket.id.clone()).or_insert(ProjectApp {ticket,app:None});}
        for slot in self.apps.values_mut() {
            let phase=slot.ticket.cell.phase.load(Ordering::Acquire);
            ensure!(phase!=RECOVERY,"managed project requires recovery; original App/owner retained");
            if phase!=READY&&phase!=SHARED_FENCED&&phase!=STARTUP_CLOSED {continue;}
            let mut carrier=match slot.ticket.cell.owner.try_lock() {Ok(c)=>c,Err(TryLockError::WouldBlock)=>continue,
                Err(_)=>anyhow::bail!("managed project carrier poisoned")};
            if phase==SHARED_FENCED||phase==STARTUP_CLOSED {
                // Existing original work keeps its PTY/backend/actor/completion
                // polling. No App is created, no new task adopted and no state
                // is relabeled Paused/stopped/settled. Shared writer/start gates
                // enforce the actual retained lease independently.
                if let Some(app)=slot.app.as_mut(){ManagedProjectApp{app,carrier,kernel:self.shared.kernel.clone()}.poll()?;}
                continue;
            }
            if slot.app.is_none() {
                if self.shared.kernel.status()!=KernelStartupStatus::Ready{continue;}
                if let Err(error)=carrier.owner.check_shared_for_app(){if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some(){continue;}if carrier.owner.shared_admission_closed(){slot.ticket.cell.phase.store(SHARED_FENCED,Ordering::Release);continue;}return Err(error);}
                time(&carrier.budget,&carrier.cancelled)?;
                let initial=carrier.initial.take().context("managed project initial display absent")?;
                slot.app=Some(App::new_managed(initial));
                slot.app.as_mut().unwrap().configure_managed_task_supervisor(carrier.supervisor.as_ref().context("actual managed supervisor absent")?.clone())?;
                time(&carrier.budget,&carrier.cancelled)?;
            }
            ManagedProjectApp {app:slot.app.as_mut().unwrap(),carrier,kernel:self.shared.kernel.clone()}.poll()?;
        }Ok(())
    }
    pub(crate) fn quarantine(&mut self) {self.shared.phase.store(QUARANTINED,Ordering::Release);}
}

/// Diverges after ANY managed selection/capture, including failure. The real
/// worker/Apps remain retained. No legacy serve/default/smoke fallback exists.
pub(crate) fn run_selected()->! {
    let bootstrap=match root_storage_bootstrap::enter_managed_machine_early() {Ok(owner)=>owner,Err(failure)=>failure.retain_quarantined()};
    let mut runtime=ManagedMachineRuntime::capture(bootstrap);
    let mut ingress_error=None;
    let mut ingress:Option<crate::managed_machine_ingress::ManagedMachineIngress>=None;
    if runtime.start_worker_retaining().is_err() {runtime.quarantine();eprintln!("MANAGED_MACHINE_WORKER_QUARANTINED");}
    loop {
        if runtime.shared.phase.load(Ordering::Acquire)==QUARANTINED {
            if let Some(actual)=ingress.as_ref(){actual.fence_new_requests();}
            eprintln!("MANAGED_MACHINE_RUNTIME_QUARANTINED");
            loop {std::hint::black_box((&runtime,&ingress,&ingress_error));std::thread::sleep(Duration::from_secs(1));}
        }
        let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
            runtime.poll_apps_on_app()?;
            // The original Root worker sends HostServing only after its actual
            // JoinHandle latch. Start command effects only once that is true.
            if runtime.shared.phase.load(Ordering::Acquire)==SERVING {
                if ingress.is_none() {
                    if let Some(host)=runtime.take_host_offer_on_app()? {
                        ingress=Some(crate::managed_machine_ingress::ManagedMachineIngress::capture(host,runtime.control()));
                        let actual=ingress.as_mut().unwrap();
                        if let Err(error)=actual.start_worker_retaining(){actual.fence_new_requests();ingress_error=Some(error);}
                    }
                }
                if let Some(actual)=ingress.as_mut(){if let Err(error)=actual.poll_on_app(&mut runtime){actual.fence_new_requests();ingress_error=Some(error);}}
            }
            Ok(())
        }));
        match result {Ok(Ok(()))=>{},Ok(Err(_))=>runtime.quarantine(),Err(panic)=>{runtime.panic=Some(panic);runtime.quarantine();}}
        std::hint::black_box((&runtime,&ingress,&ingress_error));std::thread::sleep(Duration::from_millis(33));
    }
}

#[cfg(test)]
#[path="managed_machine_entry_tests.rs"]
mod tests;

include!("managed_machine_ingress_services.rs");

#[path="managed_source_generations.rs"]
mod source_generations;
pub(crate) use source_generations::CurrentSourceAdmission;

#[path="managed_machine_kernel_startup.rs"]
mod kernel_startup;
pub(crate) use kernel_startup::{ManagedKernelStartup,ManagedMachineStartupGate,KernelStartupStatus};
