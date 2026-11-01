//! Process-local seed-use custody. Only actual original thread joins issue use
//! receipts. Dropping a guard/result never releases ledger capacity or users.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use rudder_vcs_bridge::{AttemptBinding,seed::{LocalizedMetadataFork,NeverLinkedMetadataFork}};
use std::sync::atomic::AtomicBool;
use std::{sync::{Arc,Mutex,TryLockError,atomic::{AtomicUsize,Ordering}},thread::{self,JoinHandle},time::Duration};

#[path="root_storage_use_start.rs"]
mod start;
pub use start::{SeedTaskUseGate,SealedSeedTaskUse};

const MAX_JOIN_WORKERS:usize=2;
const MAX_USES:usize=272; // 256 helper/task scopes plus bounded host plumbing.
#[derive(Clone,Copy,PartialEq,Eq)]
pub enum SeedUseKind { Task, Template, Fork, Cache }
trait ActualJoined:Send+Sync {fn joined(&self)->bool;fn drive_join(self:Arc<Self>)->Result<()>;}
struct Entry {kind:SeedUseKind,readers:Vec<uuid::Uuid>,work:Option<Arc<dyn ActualJoined>>,localized:Option<Arc<LocalizedMetadataFork>>,binding:Option<AttemptBinding>,start:Option<SeedTaskUseGate>,never_started:Option<SealedSeedTaskUse>,never_linked:Option<Arc<NeverLinkedMetadataFork>>}
struct State {closed:bool,entries:Vec<Entry>}
#[derive(Clone)]
pub(crate) struct SeedUseLedger {state:Arc<Mutex<State>>,workers:Arc<AtomicUsize>}
impl SeedUseLedger {
    pub(crate) fn new()->Self {Self {state:Arc::new(Mutex::new(State {closed:false,entries:Vec::with_capacity(MAX_USES)})),workers:Arc::new(AtomicUsize::new(0))}}
    pub(crate) fn acquire(&self,kind:SeedUseKind)->Result<SeedUseGuard> {
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("native seed use ledger poisoned"))?;
        ensure!(!state.closed&&state.entries.len()<MAX_USES,"native seed use admission closed/full");
        let id=state.entries.len();state.entries.push(Entry {kind,readers:Vec::new(),work:None,localized:None,binding:None,start:None,never_started:None,never_linked:None});
        Ok(SeedUseGuard {ledger:self.clone(),id})
    }
    /// Original two-use registration, BEFORE handing out source or starting
    /// its factory. The same linear gate stays in BOTH ledger entries even if
    /// caller delivery fails. This is not an actor or helper join receipt.
    pub(crate) fn register_task_start_retaining(&self,out:&mut Option<SeedTaskUseGate>,task:&SeedUseGuard,
        fork:&SeedUseGuard,seed:&rudder_engine::protocol::ObjectRef,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;ensure!(out.is_none()&&self.matches(task)&&self.matches(fork)&&task.id!=fork.id,"original task start guards differ");
        seed.validate()?;
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("seed ledger poisoned"))?;
        ensure!(!state.closed,"seed admission closed");
        for (id,kind) in [(task.id,SeedUseKind::Task),(fork.id,SeedUseKind::Fork)] {
            let entry=state.entries.get(id).context("original use absent")?;
            ensure!(entry.kind==kind&&entry.work.is_none()&&entry.start.is_none()&&entry.binding.is_none()&&entry.readers.is_empty(),"original task start already issued/used");
        }
        let actual=SeedTaskUseGate::new(seed.clone());
        state.entries[task.id].start=Some(actual.clone());state.entries[fork.id].start=Some(actual.clone());*out=Some(actual);
        drop(state);original.remaining()?;Ok(())
    }
    pub(crate) fn acknowledge_never_started_retaining(&self,out:&mut Option<RetainedUnusedFork>,task:&SeedUseGuard,
        fork:&SeedUseGuard,seal:&SealedSeedTaskUse,localized:&mut Option<LocalizedMetadataFork>,
        never_linked:&mut Option<NeverLinkedMetadataFork>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"never-started acknowledgment cancelled");
        ensure!(out.is_none()&&self.matches(task)&&self.matches(fork)&&task.id!=fork.id,"never-started guards/output differ");
        let dependency=if seal.fork_was_never_claimed() {
            ensure!(localized.is_none()&&never_linked.is_none(),"unspent fork permit conflicts with supplied dependency receipt");0
        } else if let Some(proof)=localized.as_ref() {proof.check_current(original,cancelled)?;1}
          else {never_linked.as_ref().context("original fork dependency release ACK absent")?.check_current(original,cancelled)?;2};
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("seed ledger poisoned"))?;
        for (id,kind) in [(task.id,SeedUseKind::Task),(fork.id,SeedUseKind::Fork)] {
            let entry=state.entries.get(id).context("original task/fork use absent")?;
            ensure!(entry.kind==kind&&entry.work.is_none()&&entry.never_started.is_none()&&entry.readers.is_empty(),"original use transferred/enrolled/already acknowledged");
            seal.check_original(entry.start.as_ref().context("original task start permit absent")?)?;
            if dependency!=0 {
                let binding=localized.as_ref().map(LocalizedMetadataFork::binding).or_else(||never_linked.as_ref().map(NeverLinkedMetadataFork::binding));
                ensure!(entry.binding.as_ref()==binding&&binding.is_some(),"dependency receipt differs from original attempt");
            }
        }
        let actual=match dependency {
            0=>UnusedDependency::Uncreated(seal.clone()),
            1=>{let proof=Arc::new(localized.take().unwrap());state.entries[task.id].localized=Some(proof.clone());state.entries[fork.id].localized=Some(proof.clone());UnusedDependency::Localized(proof)},
            _=>{let proof=Arc::new(never_linked.take().unwrap());state.entries[task.id].never_linked=Some(proof.clone());state.entries[fork.id].never_linked=Some(proof.clone());UnusedDependency::NeverLinked(proof)},
        };
        state.entries[task.id].never_started=Some(seal.clone());state.entries[fork.id].never_started=Some(seal.clone());
        *out=Some(RetainedUnusedFork {actual:Arc::new(actual)});drop(state);
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"never-started acknowledgment cancelled");Ok(())
    }
    pub(crate) fn admit(&self)->Result<()> {
        ensure!(!self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?.closed,"native seed admission closed");Ok(())
    }
    pub(crate) fn close(&self)->Result<()> {self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?.closed=true;Ok(())}
    pub(crate) fn matches(&self,guard:&SeedUseGuard)->bool {Arc::ptr_eq(&self.state,&guard.ledger.state)}
    pub(crate) fn require_kind(&self,guard:&SeedUseGuard,kind:SeedUseKind)->Result<()> {
        ensure!(self.matches(guard),"seed use belongs to another owner");
        let state=self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?;
        ensure!(state.entries.get(guard.id).is_some_and(|entry|entry.kind==kind&&entry.work.is_none()),"original seed use kind/state differs");Ok(())
    }
    pub(crate) fn associate(&self,guard:&SeedUseGuard,reader:uuid::Uuid)->Result<()> {
        ensure!(self.matches(guard),"seed use belongs to another actual owner");
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?;
        ensure!(!state.entries.iter().enumerate().any(|(id,entry)|id!=guard.id&&entry.readers.contains(&reader)),"original helper already belongs to another native task use");
        let entry=state.entries.get_mut(guard.id).context("native seed use absent")?;
        ensure!(entry.kind==SeedUseKind::Task&&entry.work.is_none()&&entry.never_started.is_none(),"helper association requires original unjoined task use");
        if !entry.readers.contains(&reader) {ensure!(entry.readers.is_empty(),"task use already bound to another helper");entry.readers.push(reader);}
        Ok(())
    }
    pub(crate) fn drive_joins_off_app(&self)->Result<()> {
        let work:Vec<_>=self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?
            .entries.iter().filter_map(|entry|entry.work.clone()).collect();
        for actual in work {actual.drive_join()?;}Ok(())
    }
    pub(crate) fn acknowledge_localized_retaining(&self,out:&mut Option<RetainedLocalizedFork>,task:&SeedUseGuard,
        fork:&SeedUseGuard,proof:&mut Option<LocalizedMetadataFork>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"localization acknowledgement cancelled");
        ensure!(out.is_none()&&self.matches(task)&&self.matches(fork)&&task.id!=fork.id,"localization guards/output differ");
        proof.as_ref().context("original opaque localization ACK absent")?.check_current(original,cancelled)?;
        let mut state=self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?;
        ensure!(state.closed,"localization ACK requires closed seed admission");
        for (id,kind) in [(task.id,SeedUseKind::Task),(fork.id,SeedUseKind::Fork)] {
            let entry=state.entries.get(id).context("original task/fork use absent")?;
            ensure!(entry.kind==kind&&entry.localized.is_none()&&entry.work.is_none(),"original localization use already transferred");
            ensure!(entry.binding.as_ref()==proof.as_ref().map(LocalizedMetadataFork::binding),"localization ACK differs from original pre-enrollment attempt binding");
        }
        ensure!(state.entries[task.id].readers.len()==1,"original helper ACK association absent");
        let actual=Arc::new(proof.take().unwrap());
        state.entries[task.id].localized=Some(actual.clone());state.entries[fork.id].localized=Some(actual.clone());
        *out=Some(RetainedLocalizedFork {actual});drop(state);
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"localization acknowledgement cancelled");Ok(())
    }
    pub(crate) fn prove(&self,readers:&std::collections::BTreeSet<uuid::Uuid>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<SeedActorsJoined> {
        let state=self.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?;
        ensure!(state.closed&&!state.entries.is_empty(),"native close-admission and actual joined use set required");
        let mut actual=std::collections::BTreeSet::new();let mut retained=Vec::with_capacity(state.entries.len());let mut localized=Vec::new();let mut never_linked=Vec::new();
        for entry in &state.entries {
            let work=entry.work.as_ref().context("native seed use dropped/unsettled; original capacity remains charged")?;
            ensure!(work.joined(),"native seed actor/worker/TLS still unjoined or uncertain");
            if let Some(seal)=&entry.never_started {
                seal.check_original(entry.start.as_ref().context("original sealed gate absent")?)?;
                ensure!(entry.readers.is_empty(),"never-started task has an enrolled helper");
                if !seal.fork_was_never_claimed() {
                    if let Some(proof)=&entry.localized {if !localized.iter().any(|old|Arc::ptr_eq(old,proof)){localized.push(proof.clone());}}
                    else {let proof=entry.never_linked.as_ref().context("never-started fork source dependency still held")?;
                        if !never_linked.iter().any(|old|Arc::ptr_eq(old,proof)){never_linked.push(proof.clone());}}
                }
            } else {
                if entry.kind==SeedUseKind::Task {ensure!(entry.readers.len()==1,"native task has no original helper ACK association");}
                if matches!(entry.kind,SeedUseKind::Task|SeedUseKind::Fork) {
                    let actual=entry.localized.clone().context("durable fork localization ACK absent; original seed remains retained")?;
                    if !localized.iter().any(|prior|Arc::ptr_eq(prior,&actual)){localized.push(actual);}
                }
            }
            actual.extend(entry.readers.iter().copied());retained.push(work.clone());
        }
        ensure!(&actual==readers,"native joined helper set differs from original enrolled readers");
        drop(state);
        for proof in localized {proof.check_current(original,cancelled)?;original.remaining()?;}
        for proof in never_linked {proof.check_current(original,cancelled)?;original.remaining()?;}
        Ok(SeedActorsJoined {_ledger:self.clone(),_retained:retained})
    }
}
/// Alias of ONE actual opaque VCS ACK consumed once into its original ledger.
/// Non-Serde; clones cannot change binding or manufacture localization.
#[derive(Clone)]
pub struct RetainedLocalizedFork {actual:Arc<LocalizedMetadataFork>}
impl RetainedLocalizedFork {
    pub fn binding(&self)->&AttemptBinding {self.actual.binding()}
    pub fn check_current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {self.actual.check_current(original,cancelled)}
}
enum UnusedDependency {Uncreated(SealedSeedTaskUse),Localized(Arc<LocalizedMetadataFork>),NeverLinked(Arc<NeverLinkedMetadataFork>)}
/// Alias of actual original unspent permits OR a genuine VCS dependency-release
/// ACK. Never converts a partial fork into a complete/localized repository.
#[derive(Clone)]
pub struct RetainedUnusedFork {actual:Arc<UnusedDependency>}
impl RetainedUnusedFork {
    pub fn check_current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"unused fork settlement cancelled");
        match &*self.actual {UnusedDependency::Uncreated(seal)=>{ensure!(seal.fork_was_never_claimed(),"fork creation permit consumed");Ok(())},
            UnusedDependency::Localized(proof)=>proof.check_current(original,cancelled),UnusedDependency::NeverLinked(proof)=>proof.check_current(original,cancelled)}
    }
}
/// No constructor/Serde/Clone. Holds the actual join cells and their T/panic
/// resources; Root IPC trusts the fixed authenticated engine issuer, not JSON.
pub struct SeedActorsJoined {_ledger:SeedUseLedger,_retained:Vec<Arc<dyn ActualJoined>>}
/// Acquire BEFORE a task/fork/cache/template can use this seed. A dropped guard
/// leaves its original entry unresolved; it can never manufacture actor drain.
#[must_use="retain through an actual original joined seed-use worker"]
pub struct SeedUseGuard {ledger:SeedUseLedger,id:usize}
impl SeedUseGuard {
    /// App-pure one-shot DATA binding, before original helper enrollment. This
    /// emits no join/localization authority; genuine receipts remain mandatory.
    pub fn bind_original_attempt(&self,binding:&AttemptBinding)->Result<()> {
        binding.validate()?;
        let mut ledger=self.ledger.state.try_lock().map_err(|_|anyhow::anyhow!("original seed ledger busy/poisoned"))?;
        let entry=ledger.entries.get_mut(self.id).context("original use absent")?;
        ensure!(matches!(entry.kind,SeedUseKind::Task|SeedUseKind::Fork)&&entry.work.is_none(),"attempt binding requires original task/fork use");
        if let Some(old)=&entry.binding {ensure!(old==binding,"original seed attempt binding differs");}
        else {entry.binding=Some(binding.clone());}Ok(())
    }
}

struct JoinState<T> {
    value:Mutex<Option<T>>,join:Mutex<JoinRecord>,original:FinalizationBudget,workers:Arc<AtomicUsize>,
}
struct JoinRecord {worker:Option<JoinHandle<Result<()>>>,observer:Option<JoinHandle<()>>,
    result:Option<thread::Result<Result<()>>>,started:bool,charged:bool,
    prior:Vec<(Option<JoinHandle<()>>,thread::Result<Result<()>>)>}
impl<T:Send+'static> ActualJoined for JoinState<T> {
    fn joined(&self)->bool {self.join.try_lock().ok().is_some_and(|state|matches!(state.result.as_ref(),Some(Ok(Ok(())))))}
    fn drive_join(self:Arc<Self>)->Result<()> {
        {let held=self.join.lock().map_err(|_|anyhow::anyhow!("seed join owner poisoned"))?;
         if !held.started||held.result.is_some() {return Ok(());}}
        RetainedSeedJoin {state:self}.start_observer()
    }
}
/// Actual retained generic cleanup/join owner. T remains in its original cell
/// BEFORE spawn, through failure/unwind/result delivery and actual worker TLS.
#[must_use="keep original value/handle/result until actual joined receipt"]
pub struct RetainedSeedJoin<T:Send+'static> {state:Arc<JoinState<T>>}
pub struct JoinedSeedUse<T:Send+'static> {state:Arc<JoinState<T>>}
impl<T:Send+'static> RetainedSeedJoin<T> {
    /// All fallible checks precede ownership moves. Ledger holds the actual
    /// state even if the caller drops this result/receiver later.
    pub fn stage_retaining(slot:&mut Option<Self>,guard:&mut Option<SeedUseGuard>,value:&mut Option<T>,original:&FinalizationBudget)->Result<()> {
        original.remaining()?;ensure!(slot.is_none()&&value.is_some(),"seed join stage is not fresh");
        let held=guard.as_ref().context("original seed use guard absent")?;
        let mut ledger=held.ledger.state.lock().map_err(|_|anyhow::anyhow!("native seed ledger poisoned"))?;
        let entry=ledger.entries.get_mut(held.id).context("original seed use absent")?;
        ensure!(entry.work.is_none(),"original seed use already transferred");
        let state=Arc::new(JoinState {value:Mutex::new(None),original:original.clone(),workers:held.ledger.workers.clone(),join:Mutex::new(JoinRecord {
            worker:None,observer:None,result:None,started:false,charged:false,prior:Vec::with_capacity(2)})});
        entry.work=Some(state.clone());*slot=Some(Self {state:state.clone()});
        *state.value.lock().expect("fresh seed value cell")=value.take();drop(ledger);guard.take();Ok(())
    }
    /// Function pointer cannot capture/drop external custody on spawn refusal.
    /// The actual mutable T stays outside this worker's unwind boundary.
    pub fn start_retaining(&mut self,work:fn(&mut T)->Result<()>)->Result<()> {
        self.state.original.remaining()?;
        let mut join=self.state.join.lock().map_err(|_|anyhow::anyhow!("seed join owner poisoned"))?;
        ensure!(!join.started&&join.worker.is_none()&&join.observer.is_none(),"seed worker already attempted");
        self.state.workers.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<MAX_JOIN_WORKERS).then_some(n+1))
            .map_err(|_|anyhow::anyhow!("actual seed cleanup/join workers busy"))?;
        join.charged=true;join.started=true;
        let state=self.state.clone();
        let spawned=thread::Builder::new().name("rudder-seed-use".into()).spawn(move|| {
            state.original.remaining()?;
            let mut value=state.value.lock().unwrap_or_else(|poison|poison.into_inner());
            work(value.as_mut().context("original seed value absent")?)?;state.original.remaining()?;Ok(())
        });
        match spawned {Ok(worker)=>{join.worker=Some(worker);Ok(())},Err(error)=>{join.started=false;join.charged=false;self.state.workers.fetch_sub(1,Ordering::AcqRel);Err(error.into())}}
    }
    /// Explicit bounded reconciliation of the SAME original mutable T. Actual
    /// previous join result/panic and observer stay retained; no new deadline.
    /// The callback must reconcile partial effects, never replay an unknown ACK.
    pub fn retry_failed_retaining(&mut self,work:fn(&mut T)->Result<()>)->Result<()> {
        self.state.original.remaining()?;
        {
            let mut join=self.state.join.lock().map_err(|_|anyhow::anyhow!("seed join owner poisoned"))?;
            ensure!(join.prior.len()<2&&matches!(join.result.as_ref(),Some(Err(_))|Some(Ok(Err(_)))),"actual failed join/retry capacity required");
            let result=join.result.take().unwrap();let observer=join.observer.take();join.prior.push((observer,result));join.started=false;
        }
        self.start_retaining(work)
    }
    fn start_observer(&self)->Result<()> {
        let mut join=self.state.join.lock().map_err(|_|anyhow::anyhow!("seed join owner poisoned"))?;
        if join.observer.is_some()||join.result.is_some() {return Ok(());}
        ensure!(join.worker.is_some(),"actual seed worker absent");let state=self.state.clone();
        let observer=thread::Builder::new().name("rudder-seed-use-join".into()).spawn(move|| {
            let original=state.join.lock().unwrap_or_else(|p|p.into_inner()).worker.take();
            if let Some(original)=original {
                let result=original.join(); // Includes original TLS destructor teardown.
                let mut held=state.join.lock().unwrap_or_else(|p|p.into_inner());held.result=Some(result);
                if held.charged {held.charged=false;state.workers.fetch_sub(1,Ordering::AcqRel);}
            }
        })?;join.observer=Some(observer);Ok(())
    }
    pub fn await_joined(&mut self)->Result<JoinedSeedUse<T>> {
        self.state.original.remaining()?;self.start_observer()?;
        loop {if let Some(joined)=self.reconcile_joined()? {return Ok(joined);}
            thread::sleep(self.state.original.phase_limit(Duration::from_millis(2))?);}
    }
    /// Nonblocking observation of an ACTUAL cached join result, never a new
    /// budget or is_finished/refcount/drop inference. A panic/result Err stays.
    pub fn reconcile_joined(&self)->Result<Option<JoinedSeedUse<T>>> {
        let join=match self.state.join.try_lock() {Ok(value)=>value,Err(TryLockError::WouldBlock)=>return Ok(None),Err(_)=>anyhow::bail!("seed join result poisoned")};
        match join.result.as_ref() {
            Some(Ok(Ok(())))=>Ok(Some(JoinedSeedUse {state:self.state.clone()})),
            Some(_)=>anyhow::bail!("original seed worker/join failed; actual value/panic retained"),None=>Ok(None),
        }
    }
}
impl<T:Send+'static> JoinedSeedUse<T> {
    /// OFF App. Borrow the original T; no extraction/dropping of its custody.
    /// OFF-App bookkeeping/disposal only after the ACTUAL original cleanup
    /// JoinHandle/TLS result. T remains in its ledger cell across callback Err.
    /// This does not authorize fresh effects or renew its original budget.
    pub fn with_value_mut<R>(&self,finish:impl FnOnce(&mut T)->Result<R>)->Result<R> {
        ensure!(self.state.joined(),"actual original cleanup/TLS join absent");
        let mut value=self.state.value.lock().unwrap_or_else(|poison|poison.into_inner());
        finish(value.as_mut().context("original seed value absent")?)
    }
    pub fn with_value<R>(&self,read:impl FnOnce(&T)->Result<R>)->Result<R> {
        ensure!(self.state.joined(),"actual seed join proof absent");
        let value=self.state.value.lock().unwrap_or_else(|poison|poison.into_inner());
        read(value.as_ref().context("original seed value absent")?)
    }
}
#[cfg(test)]
#[path="root_storage_uses_tests.rs"]
mod tests;

#[cfg(test)]
#[path="root_storage_never_started_tests.rs"]
mod never_started_tests;
