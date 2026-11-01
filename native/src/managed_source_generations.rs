//! Child of the existing machine owner. Bounded immutable-source maintenance,
//! not another task scheduler. Actual old source/task/Root owners stay charged.
use super::*;
use rudder_engine::execution::handoff::WorkerJoined;
use crate::managed_checkpoint_accept::AcceptedInputRelease;
use std::sync::{Weak,OnceLock};
const MAX_GENERATIONS:usize=4;

pub(crate) enum CurrentSourceAdmission {Ready {seed:SeedTicket,cache:SeedCacheTicket},Pending,Fenced}
pub(super) struct CurrentSource {seed:SeedTicket,cache:SeedCacheTicket,generation:Option<Arc<SourceGeneration>>}
struct SnapshotReady {_factory:WorkerJoined,_original:FinalizationBudget}
struct Installed {_bootstrap:WorkerJoined,_original:FinalizationBudget}
struct RootReadStartPermit;
pub(super) struct RootSourceReservation {id:String,generation:Weak<SourceGeneration>,original:OnceLock<FinalizationBudget>,cancelled:Arc<AtomicBool>,queued:AtomicBool,permit:Mutex<Option<RootReadStartPermit>>}
impl RootSourceReservation{fn budget(&self)->Result<&FinalizationBudget>{self.original.get().context("original reserved Root clock absent")}}
struct CacheResult {outcome:Option<Result<()>>,panic:Option<Box<dyn Any+Send>>}
struct Owners {source:Option<ManagedInitialSourceOwner>,seed:Option<SeedTicket>,cache:Option<SeedCacheTicket>,
    cache_thread:Option<std::thread::JoinHandle<()>>,cache_result:Option<Arc<Mutex<CacheResult>>>,
    release:Option<AcceptedInputRelease>,error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
struct Worker {handle:Option<OwnedWorkerJoin>,owned:Arc<AtomicBool>,attempted:bool}
impl Worker {fn new()->Self{Self{handle:None,owned:Arc::new(AtomicBool::new(false)),attempted:false}}
    fn joined(&self)->Result<Option<WorkerJoined>>{let Some(handle)=&self.handle else{return Ok(None);};handle.start_join_observer()?;handle.try_joined()}}
/// Kept in the machine map AND original task scope. Its project/old-seed/scope
/// links are weak to avoid an owner cycle. No Drop join/unpin/retry/VM stop.
pub(super) struct SourceGeneration {id:String,project_id:String,project:Weak<Cell<ProjectCarrier>>,old_seed_id:String,
    old_seed:Weak<Cell<seeds::SeedCarrier>>,scope:Weak<Cell<seeds::TaskScope>>,shared:Weak<Shared>,
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,owners:Mutex<Owners>,factory:Mutex<Worker>,bootstrap:Mutex<Worker>,retirement:Mutex<Worker>,
    snapshot:OnceLock<SnapshotReady>,released:OnceLock<AcceptedInputRelease>,reservation:Mutex<Option<Arc<RootSourceReservation>>>,installed:OnceLock<Installed>,
    recovery:Mutex<Option<anyhow::Error>>,retirement_error:Mutex<Option<anyhow::Error>>,retired:AtomicBool,retirement_joined:OnceLock<WorkerJoined>}
impl SourceGeneration {
    pub(super) fn input_released(&self)->bool{self.snapshot.get().is_some()&&self.released.get().is_some_and(AcceptedInputRelease::acknowledged)}
    fn control(&self)->Result<MachineControl>{let shared=self.shared.upgrade().context("original machine owner absent")?;Ok(MachineControl{send:shared.send.clone(),shared})}
    fn project(&self)->Result<ProjectTicket>{Ok(ProjectTicket{id:self.project_id.clone(),cell:self.project.upgrade().context("original project owner absent")?})}
    fn old_seed(&self)->Result<SeedTicket>{Ok(SeedTicket{id:self.old_seed_id.clone(),cell:self.old_seed.upgrade().context("original seed owner absent")?})}
    fn start_factory(self:&Arc<Self>)->Result<()> {
        let mut worker=self.factory.lock().unwrap_or_else(|p|p.into_inner());ensure!(!worker.attempted,"original source factory attempted");worker.attempted=true;
        let cell=self.clone();let owned=worker.owned.clone();let latch=owned.clone();
        let handle=std::thread::Builder::new().name("rudder-accepted-source-generation".into()).spawn(move||{
            while !latch.load(Ordering::Acquire){std::thread::yield_now();}
            let mut held=cell.owners.lock().unwrap_or_else(|p|p.into_inner());
            let outcome=catch_unwind(AssertUnwindSafe(||held.source.as_mut().context("actual successor source absent")?.prepare_successor_retaining(&cell.original,cell.cancelled.clone())));
            match outcome{Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        });
        match handle {Ok(handle)=>{worker.handle=Some(OwnedWorkerJoin::new(handle));owned.store(true,Ordering::Release);Ok(())},Err(error)=>Err(error.into())}
    }
    fn qualify_snapshot(&self)->Result<()> {
        if self.snapshot.get().is_some(){return Ok(());}
        let worker=self.factory.lock().unwrap_or_else(|p|p.into_inner());let Some(joined)=qualified_factory(&worker,&self.original,&self.cancelled)? else{return Ok(());};
        let mut held=self.owners.lock().unwrap_or_else(|p|p.into_inner());
        ensure!(held.error.is_none()&&held.panic.is_none(),"source generation factory failed; actual owners retained");
        held.source.as_ref().context("actual source absent")?.prepared_current(&self.original,&self.cancelled)?;time(&self.original,&self.cancelled)?;
        self.snapshot.set(SnapshotReady{_factory:joined,_original:self.original.clone()}).map_err(|_|anyhow::anyhow!("source snapshot already qualified"))?;
        // Lock release is original-resource cleanup, separate from SourceReady.
        // Its genuine ACK is retained BEFORE any late budget error is returned.
        let scope=self.scope.upgrade().context("original Published task scope absent")?;let mut scope=seeds::lock(&scope,&self.original)?;
        let seeds::Completion::Published(Some(acceptance))=scope.completion.as_mut().context("whole Published acceptor absent")? else{anyhow::bail!("original Published acceptor moved before source capture");};
        let Owners{source,release,..}=&mut *held;
        let result=source.as_ref().unwrap().release_successor_input_retaining(release,acceptance,&self.original,&self.cancelled);
        if release.as_ref().is_some_and(AcceptedInputRelease::acknowledged){let ack=release.take().unwrap();ensure!(ack.same_input(source.as_ref().unwrap().accepted_input().unwrap()),"canonical release belongs to another original source");let _=self.released.set(ack);}
        result
    }
    /// Starts only after actual bounded native Root admission was reserved under
    /// the SAME machine mutex used by all initial seed producers. No availability
    /// boolean, Root status/readback or serialized clock can reserve this slot.
    fn start_bootstrap(self:&Arc<Self>,reservation:Arc<RootSourceReservation>)->Result<bool> {
        let mut worker=self.bootstrap.lock().unwrap_or_else(|p|p.into_inner());ensure!(!worker.attempted,"original Root generation preparation attempted");worker.attempted=true;
        let cell=self.clone();let owned=worker.owned.clone();let latch=owned.clone();
        let spawned=std::thread::Builder::new().name("rudder-generation-root-cache".into()).spawn(move||{
            while !latch.load(Ordering::Acquire){std::thread::yield_now();}
            let mut held=cell.owners.lock().unwrap_or_else(|p|p.into_inner());
            let outcome=catch_unwind(AssertUnwindSafe(||bootstrap(&cell,&reservation,&mut held)));
            match outcome{Ok(Ok(()))=>{},Ok(Err(error))=>held.error=Some(error),Err(panic)=>held.panic=Some(panic)}
        });
        match spawned {Ok(handle)=>{worker.handle=Some(OwnedWorkerJoin::new(handle));owned.store(true,Ordering::Release);Ok(true)},Err(error)=>{
            self.owners.lock().unwrap_or_else(|p|p.into_inner()).error=Some(error.into());Ok(false)}}
        // false is ONLY std::thread::Builder's actual spawn Err: no thread or
        // Root dispatch exists, so the caller may release this unused reservation.
    }
    fn start_retirement(self:&Arc<Self>)->Result<()> {
        let mut worker=self.retirement.lock().unwrap_or_else(|p|p.into_inner());if worker.attempted{return Ok(());}worker.attempted=true;
        let cell=self.clone();let owned=worker.owned.clone();let latch=owned.clone();
        let handle=std::thread::Builder::new().name("rudder-old-source-retirement".into()).spawn(move||{
            while !latch.load(Ordering::Acquire){std::thread::yield_now();}
            let result=catch_unwind(AssertUnwindSafe(||->Result<()> {
                let control=cell.control()?;let seed=cell.old_seed()?;
                loop {
                    // Preflight closes admission and observes original users/joins.
                    // It cannot localize/unmount/release pins until the existing
                    // owner captures its physical-cleanup campaign. Thereafter
                    // ALL continuations use that SAME stored original clock.
                    let (original,cancel)=match control.generation_retirement_clock(&seed)?{Some(original)=>original,None=>(FinalizationBudget::begin(Duration::from_secs(30))?,Arc::new(AtomicBool::new(false)))};
                    match control.retire_seed_off_app(&seed,&original,cancel)? {
                        seeds::SeedRetirementProgress::Retired=>{cell.retired.store(true,Ordering::Release);return Ok(());},
                        seeds::SeedRetirementProgress::LegacyUntracked|seeds::SeedRetirementProgress::RetainedPartial=>anyhow::bail!("old generation has untracked/unknown users; original owners retained"),
                        _=>std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            }));
            match result{Ok(Ok(()))=>{},Ok(Err(error))=>*cell.retirement_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error),Err(panic)=>{
                *cell.retirement_error.lock().unwrap_or_else(|p|p.into_inner())=Some(anyhow::anyhow!("original retirement worker panicked"));cell.owners.lock().unwrap_or_else(|p|p.into_inner()).panic=Some(panic);}}
        })?;
        worker.handle=Some(OwnedWorkerJoin::new(handle));owned.store(true,Ordering::Release);Ok(())
    }
}
fn bootstrap(cell:&SourceGeneration,reservation:&Arc<RootSourceReservation>,held:&mut Owners)->Result<()> {
    let original=reservation.budget()?;let cancel=&reservation.cancelled;time(original,cancel)?;
    let control=cell.control()?;let project=cell.project()?;
    let store=held.source.as_ref().context("original inert snapshot absent")?.store_clone();
    SeedTicket::capture_generation_retaining(&mut held.seed,&mut held.source,store,reservation.id.clone(),original.clone(),cancel.clone())?;
    let seed=held.seed.as_ref().unwrap().clone();loop {time(original,cancel)?;if control.submit_reserved_generation_seed(reservation,&seed)?{break;}std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
    loop{time(original,cancel)?;ensure!(!seed.ingress_recovery(),"new Root seed preparation requires recovery");if seed.ingress_ready(){break;}std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
    control.admit_cache_retaining(&mut held.cache,&seed,original,cancel.clone())?;
    let cache=held.cache.as_ref().unwrap().clone();let actual=cache.clone();let actual_control=control.clone();let actual_project=project.clone();
    held.cache_result=Some(Arc::new(Mutex::new(CacheResult{outcome:None,panic:None})));let result=held.cache_result.as_ref().unwrap().clone();
    held.cache_thread=Some(std::thread::Builder::new().name("rudder-generation-cache".into()).spawn(move||{
        let mut held=result.lock().unwrap_or_else(|p|p.into_inner());
        match catch_unwind(AssertUnwindSafe(||{actual.wait_original_factory_registration()?;actual_control.prepare_cache_on_original_factory(&actual,&actual_project)})){
            Ok(outcome)=>held.outcome=Some(outcome),Err(panic)=>held.panic=Some(panic)}
    })?);
    cache.register_factory_retaining(&mut held.cache_thread)?;
    loop{time(original,cancel)?;if cache.ingress_poll_factory_joined()?{break;}std::thread::sleep(original.phase_limit(Duration::from_millis(2))?);}
    let result=held.cache_result.as_ref().unwrap().lock().unwrap_or_else(|p|p.into_inner());
    ensure!(result.panic.is_none()&&matches!(result.outcome.as_ref(),Some(Ok(()))),"original new-generation cache constructor failed/unknown");
    cache.ingress_original_ready()?;time(original,cancel)
}

/// Called by the original Root worker, OFF App. Only short try-lock observations
/// and typed joins/adoption; Git/CAS generation and cache work run on fixed owned
/// workers. This maintains immutable source generations, never schedules tasks.
pub(super) fn poll(c:&mut WorkerCarrier,shared:&Arc<Shared>)->Result<()> {
    let generations=c.generations.values().cloned().collect::<Vec<_>>();
    for generation in generations {
        // One failed generation must not starve unrelated retained sources.
        // Errors are absorbing for new generation effects; actual retirement
        // and joins below remain independent original-resource cleanup.
        if generation.recovery.lock().unwrap_or_else(|p|p.into_inner()).is_none(){
            if let Err(error)=advance(c,&generation,shared){*generation.recovery.lock().unwrap_or_else(|p|p.into_inner())=Some(error);}
        }
        if generation.input_released(){if let Err(error)=generation.start_retirement(){*generation.retirement_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error);}}
        if generation.retired.load(Ordering::Acquire){
            match generation.retirement.lock().unwrap_or_else(|p|p.into_inner()).joined(){
                Ok(Some(joined))=>{let _=generation.retirement_joined.set(joined);},Ok(None)=>{},
                Err(error)=>*generation.retirement_error.lock().unwrap_or_else(|p|p.into_inner())=Some(error),
            }
        }
    }
    let complete=c.generations.iter().filter(|(_,g)|g.installed.get().is_some()&&g.retirement_joined.get().is_some()&&g.retired.load(Ordering::Acquire)).map(|(id,_)|id.clone()).collect::<Vec<_>>();
    for id in complete{c.generations.remove(&id);}
    let disposal=FinalizationBudget::begin(Duration::from_secs(1))?;
    let _=seeds::collect_retired_in_worker(c,&disposal)?;
    discover(c,shared)
}
fn qualified_factory(worker:&Worker,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<Option<WorkerJoined>> {
    let Some(joined)=worker.joined()? else{return Ok(None);};time(original,cancelled)?;Ok(Some(joined))
}
fn advance(c:&mut WorkerCarrier,generation:&Arc<SourceGeneration>,shared:&Arc<Shared>)->Result<()> {
    if generation.snapshot.get().is_none(){generation.qualify_snapshot()?;}
    if !generation.input_released()||generation.installed.get().is_some(){return Ok(());}
    let mut reservation=generation.reservation.lock().unwrap_or_else(|p|p.into_inner());
    if reservation.is_none(){
        if shared.kernel.status()!=KernelStartupStatus::Ready{return Ok(());}
        if c.root_rpc_held||c.seeds.len()+c.source_reservations.len()>=MAX_SEEDS{return Ok(());}
        let mut held=match generation.owners.try_lock(){Ok(h)=>h,Err(TryLockError::WouldBlock)=>return Ok(()),Err(_)=>anyhow::bail!("original generation carrier poisoned")};
        ensure!(held.error.is_none()&&held.panic.is_none(),"original generation factory failed; owners retained");
        match held.source.as_ref().context("inert snapshot already moved")?.check_shared_admission(){
            Ok(())=>{},Err(error) if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>return Ok(()),Err(error)=>return Err(error),
        }
        // Reserve actual capacity first, under the common machine lock.
        let admitted=Arc::new(RootSourceReservation{id:uuid::Uuid::new_v4().to_string(),generation:Arc::downgrade(generation),original:OnceLock::new(),cancelled:generation.cancelled.clone(),queued:AtomicBool::new(false),permit:Mutex::new(Some(RootReadStartPermit))});
        c.source_reservations.insert(admitted.id.clone(),admitted.clone());*reservation=Some(admitted.clone());
        let initialize=(||->Result<()>{admitted.original.set(FinalizationBudget::begin(Duration::from_secs(30))?).map_err(|_|anyhow::anyhow!("original Root clock occupied"))?;
            shared.kernel.ticket()?.assert_new_startup_off_app(admitted.budget()?,&generation.cancelled)?;held.source.as_ref().unwrap().prepared_current(admitted.budget()?,&generation.cancelled)?;Ok(())})();
        if let Err(error)=initialize{held.error=Some(error);ensure!(admitted.permit.lock().unwrap_or_else(|p|p.into_inner()).take().is_some(),"Root dispatch permit unexpectedly spent");c.source_reservations.remove(&admitted.id);anyhow::bail!("original Root admission failed before worker dispatch; snapshot retained");}
        drop(held);
        if !generation.start_bootstrap(admitted.clone())?{ensure!(admitted.permit.lock().unwrap_or_else(|p|p.into_inner()).take().is_some(),"known absent thread spent Root permit");c.source_reservations.remove(&admitted.id);anyhow::bail!("original Root worker did not spawn; snapshot retained");}
    }
    let Some(admitted)=reservation.as_ref()else{return Ok(());};
    let Some(joined)=generation.bootstrap.lock().unwrap_or_else(|p|p.into_inner()).joined()? else{return Ok(());};
    let held=match generation.owners.try_lock(){Ok(h)=>h,Err(TryLockError::WouldBlock)=>return Ok(()),Err(_)=>anyhow::bail!("original Root preparation carrier poisoned")};
    ensure!(held.error.is_none()&&held.panic.is_none(),"original Root generation preparation failed; owners retained");
    time(admitted.budget()?,&admitted.cancelled)?;
    let seed=held.seed.as_ref().context("new actual Root seed absent")?;let cache=held.cache.as_ref().context("new actual cache absent")?;
    ensure!(c.seeds.get(&seed.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&seed.cell)),"new seed not held by actual machine");
    let source=match seed.cell.owner.try_lock(){Ok(source)=>source,Err(TryLockError::WouldBlock)=>return Ok(()),Err(_)=>anyhow::bail!("new actual source poisoned")};
    source.source.prepared_current(admitted.budget()?,&admitted.cancelled)?;cache.ingress_original_ready()?;
    let project=generation.project()?;let mut owner=match project.cell.owner.try_lock(){Ok(owner)=>owner,Err(TryLockError::WouldBlock)=>return Ok(()),Err(_)=>anyhow::bail!("actual project poisoned")};
    owner.owner.assert_current_bounded(admitted.budget()?,&admitted.cancelled)?;
    ensure!(owner.owner.journal_parts().0.project_state(source.source.owner().project_id.as_str())?.integration_head==source.source.accepted_head(),"accepted head changed before generation adoption");
    shared.kernel.ticket()?.assert_new_startup_off_app(admitted.budget()?,&admitted.cancelled)?;
    owner.current_source=Some(CurrentSource{seed:seed.clone(),cache:cache.clone(),generation:Some(generation.clone())});
    time(admitted.budget()?,&admitted.cancelled)?;
    let _=generation.installed.set(Installed{_bootstrap:joined,_original:admitted.budget()?.clone()});Ok(())
}

fn discover(c:&mut WorkerCarrier,shared:&Arc<Shared>)->Result<()> {
    if c.generations.len()>=MAX_GENERATIONS{return Ok(());}
    let projects=c.projects.values().cloned().collect::<Vec<_>>();
    for project in projects {
        if c.generations.values().any(|g|g.project_id==project.id&&g.installed.get().is_none()){continue;}
        let mut owner=match project.cell.owner.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>continue,Err(_)=>continue};
        let Some(current)=&owner.current_source else{continue;};let seed=current.seed.clone();
        let source=match seed.cell.owner.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>continue,Err(_)=>continue};
        let head=owner.owner.journal_parts().0.project_state(source.source.owner().project_id.as_str())?.integration_head;
        if head==source.source.accepted_head(){continue;}
        let scopes=source.tasks.values().cloned().collect::<Vec<_>>();
        for scope in scopes {
            let mut task=match scope.cell.owner.try_lock(){Ok(v)=>v,Err(TryLockError::WouldBlock)=>continue,Err(_)=>continue};
            if task.generation.is_some(){continue;}
            let Some(seeds::Completion::Published(Some(acceptance)))=&mut task.completion else{continue;};
            acceptance.settled_joined_task()?;
            let original=FinalizationBudget::begin(Duration::from_secs(30))?;let cancelled=Arc::new(AtomicBool::new(false));
            let generation=Arc::new(SourceGeneration{id:uuid::Uuid::new_v4().to_string(),project_id:project.id.clone(),project:Arc::downgrade(&project.cell),old_seed_id:seed.id.clone(),old_seed:Arc::downgrade(&seed.cell),scope:Arc::downgrade(&scope.cell),shared:Arc::downgrade(shared),original:original.clone(),cancelled:cancelled.clone(),
                owners:Mutex::new(Owners{source:None,seed:None,cache:None,cache_thread:None,cache_result:None,release:None,error:None,panic:None}),factory:Mutex::new(Worker::new()),bootstrap:Mutex::new(Worker::new()),retirement:Mutex::new(Worker::new()),snapshot:OnceLock::new(),released:OnceLock::new(),reservation:Mutex::new(None),installed:OnceLock::new(),recovery:Mutex::new(None),retirement_error:Mutex::new(None),retired:AtomicBool::new(false),retirement_joined:OnceLock::new()});
            c.generations.insert(generation.id.clone(),generation.clone());
            let mut held=generation.owners.lock().unwrap_or_else(|p|p.into_inner());
            let result=ManagedInitialSourceOwner::capture_successor_retaining(&mut held.source,&mut owner.owner,&source.source,acceptance,&original,&cancelled);
            task.generation=Some(generation.clone());
            if let Err(error)=result{held.error=Some(error);}else if let Err(error)=generation.start_factory(){held.error=Some(error);}break;
        }
    }Ok(())
}

impl MachineControl {
    pub(crate) fn register_initial_current_source(&self,project:&ProjectTicket,seed:&SeedTicket,cache:&SeedCacheTicket,original:&FinalizationBudget)->Result<()> {
        let c=self.carrier_for_source(original)?;ensure!(c.projects.get(&project.id).is_some_and(|p|Arc::ptr_eq(&p.cell,&project.cell))&&c.seeds.get(&seed.id).is_some_and(|s|Arc::ptr_eq(&s.cell,&seed.cell)),"foreign actual initial source");
        let source=seeds::lock(&seed.cell,original)?;source.source.prepared_current(original,&source.cancelled)?;cache.ingress_original_ready()?;
        let mut owner=seeds::lock(&project.cell,original)?;owner.owner.assert_current_bounded(original,&source.cancelled)?;
        ensure!(owner.current_source.is_none(),"initial source already installed");owner.current_source=Some(CurrentSource{seed:seed.clone(),cache:cache.clone(),generation:None});Ok(())
    }
    pub(crate) fn current_source_for_task(&self,project:&ProjectTicket,original:&FinalizationBudget)->Result<CurrentSourceAdmission> {
        let c=self.carrier_for_source(original)?;ensure!(c.projects.get(&project.id).is_some_and(|p|Arc::ptr_eq(&p.cell,&project.cell)),"foreign actual project");
        let mut owner=seeds::lock(&project.cell,original)?;let Some(current)=owner.current_source.as_ref()else{return Ok(CurrentSourceAdmission::Pending);};
        let seed=current.seed.clone();let cache=current.cache.clone();let installed=current.generation.as_ref().is_none_or(|g|g.installed.get().is_some());
        let source=seeds::lock(&seed.cell,original)?;
        // A changed accepted head is ordinary generation maintenance; loss of
        // actual local source custody keeps the existing quarantine behavior.
        if let Err(error)=source.source.assert_original_retained_custody(){project.cell.phase.store(RECOVERY,Ordering::Release);return Err(error);}
        if source.source.shared_admission_closed(){return Ok(CurrentSourceAdmission::Fenced);}
        let head=owner.owner.journal_parts().0.project_state(source.source.owner().project_id.as_str())?.integration_head;
        if !installed||head!=source.source.accepted_head(){return Ok(CurrentSourceAdmission::Pending);}
        if let Err(error)=source.source.prepared_current(original,&source.cancelled){
            if let Err(custody)=source.source.assert_original_retained_custody(){project.cell.phase.store(RECOVERY,Ordering::Release);return Err(custody);}
            if source.source.shared_admission_closed(){return Ok(CurrentSourceAdmission::Fenced);}
            let head=owner.owner.journal_parts().0.project_state(source.source.owner().project_id.as_str())?.integration_head;
            if head!=source.source.accepted_head(){return Ok(CurrentSourceAdmission::Pending);}return Err(error);
        }
        Ok(CurrentSourceAdmission::Ready{seed:seed.clone(),cache})
    }
    fn carrier_for_source(&self,original:&FinalizationBudget)->Result<std::sync::MutexGuard<'_,WorkerCarrier>> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"source maintenance requires off App");
        loop{original.remaining()?;match self.shared.carrier.try_lock(){Ok(c)=>return Ok(c),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("original machine owner poisoned")}}
    }
    fn submit_reserved_generation_seed(&self,reservation:&Arc<RootSourceReservation>,seed:&SeedTicket)->Result<bool> {
        let mut c=match self.shared.carrier.try_lock(){Ok(c)=>c,Err(TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("machine Root owner poisoned")};
        time(reservation.budget()?,&reservation.cancelled)?;ensure!(!reservation.queued.load(Ordering::Acquire)&&c.source_reservations.get(&reservation.id).is_some_and(|r|Arc::ptr_eq(r,reservation)),"original Root reservation differs/already dispatched");
        if c.root_rpc_held{return Ok(false);} // Same reserved clock/key/permit; no Root dispatch.
        let generation=reservation.generation.upgrade().context("original source reservation owner absent")?;ensure!(generation.input_released()&&seed.id==reservation.id,"Root reservation has no actual joined inert snapshot");
        ensure!(self.shared.phase.load(Ordering::Acquire)==SERVING&&!c.seeds.contains_key(&seed.id),"Root source queue unavailable/duplicate");
        let permit=reservation.permit.lock().unwrap_or_else(|p|p.into_inner()).take().context("original Root dispatch already attempted")?;
        c.seeds.insert(seed.id.clone(),seed.clone());
        match self.send.try_send(Job::Seed(seed.clone())){
            Ok(())=>{drop(permit);reservation.queued.store(true,Ordering::Release);c.source_reservations.remove(&reservation.id);Ok(true)},
            Err(mpsc::TrySendError::Full(_))=>{c.seeds.remove(&seed.id);*reservation.permit.lock().unwrap_or_else(|p|p.into_inner())=Some(permit);Ok(false)},
            Err(mpsc::TrySendError::Disconnected(_))=>{c.seeds.remove(&seed.id);*reservation.permit.lock().unwrap_or_else(|p|p.into_inner())=Some(permit);anyhow::bail!("actual Root queue disconnected; reservation retained")}}
    }
}

pub(super) fn is_generation_seed(c:&WorkerCarrier,seed:&SeedTicket)->bool {
    c.generations.values().any(|generation|generation.reservation.try_lock().ok().is_some_and(|held|held.as_ref().is_some_and(|r|r.id==seed.id&&r.queued.load(Ordering::Acquire))))
        &&c.seeds.get(&seed.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&seed.cell))
}

#[cfg(test)]
#[path="managed_source_generation_worker_tests.rs"]
mod tests;

#[path="managed_current_source_sdk.rs"]
mod current_sdk;
pub(crate) use current_sdk::{CurrentSourceSdkCampaign,CurrentSdkBuildState,CurrentSourceSdkCleanup};
