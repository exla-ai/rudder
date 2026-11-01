//! One original machine-owned startup probe. This gate admits NEW startup
//! only; it never closes a project lease, broker, App poll, or original cleanup.
use super::*;
use crate::provider_kernel_qualification::{KernelStartupProbe,KernelStartupOwner,KernelProbeFixtureRetired};
use rudder_native::host_ingress::HostIngressConfigOwner;
use rudder_engine::execution::handoff::WorkerJoined;
use std::{fs::{self,File,OpenOptions},os::unix::fs::{MetadataExt,OpenOptionsExt},path::{Path,PathBuf},sync::{OnceLock,Weak}};
const EMPTY:u8=0;const QUEUED:u8=1;const RUNNING:u8=2;const ADMITTED:u8=3;const CLOSED:u8=4;
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum KernelStartupStatus {Pending,Ready,Closed}
struct BeginProbePermit;
struct Directory {path:PathBuf,file:File,id:(u64,u64),uid:u32,gid:u32}
impl Directory {
    fn capture(path:&Path,uid:u32,gid:u32)->Result<Self>{ensure!(path.is_absolute()&&path.canonicalize()?==path,"startup state directory redirects");let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?;let m=file.metadata()?;let value=Self{path:path.into(),file,id:(m.dev(),m.ino()),uid,gid};value.current()?;Ok(value)}
    fn current(&self)->Result<()>{ensure!(self.path.canonicalize()?==self.path,"startup state directory redirects");for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&m.uid()==self.uid&&m.gid()==self.gid&&m.mode()&0o7777==0o700,"original startup state directory changed");}Ok(())}
}
struct Owners {directory:Option<Directory>,unused:Option<BeginProbePermit>,attempted:Option<BeginProbePermit>,probe:Option<KernelStartupProbe>,owner:Option<Arc<KernelStartupOwner>>,
    result:Option<Result<()>>,panic:Option<Box<dyn Any+Send>>,retired:Option<KernelProbeFixtureRetired>,retirement_result:Option<Result<()>>}
struct Fault {error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
struct Worker {original:Option<OwnedWorkerJoin>,owned:Arc<AtomicBool>,attempted:bool}
impl Worker {fn new()->Self{Self{original:None,owned:Arc::new(AtomicBool::new(false)),attempted:false}}fn poll_joined(&self)->Result<Option<WorkerJoined>>{let Some(worker)=&self.original else{return Ok(None);};worker.start_join_observer()?;worker.try_joined()}}
/// Machine retains actual resources independently of delivery and every ticket.
/// No Drop cleanup/join. A constructor/IO timeout cannot free this fixed slot.
pub(super) struct KernelSlot {phase:AtomicU8,closed:AtomicBool,original:OnceLock<FinalizationBudget>,cancelled:Arc<AtomicBool>,
    config:OnceLock<Arc<HostIngressConfigOwner>>,registration:OnceLock<Arc<RegisteredEngineOwner>>,owners:Mutex<Owners>,fault:Mutex<Fault>,
    worker:Mutex<Worker>,joined:OnceLock<WorkerJoined>,staged_owner:OnceLock<Arc<KernelStartupOwner>>,capability:OnceLock<Arc<ManagedKernelStartup>>,
    retirement_clock:OnceLock<FinalizationBudget>,retirement_cancel:Arc<AtomicBool>,retirement:Mutex<Worker>,retirement_joined:OnceLock<WorkerJoined>}
/// No Serde/constructor. Aliases retain the SAME actual completed owner; they
/// add no source, model, project, PG lease, or default-routing authority.
pub(crate) struct ManagedKernelStartup {slot:Weak<KernelSlot>,owner:Arc<KernelStartupOwner>,registration:Arc<RegisteredEngineOwner>}
/// Concrete same-machine restriction, never a bool/callback permission. An
/// empty/failed slot cannot admit; only its genuine completed owner can.
pub(crate) struct ManagedMachineStartupGate {slot:Arc<KernelSlot>}
impl ManagedMachineStartupGate {pub(crate) fn check_on_app(&self)->Result<()>{self.slot.ticket()?.check_on_app()}
    #[cfg(test)]
    pub(crate) fn closed_for_tests()->Self{let slot=KernelSlot::new();slot.close();slot.gate()}
}
impl ManagedKernelStartup {
    pub(crate) fn assert_new_startup_off_app(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()>{
        let result=(||{time(original,cancel)?;self.check_on_app()?;self.owner.assert_startup_admission_off_app()?;
            ensure!(Arc::ptr_eq(self.owner.registration(),&self.registration),"startup owner differs from original registration");self.registration.assert_current()?;self.check_on_app()?;time(original,cancel)})();
        if result.is_err()&&self.owner.check_startup_admission_on_app().is_err(){if let Some(slot)=self.slot.upgrade(){slot.close();}}result
    }
    pub(crate) fn check_on_app(&self)->Result<()>{let slot=self.slot.upgrade().context("original startup machine absent")?;ensure!(!slot.closed.load(Ordering::Acquire)&&slot.phase.load(Ordering::Acquire)==ADMITTED,"new startup admission closed/pending");
        ensure!(slot.registration.get().is_some_and(|r|Arc::ptr_eq(r,&self.registration))&&Arc::ptr_eq(self.owner.registration(),&self.registration),"startup registration owner changed");self.owner.check_startup_admission_on_app()?;
        ensure!(!slot.closed.load(Ordering::Acquire),"new startup admission closed");Ok(())}
    pub(crate) fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}
}
impl KernelSlot {
    pub(super) fn new()->Arc<Self>{Arc::new(Self{phase:AtomicU8::new(EMPTY),closed:AtomicBool::new(false),original:OnceLock::new(),cancelled:Arc::new(AtomicBool::new(false)),config:OnceLock::new(),registration:OnceLock::new(),owners:Mutex::new(Owners{directory:None,unused:Some(BeginProbePermit),attempted:None,probe:None,owner:None,result:None,panic:None,retired:None,retirement_result:None}),fault:Mutex::new(Fault{error:None,panic:None}),worker:Mutex::new(Worker::new()),joined:OnceLock::new(),staged_owner:OnceLock::new(),capability:OnceLock::new(),retirement_clock:OnceLock::new(),retirement_cancel:Arc::new(AtomicBool::new(false)),retirement:Mutex::new(Worker::new()),retirement_joined:OnceLock::new()})}
    fn stage_request(&self,config:Arc<HostIngressConfigOwner>,original:FinalizationBudget)->Result<()> {
        ensure!(self.original.get().is_none()&&self.phase.load(Ordering::Acquire)==EMPTY&&!self.closed.load(Ordering::Acquire),"original kernel request already captured/closed");
        self.original.set(original).map_err(|_|anyhow::anyhow!("original startup clock occupied"))?;self.config.set(config).map_err(|_|anyhow::anyhow!("original startup config occupied"))?;Ok(())
    }
    fn complete_request(&self,registration:Arc<RegisteredEngineOwner>)->Result<()> {
        self.registration.set(registration).map_err(|_|anyhow::anyhow!("original startup registration occupied"))?;
        let original=self.original.get().unwrap();time(original,&self.cancelled)?;let config=self.config.get().unwrap();let registered=self.registration.get().unwrap();config.assert_current()?;registered.assert_current()?;
        ensure!(registered.data().binding.config_sha256==config.config_sha256(),"startup config differs from actual registration");
        let mut held=self.owners.lock().unwrap_or_else(|p|p.into_inner());held.directory=Some(Directory::capture(&config.data().state_directory,1500,1500)?);time(original,&self.cancelled)?;ensure!(!self.closed.load(Ordering::Acquire),"startup closed during request");self.phase.store(QUEUED,Ordering::Release);Ok(())
    }
    pub(super) fn gate(self:&Arc<Self>)->ManagedMachineStartupGate{ManagedMachineStartupGate{slot:self.clone()}}
    pub(super) fn close(&self){self.closed.store(true,Ordering::Release);self.phase.store(CLOSED,Ordering::Release);if let Some(owner)=self.staged_owner.get(){owner.close_startup_admission();}}
    pub(super) fn status(&self)->KernelStartupStatus {if self.closed.load(Ordering::Acquire){return KernelStartupStatus::Closed;}match self.capability.get(){Some(ready) if ready.check_on_app().is_ok()=>KernelStartupStatus::Ready,Some(_)=>{self.close();KernelStartupStatus::Closed},None=>KernelStartupStatus::Pending}}
    pub(super) fn ticket(&self)->Result<Arc<ManagedKernelStartup>>{ensure!(self.status()==KernelStartupStatus::Ready,"new startup pending/closed");let ready=self.capability.get().context("original successful startup absent")?.clone();ready.check_on_app()?;Ok(ready)}
    /// Only the SAME original machine worker calls this. Failures are scoped to
    /// new startup; the machine keeps serving old Apps/Root/lease/cleanup.
    pub(super) fn poll_retaining(self:&Arc<Self>,registered:Option<&Arc<RegisteredEngineOwner>>){
        let result=catch_unwind(AssertUnwindSafe(||self.poll_inner(registered)));
        match result{Ok(Ok(()))=>{},Ok(Err(error))=>{self.close();self.fault.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);},Err(panic)=>{self.close();self.fault.lock().unwrap_or_else(|p|p.into_inner()).panic=Some(panic);}}
    }
    fn poll_inner(self:&Arc<Self>,registered:Option<&Arc<RegisteredEngineOwner>>)->Result<()> {
        if self.original.get().is_none()||self.registration.get().is_none()||self.phase.load(Ordering::Acquire)==EMPTY{return Ok(());}let actual=registered.context("original registered machine absent")?;ensure!(self.registration.get().is_some_and(|r|Arc::ptr_eq(r,actual)),"startup belongs to another registered machine");
        if self.phase.load(Ordering::Acquire)==QUEUED&&!self.closed.load(Ordering::Acquire){self.start_retaining()?;}
        if self.joined.get().is_none(){let worker=self.worker.lock().unwrap_or_else(|p|p.into_inner());if let Some(joined)=worker.poll_joined()?{let _=self.joined.set(joined);}else{return Ok(());}}
        if self.phase.load(Ordering::Acquire)==RUNNING&&!self.closed.load(Ordering::Acquire){
            let original=self.original.get().unwrap();time(original,&self.cancelled)?;let mut held=self.owners.lock().unwrap_or_else(|p|p.into_inner());
            ensure!(held.panic.is_none()&&matches!(held.result.as_ref(),Some(Ok(()))),"actual kernel controller failed; original probe retained");
            // take stages the genuine owner BEFORE all fallible postchecks.
            let Owners{probe,owner,..}=&mut *held;let adoption=probe.as_mut().context("original probe absent")?.take_startup_owner_retaining(owner);
            if let Some(owner)=owner.as_ref(){let _=self.staged_owner.set(owner.clone());}adoption?;
            let owner=held.owner.as_ref().context("actual startup owner absent")?.clone();ensure!(Arc::ptr_eq(owner.registration(),actual),"genuine startup owner registration differs");
            owner.assert_startup_admission_off_app()?;held.directory.as_ref().unwrap().current()?;time(original,&self.cancelled)?;ensure!(!self.closed.load(Ordering::Acquire),"new startup closed before adoption");
            let capability=Arc::new(ManagedKernelStartup{slot:Arc::downgrade(self),owner,registration:actual.clone()});self.capability.set(capability).map_err(|_|anyhow::anyhow!("startup capability occupied"))?;self.phase.store(ADMITTED,Ordering::Release);
            eprintln!("MANAGED_MACHINE_KERNEL_STARTUP_READY"); // bounded DATA observation, never an issuer
        }
        // Physical validation is performed at each NEW startup boundary.
        // Idle/old-work polling never becomes a provider-filesystem monitor.
        // An explicit normal retirement has its own original cleanup clock and
        // genuine receipt. Late joins retain truth, never reopen startup.
        let retirement=self.retirement.lock().unwrap_or_else(|p|p.into_inner());if let Some(joined)=retirement.poll_joined()?{let _=self.retirement_joined.set(joined);}Ok(())
    }
    fn start_retaining(self:&Arc<Self>)->Result<()> {
        let original=self.original.get().unwrap();time(original,&self.cancelled)?;let mut held=self.owners.lock().unwrap_or_else(|p|p.into_inner());
        held.directory.as_ref().unwrap().current()?;let mut worker=self.worker.lock().unwrap_or_else(|p|p.into_inner());ensure!(!worker.attempted,"original startup controller already attempted");worker.attempted=true;
        let permit=held.unused.take().context("original begin permit already spent")?;held.attempted=Some(permit);
        KernelStartupProbe::begin_registered_retaining(&mut held.probe,self.registration.get().unwrap().clone(),self.config.get().unwrap().clone(),original.clone(),self.cancelled.clone())?;
        let cell=self.clone();let owned=worker.owned.clone();let latch=owned.clone();let handle=std::thread::Builder::new().name("rudder-machine-kernel-probe".into()).spawn(move||{while !latch.load(Ordering::Acquire){std::thread::yield_now();}
            let mut held=cell.owners.lock().unwrap_or_else(|p|p.into_inner());let result=catch_unwind(AssertUnwindSafe(||held.probe.as_mut().context("actual startup probe absent")?.run_retaining()));
            match result{Ok(result)=>held.result=Some(result),Err(panic)=>held.panic=Some(panic)}
        })?;worker.original=Some(OwnedWorkerJoin::new(handle));self.phase.store(RUNNING,Ordering::Release);owned.store(true,Ordering::Release);Ok(())
    }
    fn retire_retaining(self:&Arc<Self>,original:FinalizationBudget)->Result<()> {
        self.close();ensure!(self.joined.get().is_some(),"probe constructor/controller TLS still unresolved; original carrier retained");
        {let held=self.owners.lock().unwrap_or_else(|p|p.into_inner());ensure!(held.panic.is_none()&&(held.owner.is_some()||matches!(held.result.as_ref(),Some(Ok(())))),"probe construction/result unresolved; original carrier retained");}
        ensure!(self.retirement_clock.get().is_none(),"original kernel retirement already captured");self.retirement_clock.set(original).map_err(|_|anyhow::anyhow!("kernel retirement clock occupied"))?;
        time(self.retirement_clock.get().unwrap(),&self.retirement_cancel)?;
        let mut worker=self.retirement.lock().unwrap_or_else(|p|p.into_inner());ensure!(!worker.attempted,"original kernel retirement attempted");worker.attempted=true;let cell=self.clone();let owned=worker.owned.clone();let latch=owned.clone();
        let handle=std::thread::Builder::new().name("rudder-machine-kernel-retirement".into()).spawn(move||{while !latch.load(Ordering::Acquire){std::thread::yield_now();}
            let mut held=cell.owners.lock().unwrap_or_else(|p|p.into_inner());let result=catch_unwind(AssertUnwindSafe(||->Result<()>{let original=cell.retirement_clock.get().unwrap();time(original,&cell.retirement_cancel)?;
                let receipt=if let Some(owner)=held.owner.as_ref(){owner.close_startup_admission();owner.retire_own_fixture_retaining(original,&cell.retirement_cancel)?}
                else {ensure!(matches!(held.result.as_ref(),Some(Ok(()))),"original probe success absent");held.probe.as_mut().context("actual completed probe absent")?.retire_own_fixture_retaining(original,&cell.retirement_cancel)?};
                held.retired=Some(receipt);time(original,&cell.retirement_cancel)}));
            match result{Ok(result)=>held.retirement_result=Some(result),Err(panic)=>held.panic=Some(panic)}
        })?;worker.original=Some(OwnedWorkerJoin::new(handle));owned.store(true,Ordering::Release);Ok(())
    }
}
impl MachineControl {
    /// Called only by the fixed ingress AFTER its actual receipt-store open ACK.
    /// The original30 is created before this request/one-slot queue admission.
    pub(crate) fn request_kernel_startup_after_receipts(&self,config:Arc<HostIngressConfigOwner>,original:FinalizationBudget)->Result<()> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"kernel request requires off App");
        self.shared.kernel.stage_request(config,original.clone())?;
        let result=(||{let registered=self.ingress_registered_engine(&original)?;self.shared.kernel.complete_request(registered)})();
        if let Err(error)=result{self.shared.kernel.close();self.shared.kernel.fault.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error);anyhow::bail!("original startup request failed; actual carrier retained");}Ok(())
    }
    pub(crate) fn kernel_startup_status(&self)->KernelStartupStatus{self.shared.kernel.status()}
    pub(crate) fn kernel_startup_off_app(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<Arc<ManagedKernelStartup>>{ensure!(self.shared.app_thread!=std::thread::current().id(),"physical startup check requires off App");let ready=self.shared.kernel.ticket()?;ready.assert_new_startup_off_app(original,cancel)?;Ok(ready)}
    pub(crate) fn close_kernel_startup_admission(&self){self.shared.kernel.close();}
    pub(crate) fn retire_kernel_fixture_off_app(&self,original:FinalizationBudget)->Result<()> {ensure!(self.shared.app_thread!=std::thread::current().id(),"kernel retirement requires off App");self.shared.kernel.retire_retaining(original)}
    pub(crate) fn kernel_fixture_retired_off_app(&self)->Result<bool>{ensure!(self.shared.app_thread!=std::thread::current().id(),"kernel retirement observation requires off App");if self.shared.kernel.retirement_joined.get().is_none(){return Ok(false);}let held=self.shared.kernel.owners.try_lock().map_err(|_|anyhow::anyhow!("actual kernel retirement busy"))?;Ok(held.retired.is_some()&&held.panic.is_none())}
}
#[cfg(test)]
#[path="managed_machine_kernel_startup_tests.rs"]
mod tests;
