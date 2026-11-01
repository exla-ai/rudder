//! Retained machine seed/task/cache ownership. All blocking methods are OFF App;
//! original factory joins and Root ACKs remain separate from display phases.
use super::*;
use crate::{managed_seed_fork::{SeedAdmission,SeedFactoryOwner,ManagedSeedForkCell},
    managed_task_launch::PreparedManagedTask,managed_app::{JoinedManagedTask,ManagedTaskStartFailure,NeverStartedManagedTask,ManagedTaskLifetimeSupervisor},
    managed_checkpoint_accept::ManagedCheckpointAcceptance,managed_root_seed_retirement::ManagedSeedTaskDrain,
    managed_preparation_cache::ManagedPreparationCache};
use rudder_native::root_storage_uses::{SeedUseGuard,SeedUseKind,RetainedSeedJoin,RetainedLocalizedFork,SeedTaskUseGate,SealedSeedTaskUse,RetainedUnusedFork};
use rudder_engine::protocol::ObjectRef;
use std::{sync::Weak,thread::{JoinHandle,ThreadId}};
const CLOSING:u8=3;
const RETIRED:u8=4;
const MAX_TASK_SCOPES:usize=128; // two registered uses per task + bounded host scopes.
const MAX_CACHE_SCOPES:usize=2;
const MAX_RETIRED_SEEDS:usize=2;

pub(super) struct SeedCarrier {
    source:ManagedInitialSourceOwner,store:LocalStore,budget:FinalizationBudget,cancelled:Arc<AtomicBool>,
    client_owner:Option<ClientSeedOwner>,template:Option<Arc<VerifiedMetadataTemplate>>,template_guard:Option<SeedUseGuard>,
    template_value:Option<TemplateClose>,template_close:Option<RetainedSeedJoin<TemplateClose>>,template_started:bool,
    admission:Arc<SeedAdmission>,tasks:BTreeMap<String,SeedTaskTicket>,caches:BTreeMap<String,SeedCacheTicket>,
    retirement:Option<Campaign>,panic:Option<Box<dyn Any+Send>>,
}
struct Campaign {original:FinalizationBudget,cancelled:Arc<AtomicBool>}
#[derive(Clone)]
pub(crate) struct SeedTicket {pub(super) id:String,pub(super) cell:Arc<Cell<SeedCarrier>>}
impl SeedTicket {
    pub(crate) fn capture(source:ManagedInitialSourceOwner,store:LocalStore,budget:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self {
        Self {id:uuid::Uuid::new_v4().to_string(),cell:Cell::new(SeedCarrier {source,store,budget,cancelled,client_owner:None,
            template:None,template_guard:None,template_value:None,template_close:None,template_started:false,
            admission:SeedAdmission::new(),tasks:BTreeMap::new(),caches:BTreeMap::new(),retirement:None,panic:None})}
    }
    /// Legacy compatibility: unscoped aliases irreversibly block retirement.
    /// New callers use source_for_factory after pre-admitted original custody.
    pub(crate) fn template_if_ready(&self)->Result<Option<Arc<VerifiedMetadataTemplate>>> {
        let phase=self.cell.phase.load(Ordering::Acquire);ensure!(phase!=RECOVERY&&phase!=RETIRED&&phase!=CLOSING,"seed no longer admits aliases");
        if phase!=READY {return Ok(None);}
        match self.cell.owner.try_lock() {Ok(owner)=>{owner.admission.check_open()?;
            owner.client_owner.as_ref().context("actual seed owner absent")?.latch_untracked_native_use();Ok(owner.template.clone())},
            Err(TryLockError::WouldBlock)=>Ok(None),Err(_)=>anyhow::bail!("seed carrier poisoned")}
    }
}
#[derive(Clone)]
pub(crate) struct SeedTaskTicket {id:String,seed_id:String,seed:Weak<Cell<SeedCarrier>>,cell:Arc<Cell<TaskScope>>,admission:Arc<SeedAdmission>,original:FinalizationBudget,app_thread:ThreadId}
struct TaskScope {
    factory:SeedFactoryOwner,start:Option<SeedTaskUseGate>,factory_outcome:Option<Result<PreparedManagedTask>>,factory_refusal:Option<ManagedTaskStartFailure>,never:Option<NeverStartedScope>,guard:Option<SeedUseGuard>,fork_guard:Option<SeedUseGuard>,fork:Option<Arc<ManagedSeedForkCell>>,
    expected_run:Option<String>,bound:bool,enrollment:Option<EnrollmentTicket>,completion:Option<Completion>,
    localized:Option<RetainedLocalizedFork>,localize_attempted:bool,
    fork_value:Option<ForkClose>,fork_close:Option<RetainedSeedJoin<ForkClose>>,fork_started:bool,
    task_close:Option<ManagedSeedTaskDrain>,task_started:bool,
}
enum Completion {Discarded(Option<JoinedManagedTask>),Published(Option<ManagedCheckpointAcceptance>)}
impl Completion {fn joined(&self)->Result<&JoinedManagedTask> {match self {
    Self::Discarded(value)=>{let value=value.as_ref().context("original task transferred to drain")?;ensure!(value.is_settled(),"discard not actually settled");Ok(value)},
    Self::Published(value)=>value.as_ref().context("original acceptor transferred to drain")?.settled_joined_task(),}}}
#[derive(Clone)]
pub(crate) struct SeedCacheTicket {id:String,seed_id:String,seed:Weak<Cell<SeedCarrier>>,cell:Arc<Cell<CacheScope>>,original:FinalizationBudget,app_thread:ThreadId}
struct CacheScope {factory:SeedFactoryOwner,guard:Option<SeedUseGuard>,cache:Option<Arc<ManagedPreparationCache>>,
    weak:Option<Weak<ManagedPreparationCache>>,attempted:bool,acknowledged:bool,value:Option<CacheClose>,close:Option<RetainedSeedJoin<CacheClose>>,started:bool}
/// DATA/actual Arcs from an already admitted actual source. Construct resources
/// on the ORIGINAL registered thread, then bind its actual PreparedManagedTask.
pub(crate) struct ManagedSeedTaskSource {pub store:LocalStore,pub runnable_root:ObjectRef,pub seed_source:ObjectRef,
    pub seed:Arc<VerifiedMetadataTemplate>,pub expected_integration_head:String,
    pub preparation_cache:Option<Arc<ManagedPreparationCache>>}
struct TemplateClose {template:Option<Arc<VerifiedMetadataTemplate>>}
fn close_template(value:&mut TemplateClose)->Result<()> {value.template=None;Ok(())}
struct CacheClose {cache:Option<Arc<ManagedPreparationCache>>}
fn close_cache(value:&mut CacheClose)->Result<()> {value.cache=None;Ok(())}
struct ForkClose {_cell:Arc<ManagedSeedForkCell>,proof:RetainedLocalizedFork,original:FinalizationBudget,cancelled:Arc<AtomicBool>}
fn close_fork(value:&mut ForkClose)->Result<()> {value.proof.check_current(&value.original,&value.cancelled)}

pub(super) struct EnrollmentCarrier {seed_id:String,seed:Weak<Cell<SeedCarrier>>,_legacy_seed:Option<SeedTicket>,
    task:Option<Weak<Cell<TaskScope>>>,enrollment:Option<ManagedSeedEnrollment>,panic:Option<Box<dyn Any+Send>>}
#[derive(Clone)]
pub(crate) struct EnrollmentTicket {pub(super) id:String,pub(super) cell:Arc<Cell<EnrollmentCarrier>>,admission:Option<Arc<SeedAdmission>>}
impl EnrollmentTicket {
    pub(crate) fn capture(seed:SeedTicket,pending:crate::managed_app::PendingManagedTaskHelper)->Self {
        Self {id:uuid::Uuid::new_v4().to_string(),admission:None,cell:Cell::new(EnrollmentCarrier {seed_id:seed.id.clone(),seed:Arc::downgrade(&seed.cell),
            _legacy_seed:Some(seed),task:None,enrollment:Some(ManagedSeedEnrollment::new(pending)),panic:None})}
    }
    /// App-pure original-cell checks before moving the actual pending helper.
    pub(crate) fn capture_scoped(task:&SeedTaskTicket,pending:crate::managed_app::PendingManagedTaskHelper)
        ->std::result::Result<Self,(crate::managed_app::PendingManagedTaskHelper,anyhow::Error)> {
        let result=(||->Result<()> {task.admission.check_open()?;
            let held=task.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("task source scope busy/poisoned"))?;
            ensure!(held.bound&&held.expected_run.as_deref()==Some(pending.fence().run_id.as_str())
                &&held.fork.as_ref().is_some_and(|fork|pending.seed_fork().is_some_and(|actual|Arc::ptr_eq(actual,fork))),"pending helper differs from original pre-admitted fork");
            let binding=held.fork.as_ref().unwrap().original_binding().context("original fork binding absent")?;
            held.guard.as_ref().context("original task guard absent")?.bind_original_attempt(binding)?;
            held.fork_guard.as_ref().context("original fork guard absent")?.bind_original_attempt(binding)?;Ok(())})();
        if let Err(error)=result {return Err((pending,error));}
        Ok(Self {id:uuid::Uuid::new_v4().to_string(),admission:Some(task.admission.clone()),cell:Cell::new(EnrollmentCarrier {
            seed_id:task.seed_id.clone(),seed:task.seed.clone(),_legacy_seed:None,task:Some(Arc::downgrade(&task.cell)),
            enrollment:Some(ManagedSeedEnrollment::new(pending)),panic:None})})
    }
}

fn lock<'a,T>(cell:&'a Arc<Cell<T>>,original:&FinalizationBudget)->Result<std::sync::MutexGuard<'a,T>> {
    loop {original.remaining()?;match cell.owner.try_lock() {Ok(held)=>return Ok(held),
        Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual machine source carrier poisoned")}}
}
fn off_app(control:&MachineControl)->Result<()> {ensure!(control.shared.app_thread!=std::thread::current().id(),"blocking source operation requires off-App owner");Ok(())}
fn original_seed<'a>(carrier:&'a WorkerCarrier,ticket:&SeedTicket)->Result<&'a SeedTicket> {
    let actual=carrier.seeds.get(&ticket.id).or_else(||carrier.retired_seeds.get(&ticket.id)).context("original seed not registered")?;
    ensure!(Arc::ptr_eq(&actual.cell,&ticket.cell),"foreign original seed ticket");Ok(actual)
}
fn task_seed(ticket:&SeedTaskTicket)->Result<SeedTicket> {Ok(SeedTicket {id:ticket.seed_id.clone(),cell:ticket.seed.upgrade().context("stale task seed ticket")?})}
fn cache_seed(ticket:&SeedCacheTicket)->Result<SeedTicket> {Ok(SeedTicket {id:ticket.seed_id.clone(),cell:ticket.seed.upgrade().context("stale cache seed ticket")?})}

pub(super) fn prepare_seed(machine:&mut ManagedMachineBootstrap,ticket:&SeedTicket)->Result<()> {
    let mut c=ticket.cell.owner.lock().map_err(|_|anyhow::anyhow!("managed seed carrier poisoned"))?;
    let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
        c.admission.check_open()?;time(&c.budget,&c.cancelled)?;let seed=c.source.prepared_current(&c.budget,&c.cancelled)?.seed_root.clone();
        c.client_owner=Some(ClientSeedOwner::capture(c.store.clone(),seed)?);
        let original=c.budget.clone();machine.client_mut_retaining()?.prepare_seed_retaining(c.client_owner.as_mut().unwrap(),&original)?;
        let SeedCarrier {client_owner,template_guard,template,..}=&mut *c;
        client_owner.as_ref().unwrap().acquire_use_retaining(template_guard,SeedUseKind::Template,&original)?;
        *template=Some(machine.client_mut_retaining()?.open_tracked_template(client_owner.as_mut().unwrap(),template_guard.as_ref().unwrap(),&original)?);
        c.source.prepared_current(&c.budget,&c.cancelled)?;time(&c.budget,&c.cancelled)?;Ok(())
    }));
    match result {Ok(Ok(()))=>{ticket.cell.phase.store(READY,Ordering::Release);Ok(())},Ok(Err(_))=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed seed preparation failed; all owners retained"))},
        Err(panic)=>{c.panic=Some(panic);ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed seed preparation panicked; all owners retained"))}}
}
pub(super) fn enrollment_closed(ticket:&EnrollmentTicket)->bool {
    if ticket.admission.as_ref().is_some_and(|gate|gate.check_open().is_err()){return true;}
    ticket.cell.owner.try_lock().ok().is_some_and(|held|held.seed.upgrade().is_none_or(|seed|seed.phase.load(Ordering::Acquire)!=READY))
}
pub(super) fn enroll(machine:&mut ManagedMachineBootstrap,ticket:&EnrollmentTicket)->Result<()> {
    let mut c=ticket.cell.owner.lock().map_err(|_|anyhow::anyhow!("managed enrollment carrier poisoned"))?;
    let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
        let seed=c.seed.upgrade().context("original seed carrier no longer held")?;
        ensure!(seed.phase.load(Ordering::Acquire)==READY,"seed no longer admits enrollment");
        let mut owner=seed.owner.lock().map_err(|_|anyhow::anyhow!("actual seed poisoned"))?;owner.admission.check_open()?;
        if let Some(task)=&c.task {
            let task=task.upgrade().context("original task scope absent")?;let mut task=task.owner.lock().map_err(|_|anyhow::anyhow!("actual task scope poisoned"))?;
            ensure!(task.enrollment.is_none()&&task.guard.is_some()&&task.fork_guard.is_some(),"original task enrollment already attempted/absent");
            task.enrollment=Some(ticket.clone());
        } else {owner.client_owner.as_ref().context("actual seed absent")?.latch_untracked_native_use();}
        c.enrollment.as_mut().context("original enrollment already moved")?.enroll_retaining(machine.client_mut_retaining()?,owner.client_owner.as_mut().context("actual Root seed owner absent")?)
    }));
    match result {Ok(Ok(()))=>{ticket.cell.phase.store(READY,Ordering::Release);Ok(())},Ok(Err(_))=>{ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed enrollment failed; original helper/ACK retained"))},
        Err(panic)=>{c.panic=Some(panic);ticket.cell.phase.store(RECOVERY,Ordering::Release);Err(anyhow::anyhow!("managed enrollment panicked; original helper/ACK retained"))}}
}

impl MachineControl {
    fn carrier_off_app(&self,original:&FinalizationBudget)->Result<std::sync::MutexGuard<'_,WorkerCarrier>> {
        off_app(self)?;
        loop {original.remaining()?;match self.shared.carrier.try_lock(){Ok(held)=>return Ok(held),
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual machine carrier poisoned")}}
    }
    pub(crate) fn submit_seed(&self,ticket:SeedTicket)->std::result::Result<SeedTicket,(SeedTicket,anyhow::Error)> {
        let result=(||->Result<()> {ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"machine unavailable");
            let mut c=self.shared.carrier.try_lock().map_err(|_|anyhow::anyhow!("machine busy; same seed retained"))?;
            ensure!(!c.root_rpc_held,"original Root retirement RPC requires explicit reconciliation");
            ensure!(c.seeds.len()<MAX_SEEDS&&!c.seeds.contains_key(&ticket.id)&&!c.retired_seeds.contains_key(&ticket.id)
                &&ticket.cell.phase.load(Ordering::Acquire)==PENDING,"seed admission full/stale/already attempted");
            c.seeds.insert(ticket.id.clone(),ticket.clone());
            if self.send.try_send(Job::Seed(ticket.clone())).is_err(){ticket.cell.phase.store(RECOVERY,Ordering::Release);anyhow::bail!("seed queue failed; actual registered owner retained");}Ok(())})();
        match result {Ok(())=>Ok(ticket),Err(error)=>Err((ticket,error))}
    }
    pub(crate) fn submit_enrollment(&self,ticket:EnrollmentTicket)->std::result::Result<EnrollmentTicket,(EnrollmentTicket,anyhow::Error)> {
        let result=(||->Result<()> {ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"machine unavailable");
            let mut c=self.shared.carrier.try_lock().map_err(|_|anyhow::anyhow!("machine busy; same helper retained"))?;
            ensure!(!c.root_rpc_held,"original Root retirement RPC requires explicit reconciliation");
            ensure!(c.enrollments.len()<MAX_ENROLLMENTS&&!c.enrollments.contains_key(&ticket.id)
                &&ticket.cell.phase.load(Ordering::Acquire)==PENDING,"helper admission full/already attempted");
            {let held=ticket.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("enrollment busy/poisoned"))?;
                ensure!(c.seeds.get(&held.seed_id).is_some_and(|actual|held.seed.upgrade().is_some_and(|seed|Arc::ptr_eq(&actual.cell,&seed))),"helper uses another actual seed generation");}
            c.enrollments.insert(ticket.id.clone(),ticket.clone());
            if self.send.try_send(Job::Enroll(ticket.clone())).is_err(){ticket.cell.phase.store(RECOVERY,Ordering::Release);anyhow::bail!("enrollment queue failed; actual helper retained");}Ok(())})();
        match result {Ok(())=>Ok(ticket),Err(error)=>Err((ticket,error))}
    }
    /// OFF App, BEFORE factory spawn or source/template/cache/fork use. Reserve
    /// only2 concurrent original factories; live task guards remain independent.
    pub(crate) fn admit_task_retaining(&self,out:&mut Option<SeedTaskTicket>,seed:&SeedTicket,project:&ProjectTicket,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"task scope output already exists");let c=self.carrier_off_app(original)?;
        ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"machine unavailable");original_seed(&c,seed)?;
        ensure!(c.projects.get(&project.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&project.cell)),"foreign actual project");
        let mut s=lock(&seed.cell,original)?;s.admission.check_open()?;
        ensure!(seed.cell.phase.load(Ordering::Acquire)==READY&&s.tasks.len()<MAX_TASK_SCOPES,"seed task admission unavailable/full");
        {let mut p=lock(&project.cell,original)?;ensure!(p.owner.journal_parts().1==s.source.owner()&&s.source.cas_root()==s.store.root(),"source/project owner or CAS differs");}
        let id=uuid::Uuid::new_v4().to_string();
        ensure!(!s.tasks.contains_key(&id)&&!s.caches.contains_key(&id),"original scope nonce collision");
        let mut factory=None;let reserved=SeedFactoryOwner::reserve_retaining(&mut factory,&s.admission,original,cancelled);
        if factory.is_none(){return reserved;}
        let ticket=SeedTaskTicket {id,seed_id:seed.id.clone(),seed:Arc::downgrade(&seed.cell),admission:s.admission.clone(),original:original.clone(),app_thread:self.shared.app_thread,
            cell:Cell::new(TaskScope {factory:factory.take().unwrap(),start:None,factory_outcome:None,factory_refusal:None,never:None,guard:None,fork_guard:None,fork:None,expected_run:None,bound:false,enrollment:None,
                completion:None,localized:None,localize_attempted:false,fork_value:None,fork_close:None,fork_started:false,task_close:None,task_started:false})};
        s.tasks.insert(ticket.id.clone(),ticket.clone());*out=Some(ticket.clone());reserved?;
        let mut held=lock(&ticket.cell,original)?;let actual=s.client_owner.as_ref().context("actual seed owner absent")?;
        actual.acquire_use_retaining(&mut held.guard,SeedUseKind::Task,original)?;
        actual.acquire_use_retaining(&mut held.fork_guard,SeedUseKind::Fork,original)?;
        let TaskScope {start,guard,fork_guard,factory,..}=&mut *held;
        actual.register_task_start_retaining(start,guard.as_ref().unwrap(),fork_guard.as_ref().unwrap(),original)?;
        factory.attach_start_retaining(start.as_ref().unwrap())?;
        ticket.cell.phase.store(READY,Ordering::Release);Ok(())
    }
    pub(crate) fn admit_cache_retaining(&self,out:&mut Option<SeedCacheTicket>,seed:&SeedTicket,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"cache scope output exists");let c=self.carrier_off_app(original)?;original_seed(&c,seed)?;
        ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING,"machine unavailable");let mut s=lock(&seed.cell,original)?;
        s.admission.check_open()?;ensure!(seed.cell.phase.load(Ordering::Acquire)==READY&&s.caches.len()<MAX_CACHE_SCOPES,"cache admission full/closed");
        let id=uuid::Uuid::new_v4().to_string();
        ensure!(!s.tasks.contains_key(&id)&&!s.caches.contains_key(&id),"original scope nonce collision");
        let mut factory=None;let reserved=SeedFactoryOwner::reserve_retaining(&mut factory,&s.admission,original,cancelled);
        if factory.is_none(){return reserved;}
        let ticket=SeedCacheTicket {id,seed_id:seed.id.clone(),seed:Arc::downgrade(&seed.cell),original:original.clone(),app_thread:self.shared.app_thread,cell:Cell::new(CacheScope {
            factory:factory.take().unwrap(),guard:None,cache:None,weak:None,attempted:false,acknowledged:false,value:None,close:None,started:false})};
        s.caches.insert(ticket.id.clone(),ticket.clone());*out=Some(ticket.clone());reserved?;
        let mut held=lock(&ticket.cell,original)?;
        s.client_owner.as_ref().context("actual seed owner absent")?.acquire_use_retaining(&mut held.guard,SeedUseKind::Cache,original)?;
        ticket.cell.phase.store(READY,Ordering::Release);Ok(())
    }
    /// Only the registered ORIGINAL factory thread can obtain actual source
    /// objects/aliases. Source provenance is rechecked with its original budget.
    pub(crate) fn source_for_factory(&self,ticket:&SeedTaskTicket,cache:Option<&SeedCacheTicket>)->Result<ManagedSeedTaskSource> {
        off_app(self)?;let original=ticket.original.clone();
        let mut c=self.carrier_off_app(&original)?;c.bootstrap.client_mut_retaining()?.assert_authenticated_current()?;let seed=task_seed(ticket)?;original_seed(&c,&seed)?;
        let s=lock(&seed.cell,&original)?;s.admission.check_open()?;ensure!(seed.cell.phase.load(Ordering::Acquire)==READY,"seed no longer active");
        ensure!(s.tasks.get(&ticket.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&ticket.cell)),"foreign actual task scope");
        let mut held=lock(&ticket.cell,&original)?;held.factory.check_original_thread()?;
        ensure!(held.fork.is_none()&&held.guard.is_some()&&held.fork_guard.is_some(),"source already handed out or admission incomplete");
        let preparation_cache=if let Some(cache)=cache {
            ensure!(cache.seed_id==ticket.seed_id&&s.caches.get(&cache.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&cache.cell)),"cache belongs to another actual seed");
            let cache=lock(&cache.cell,&original)?;ensure!(cache.factory.is_joined()&&cache.acknowledged,"actual cache factory/TLS or original constructor ACK absent");Some(cache.cache.as_ref().context("actual cache absent")?.clone())
        }else{None};
        let prepared=s.source.prepared_current(&original,&s.cancelled)?;
        let template=s.template.as_ref().context("actual tracked template absent")?;held.factory.check_original_thread()?;
        held.fork=Some(ManagedSeedForkCell::new(template,prepared.runnable_root.clone(),prepared.source_root.clone(),s.admission.clone(),held.start.as_ref().context("original task start gate absent")?.clone(),s.store.root(),preparation_cache.as_ref(),&original,held.factory.cancellation())?);
        let result=ManagedSeedTaskSource {store:s.store.clone(),runnable_root:prepared.runnable_root.clone(),seed_source:prepared.source_root.clone(),seed:template.clone(),
            expected_integration_head:prepared.baseline.commit.0.clone(),preparation_cache};held.factory.check_original_thread()?;Ok(result)
    }
}

impl SeedTaskTicket {
    pub(crate) fn register_factory_retaining(&self,worker:&mut Option<JoinHandle<()>>)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"factory registration requires off-App owner");
        ensure!(self.cell.phase.load(Ordering::Acquire)==READY,"task source scope unavailable");
        lock(&self.cell,&self.original)?.factory.register_retaining(worker)
    }
    pub(crate) fn wait_original_factory_registration(&self)->Result<()> {
        loop {self.original.remaining()?;self.admission.check_open()?;
            let held=lock(&self.cell,&self.original)?;
            if held.factory.registration_present(){return held.factory.check_original_thread();}drop(held);
            std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);}
    }
    pub(crate) fn bind_prepared_retaining(&self,prepared:&mut PreparedManagedTask)->Result<()> {
        let mut held=lock(&self.cell,&self.original)?;held.factory.check_original_thread()?;
        ensure!(!held.bound&&prepared.original_budget().constraint()==self.original.constraint(),"prepared task scope/budget changed");
        let fork=held.fork.as_ref().context("original registered factory has not obtained its source")?.clone();
        held.expected_run=Some(prepared.target_run_id().to_owned());
        let result=prepared.install_seed_fork_retaining(&fork);held.bound=prepared.has_seed_fork(&fork);result
    }
    /// Actual join waits occur with ALL machine/source/scope locks released.
    pub(crate) fn join_factory_off_app(&self)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"factory join requires off-App owner");
        let (driver,original)={let held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual factory owner busy/poisoned; original handle retained"))?;held.factory.join_driver()?};
        driver.start_join_observer()?;
        if driver.try_joined()?.is_none(){let _=driver.await_joined(original.remaining()?)?;}
        ensure!(self.poll_factory_joined_off_app()?,"original factory join not recorded");original.remaining()?;Ok(())
    }
    pub(crate) fn poll_factory_joined_off_app(&self)->Result<bool> {
        ensure!(self.app_thread!=std::thread::current().id(),"factory join observation requires off App");
        match self.cell.owner.try_lock(){Ok(mut held)=>held.factory.poll_joined(),Err(TryLockError::WouldBlock)=>Ok(false),Err(_)=>anyhow::bail!("factory carrier poisoned")}
    }
    pub(crate) fn offer_discarded_retaining(&self,original:&mut Option<JoinedManagedTask>,budget:&FinalizationBudget)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"completion transfer requires off App");let mut held=lock(&self.cell,budget)?;
        let actual=original.as_ref().context("original joined task absent")?;check_completion(&held,actual)?;
        held.completion=Some(Completion::Discarded(Some(original.take().unwrap())));Ok(())
    }
    pub(crate) fn offer_published_retaining(&self,original:&mut Option<ManagedCheckpointAcceptance>,budget:&FinalizationBudget)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"completion transfer requires off App");let mut held=lock(&self.cell,budget)?;
        let actual=original.as_ref().context("original whole acceptor absent")?.settled_joined_task()?;check_completion(&held,actual)?;
        held.completion=Some(Completion::Published(Some(original.take().unwrap())));Ok(())
    }
}
fn check_completion(scope:&TaskScope,joined:&JoinedManagedTask)->Result<()> {
    ensure!(scope.completion.is_none()&&scope.task_close.is_none()&&scope.factory.is_joined()&&joined.is_settled(),"original completion/factory/acceptance not settled or already offered");
    ensure!(scope.fork.as_ref().is_some_and(|fork|joined.prepared().seed_fork.as_ref().is_some_and(|actual|Arc::ptr_eq(actual,fork)))
        &&scope.expected_run.as_deref()==Some(joined.prepared().assignment.fence.run_id.as_str()),"completion differs from original pre-admitted task");
    let ticket=scope.enrollment.as_ref().context("original enrolled helper absent")?;
    let enrollment=ticket.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual enrollment busy/poisoned"))?;
    enrollment.enrollment.as_ref().context("original enrollment already moved")?.matches_joined(joined)
}
impl SeedCacheTicket {
    pub(crate) fn register_factory_retaining(&self,worker:&mut Option<JoinHandle<()>>)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"cache registration requires off App");lock(&self.cell,&self.original)?.factory.register_retaining(worker)
    }
    pub(crate) fn wait_original_factory_registration(&self)->Result<()> {
        loop {self.original.remaining()?;let held=lock(&self.cell,&self.original)?;
            if held.factory.registration_present(){return held.factory.check_original_thread();}drop(held);
            std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);}
    }
    pub(crate) fn join_factory_off_app(&self)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"cache join requires off App");
        let (driver,original)={let held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual factory owner busy/poisoned; original handle retained"))?;held.factory.join_driver()?};
        driver.start_join_observer()?;if driver.try_joined()?.is_none(){let _=driver.await_joined(original.remaining()?)?;}
        let mut held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("cache actual join remains retained; poll explicitly"))?;
        ensure!(held.factory.poll_joined()?,"actual cache join not recorded");original.remaining()?;Ok(())
    }
}
impl MachineControl {
    /// Actual selected project's existing cache constructor, on the registered
    /// original factory thread. No user callback or cache-hit claim accepted.
    pub(crate) fn prepare_cache_on_original_factory(&self,ticket:&SeedCacheTicket,project:&ProjectTicket)->Result<()> {
        let original=&ticket.original;let mut c=self.carrier_off_app(original)?;c.bootstrap.client_mut_retaining()?.assert_authenticated_current()?;let seed=cache_seed(ticket)?;original_seed(&c,&seed)?;
        let s=lock(&seed.cell,original)?;s.admission.check_open()?;
        ensure!(s.caches.get(&ticket.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&ticket.cell))
            &&c.projects.get(&project.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&project.cell)),"foreign cache/project scope");
        let mut cache=lock(&ticket.cell,original)?;cache.factory.check_original_thread()?;
        ensure!(!cache.attempted&&cache.cache.is_none()&&cache.guard.is_some(),"cache constructor already attempted/unadmitted");cache.attempted=true;
        let mut project=lock(&project.cell,original)?;project.owner.assert_current()?;
        let fence=project.owner.journal_parts().1.clone();ensure!(&fence==s.source.owner(),"cache differs from source owner/epoch");
        let lease=project.owner.source_lease(&fence)?;let journal=lease.open_source_writer()?;
        let store=project.owner.managed_recovery_store().context("actual project store absent")?.clone();
        let actual=ManagedPreparationCache::open(lease,&journal,store,s.store.clone())?;
        cache.weak=Some(Arc::downgrade(&actual));cache.cache=Some(actual);cache.factory.check_original_thread()?;cache.acknowledged=true;Ok(())
    }
}
impl ManagedProjectApp<'_> {
    pub(crate) fn start_scoped_task(&mut self,scope:&SeedTaskTicket,index:usize,prepared:PreparedManagedTask)
        ->std::result::Result<crate::managed_launch::ManagedLaunchFence,ManagedTaskStartFailure> {
        let checks=(||->Result<()> {scope.admission.check_open()?;ensure!(scope.cell.phase.load(Ordering::Acquire)==READY,"original task admission closed");
            let held=scope.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("task scope busy/poisoned"))?;
            ensure!(held.factory.is_joined()&&held.bound&&held.fork.as_ref().is_some_and(|fork|prepared.has_seed_fork(fork)),"actual factory/TLS or prepared binding not settled");Ok(())})();
        if let Err(error)=checks{return Err(ManagedTaskStartFailure::Prepared(prepared,error));}
        let project=self.carrier.project.as_ref().unwrap().clone();let (journal,owner)=self.carrier.owner.journal_parts();
        self.app.start_managed_task(index,prepared,&project,journal,owner)
    }
    pub(crate) fn release_enrollment_retaining(&mut self,ticket:&EnrollmentTicket)->Result<bool> {
        if let Some(admission)=&ticket.admission {admission.check_open()?;}
        let phase=ticket.cell.phase.load(Ordering::Acquire);ensure!(phase!=RECOVERY&&phase!=RETIRED&&phase!=CLOSING,"enrollment no longer admits start");
        if phase!=READY{return Ok(false);}
        let mut held=match ticket.cell.owner.try_lock(){Ok(held)=>held,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("actual enrollment poisoned")};
        let result=held.enrollment.as_mut().context("original enrollment transferred to drain")?.release_on_app_retaining(self.app);
        if result.is_err(){ticket.cell.phase.store(RECOVERY,Ordering::Release);}result.map(|_|true)
    }
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(crate) enum SeedRetirementProgress {WaitingFactories,WaitingTasks,WaitingAliases,LegacyUntracked,RetainedPartial,Retired}
impl MachineControl {
    /// Close ONLY new admission. Existing original users are not claimed stopped.
    pub(crate) fn close_seed_admission_off_app(&self,ticket:&SeedTicket,original:&FinalizationBudget)->Result<()> {
        let c=self.carrier_off_app(original)?;original_seed(&c,ticket)?;
        let s=lock(&ticket.cell,original)?;s.admission.close();
        if ticket.cell.phase.load(Ordering::Acquire)!=RETIRED {ticket.cell.phase.store(CLOSING,Ordering::Release);}
        if let Some(owner)=&s.client_owner {owner.close_native_admission()?;}Ok(())
    }
    pub(crate) fn retire_seed_off_app(&self,ticket:&SeedTicket,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<SeedRetirementProgress> {
        self.retire_seed_inner(ticket,original,cancelled,false)
    }
    /// Explicit continuation only, same stored campaign/deadline. No publication,
    /// helper relaunch or failed Root exchange is silently replayed here.
    pub(crate) fn reconcile_seed_retirement_off_app(&self,ticket:&SeedTicket,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<SeedRetirementProgress> {
        self.retire_seed_inner(ticket,original,cancelled,true)
    }
    fn retire_seed_inner(&self,ticket:&SeedTicket,original:&FinalizationBudget,cancelled:Arc<AtomicBool>,reconcile:bool)->Result<SeedRetirementProgress> {
        let mut c=self.carrier_off_app(original)?;original_seed(&c,ticket)?;
        if ticket.cell.phase.load(Ordering::Acquire)==RETIRED {return Ok(SeedRetirementProgress::Retired);}
        let mut s=lock(&ticket.cell,original)?;s.admission.close();
        s.client_owner.as_ref().context("actual Root seed absent")?.close_native_admission()?;
        ticket.cell.phase.store(CLOSING,Ordering::Release);
        if s.client_owner.as_ref().unwrap().has_untracked_native_use(){return Ok(SeedRetirementProgress::LegacyUntracked);}
        // Original pending/stale enrollment cells also keep capacity charged.
        for enrollment in c.enrollments.values() {
            let held=lock(&enrollment.cell,original)?;
            if held.seed_id==ticket.id&&held.task.is_none(){return Ok(SeedRetirementProgress::LegacyUntracked);}
        }
        for task in s.tasks.values() {
            let mut held=lock(&task.cell,original)?;
            if !held.factory.registration_present()||!held.factory.poll_joined()? {return Ok(SeedRetirementProgress::WaitingFactories);}
            if let Some(unused)=held.never.as_ref().filter(|n|n.seal.is_some()) {if unused.task_close.is_some(){continue;}else{return Ok(SeedRetirementProgress::WaitingTasks);}}
            if held.task_close.is_none() {
                let Some(completion)=&held.completion else{return Ok(SeedRetirementProgress::WaitingTasks);};
                check_completion_before_close(&held,completion.joined()?)?;
            }
        }
        for cache in s.caches.values() {let mut held=lock(&cache.cell,original)?;
            if !held.factory.registration_present()||!held.factory.poll_joined()? {return Ok(SeedRetirementProgress::WaitingFactories);}}
        if s.retirement.is_none() {
            if c.retired_seeds.len()>=MAX_RETIRED_SEEDS {return Ok(SeedRetirementProgress::WaitingAliases);}
            if s.template.as_ref().is_some_and(|template|Arc::strong_count(template)!=1){return Ok(SeedRetirementProgress::WaitingAliases);}
            for cache in s.caches.values() {let cache=lock(&cache.cell,original)?;
                if cache.cache.as_ref().is_some_and(|actual|Arc::strong_count(actual)!=1){return Ok(SeedRetirementProgress::WaitingAliases);}}
            time(original,&cancelled)?;s.retirement=Some(Campaign {original:original.clone(),cancelled});
        }
        let campaign=s.retirement.as_ref().unwrap();
        ensure!(campaign.original.constraint()==original.constraint(),"seed retirement original campaign changed");
        let budget=campaign.original.clone();let cancel=campaign.cancelled.clone();time(&budget,&cancel)?;
        let WorkerCarrier {bootstrap,root_rpc_held,..}=&mut *c;
        bootstrap.check_current_retaining()?;
        let client=bootstrap.client_mut_retaining()?;
        let actual=s.client_owner.as_ref().unwrap();actual.poll_native_joins_off_app()?;
        for task in s.tasks.values(){
            if lock(&task.cell,&budget)?.never.as_ref().is_some_and(|n|n.seal.is_some()){close_never_started(client,actual,task,&budget,&cancel,reconcile)?;}
            else {close_task(client,actual,task,&budget,&cancel,reconcile)?;}
        }
        for cache in s.caches.values(){close_cache_scope(cache,&budget)?;}
        // Caller aliases must disappear; worker result/drop is never used as an
        // actor/TLS proof. Native client independently checks tracked templates.
        for cache in s.caches.values(){let held=lock(&cache.cell,&budget)?;
            if held.weak.as_ref().is_some_and(|weak|weak.upgrade().is_some()){return Ok(SeedRetirementProgress::WaitingAliases);}}
        if s.template_close.is_none() {
            // Store owning value before moving original Arc/guard into worker.
            s.template_value=Some(TemplateClose {template:s.template.take()});
            let SeedCarrier {template_close,template_guard,template_value,..}=&mut *s;
            RetainedSeedJoin::stage_retaining(template_close,template_guard,template_value,&budget)?;
        }
        if !s.template_started {s.template_close.as_mut().unwrap().start_retaining(close_template)?;s.template_started=true;}
        s.template_close.as_mut().unwrap().await_joined()?;time(&budget,&cancel)?;
        let root=s.client_owner.as_mut().unwrap();
        *root_rpc_held=true;
        let result=client.request_retirement_retaining(root,&budget);*root_rpc_held=client.retirement_pending();result?;
        ensure!(root.is_retired(),"original Root Release ACK absent");
        mark_retired(&s);ticket.cell.phase.store(RETIRED,Ordering::Release);drop(s);
        let old=c.seeds.remove(&ticket.id).context("original active seed vanished")?;
        c.enrollments.retain(|_,enrollment|enrollment.cell.owner.try_lock().map(|held|held.seed_id!=ticket.id).unwrap_or(true));
        c.retired_seeds.insert(ticket.id.clone(),old);Ok(SeedRetirementProgress::Retired)
    }
    /// ONE nonblocking original ACK receive only. Expiry cannot authorize fresh
    /// cleanup. A genuine final ACK alone can move the same seed into archive.
    pub(crate) fn observe_retirement_ack_off_app(&self,ticket:&SeedTicket)->Result<SeedRetirementProgress> {
        off_app(self)?;let mut c=self.shared.carrier.try_lock().map_err(|_|anyhow::anyhow!("machine busy; actual retirement retained"))?;
        original_seed(&c,ticket)?;
        if ticket.cell.phase.load(Ordering::Acquire)==RETIRED{return Ok(SeedRetirementProgress::Retired);}
        let mut s=ticket.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("seed busy; original owner retained"))?;
        ensure!(s.retirement.is_some(),"original retirement campaign absent");
        let actual=s.client_owner.as_mut().context("actual Root owner absent")?;
        let client=c.bootstrap.client_mut_retaining()?;
        if !client.retirement_pending()&&!actual.is_retired(){return Ok(SeedRetirementProgress::RetainedPartial);}
        let result=if client.retirement_pending(){client.reconcile_retirement_ack_retaining(actual)}else{Ok(true)};let pending=client.retirement_pending();c.root_rpc_held=pending;result?;
        if !actual.is_retired(){return Ok(SeedRetirementProgress::RetainedPartial);}
        ensure!(c.retired_seeds.len()<MAX_RETIRED_SEEDS,"retired original-owner storage remains full");
        mark_retired(&s);ticket.cell.phase.store(RETIRED,Ordering::Release);drop(s);
        let old=c.seeds.remove(&ticket.id).context("original active seed absent")?;
        c.enrollments.retain(|_,enrollment|enrollment.cell.owner.try_lock().map(|held|held.seed_id!=ticket.id).unwrap_or(true));
        c.retired_seeds.insert(ticket.id.clone(),old);Ok(SeedRetirementProgress::Retired)
    }
    /// Disposal is OFF App and only after genuine Root Release + actual joins.
    /// Ticket/refcount checks are ADDITIONAL alias gates; never join authority.
    pub(crate) fn collect_retired_seed_owners_off_app(&self,original:&FinalizationBudget)->Result<usize> {
        let mut c=self.carrier_off_app(original)?;let mut settled=Vec::new();let mut retired=std::collections::BTreeSet::new();
        for (id,ticket) in &c.retired_seeds {
            let held=lock(&ticket.cell,original)?;
            ensure!(held.client_owner.as_ref().is_some_and(ClientSeedOwner::is_retired),"archive lacks actual Root Release ACK");retired.insert(id.clone());
        }
        // Retry only removal of old map aliases; busy cells stay held. This is
        // after actual Root Release, not a phase/refcount-derived drain proof.
        c.enrollments.retain(|_,ticket|ticket.cell.owner.try_lock().map(|held|!retired.contains(&held.seed_id)).unwrap_or(true));
        for (id,ticket) in &c.retired_seeds {
            if Arc::strong_count(&ticket.cell)!=1{continue;}
            let held=lock(&ticket.cell,original)?;
            ensure!(held.client_owner.as_ref().is_some_and(ClientSeedOwner::is_retired),"retired archive lacks genuine Root Release");
            if !archive_aliases_gone(&held){continue;}
            settled.push(id.clone());
        }
        for id in &settled {c.retired_seeds.remove(id);}Ok(settled.len())
    }
}
fn check_completion_before_close(scope:&TaskScope,joined:&JoinedManagedTask)->Result<()> {
    ensure!(scope.factory.is_joined()&&joined.is_settled(),"actual factory/task joins or acceptance unsettled");
    let ticket=scope.enrollment.as_ref().context("original enrollment absent")?;
    let held=ticket.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual enrollment busy"))?;
    held.enrollment.as_ref().context("original enrollment already transferred")?.matches_joined(joined)
}
fn close_task(client:&mut rudder_native::root_storage_client::RootStorageClient,seed:&ClientSeedOwner,ticket:&SeedTaskTicket,
    original:&FinalizationBudget,cancelled:&Arc<AtomicBool>,reconcile:bool)->Result<()> {
    let mut held=lock(&ticket.cell,original)?;
    if held.localized.is_none() {
        let joined=held.completion.as_ref().context("original completion absent")?.joined()?;
        check_completion_before_close(&held,joined)?;
        let original_enrollment=held.enrollment.as_ref().unwrap().clone();let enrollment=lock(&original_enrollment.cell,original)?;
        client.associate_original_helper_use(seed,held.guard.as_ref().unwrap(),enrollment.enrollment.as_ref().unwrap().receipt().context("original helper ACK absent")?)?;
        let fork=held.fork.as_ref().context("original fork cell absent")?.clone();
        if held.localize_attempted {ensure!(reconcile,"original localization partial; use explicit reconciliation");}
        let resume=held.localize_attempted;held.localize_attempted=true;
        let joined=held.completion.as_ref().unwrap().joined()?;
        fork.localize_after_joined(joined,original,cancelled,resume)?;
        let TaskScope {guard,fork_guard,localized,..}=&mut *held;
        fork.with_localized(original,cancelled,|proof|client.acknowledge_localized_fork_retaining(seed,localized,
            guard.as_ref().unwrap(),fork_guard.as_ref().unwrap(),proof,original,cancelled))?;
    }
    if held.fork_close.is_none() {
        held.fork_value=Some(ForkClose {_cell:held.fork.as_ref().unwrap().clone(),proof:held.localized.as_ref().unwrap().clone(),original:original.clone(),cancelled:cancelled.clone()});
        let TaskScope {fork_close,fork_guard,fork_value,..}=&mut *held;
        RetainedSeedJoin::stage_retaining(fork_close,fork_guard,fork_value,original)?;
    }
    if !held.fork_started {held.fork_close.as_mut().unwrap().start_retaining(close_fork)?;held.fork_started=true;}
    held.fork_close.as_mut().unwrap().await_joined()?;
    if held.task_close.is_none() {
        let ticket=held.enrollment.as_ref().unwrap().clone();let mut enrollment=lock(&ticket.cell,original)?;
        let TaskScope {task_close,guard,completion,..}=&mut *held;
        match completion.as_mut().context("original completion absent")? {
            Completion::Discarded(value)=>ManagedSeedTaskDrain::stage_discarded_retaining(task_close,guard,value,&mut enrollment.enrollment,client,seed,original)?,
            Completion::Published(value)=>ManagedSeedTaskDrain::stage_published_retaining(task_close,guard,value,&mut enrollment.enrollment,client,seed,original)?,
        }
        ticket.cell.phase.store(CLOSING,Ordering::Release);
    }
    if !held.task_started {held.task_close.as_mut().unwrap().start_off_app()?;held.task_started=true;}
    held.task_close.as_mut().unwrap().await_joined_off_app()?;Ok(())
}
fn close_cache_scope(ticket:&SeedCacheTicket,original:&FinalizationBudget)->Result<()> {
    let mut held=lock(&ticket.cell,original)?;
    ensure!(held.factory.is_joined(),"cache original factory/TLS unjoined");
    if held.close.is_none(){held.value=Some(CacheClose {cache:held.cache.take()});let CacheScope {close,guard,value,..}=&mut *held;
        RetainedSeedJoin::stage_retaining(close,guard,value,original)?;}
    if !held.started {held.close.as_mut().unwrap().start_retaining(close_cache)?;held.started=true;}
    held.close.as_mut().unwrap().await_joined()?;Ok(())
}
fn mark_retired(seed:&SeedCarrier) {
    for task in seed.tasks.values(){task.cell.phase.store(RETIRED,Ordering::Release);
        if let Ok(task)=task.cell.owner.try_lock(){if let Some(enrollment)=&task.enrollment{enrollment.cell.phase.store(RETIRED,Ordering::Release);}}}
    for cache in seed.caches.values(){cache.cell.phase.store(RETIRED,Ordering::Release);}
}
fn archive_aliases_gone(seed:&SeedCarrier)->bool {
    seed.tasks.values().all(|ticket|Arc::strong_count(&ticket.cell)==1&&ticket.cell.owner.try_lock().ok().is_some_and(|task|
        task.enrollment.as_ref().is_none_or(|ticket|Arc::strong_count(&ticket.cell)==1)))
        &&seed.caches.values().all(|ticket|Arc::strong_count(&ticket.cell)==1)
}

include!("managed_machine_never_started.rs");
