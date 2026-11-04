//! Original Native runtime-use custody only. No aggregate drain issuer exists
//! yet: helper completion, never-started tasks and every immutable backing alias
//! still need their genuine terminal owners. Nothing here sends or unmounts.
use super::{ClientRequest,environment::NativeBackingState,MAX_HELPERS};
use anyhow::{ensure,Context,Result};
use rudder_engine::execution::handoff::{OwnedWorkerJoin,WorkerJoined};
use std::sync::{Arc,Mutex,TryLockError};

const MAX_USES:usize=MAX_HELPERS+2; // Original helpers + one export + one preparation.
enum Entry {
    Helper(Arc<Mutex<ClientRequest>>),
    Backing(Arc<Mutex<NativeBackingState>>),
    Preparation {worker:Option<OwnedWorkerJoin>,joined:Option<WorkerJoined>},
}
struct State {closed:bool,entries:Vec<Entry>}
#[derive(Clone)]
pub(super) struct RuntimeUseLedger {state:Arc<Mutex<State>>}
/// A single live alias of the ORIGINAL appendable ledger. This owner never
/// closes admission or removes entries, and a sample cannot be a final census.
pub(super) struct RuntimeUseObservation {ledger:RuntimeUseLedger}
pub(super) struct RuntimeUseSample {
    pub(super) closed_at_sample:bool,
    pub(super) helpers:Vec<Arc<Mutex<ClientRequest>>>,
    // Keep actual capsules alive through this sample without inspecting or
    // settling immutable read aliases. The live ledger remains their owner.
    backings:Vec<Arc<Mutex<NativeBackingState>>>,
    preparation:Option<(Option<OwnedWorkerJoin>,Option<WorkerJoined>)>,
}
/// Fixed diagnostic DATA only. Even a closed, empty sample grants no drain,
/// retirement, read-closure or release authority. Later admission may append.
#[derive(Clone,Copy,Debug,Default)]
pub struct RuntimeNativeUseData {
    pub sampled:bool,
    pub ledger_closed_at_sample:bool,
    pub helper_uses:usize,
    pub associated_original_helpers:usize,
    pub unmatched_helpers:usize,
    pub unknown_helpers:usize,
    pub unattached_helpers:usize,
    pub borrowed_helper_capsules:usize,
    pub backing_uses:usize,
    pub preparation_uses:usize,
    pub preparation_worker_registered:bool,
    pub preparation_join_observed:bool,
}
impl RuntimeUseLedger {
    pub(super) fn new()->Self {Self {state:Arc::new(Mutex::new(State {closed:false,entries:Vec::with_capacity(MAX_USES)}))}}
    fn state(&self)->Result<std::sync::MutexGuard<'_,State>> {self.state.lock().map_err(|_|anyhow::anyhow!("original runtime-use custody poisoned"))}
    fn admit(state:&State)->Result<()> {ensure!(!state.closed&&state.entries.len()<MAX_USES,"original runtime-use admission closed/full");Ok(())}
    pub(super) fn close(&self)->Result<()> {self.state()?.closed=true;Ok(())}
    pub(super) fn closed_and_empty(&self)->Result<bool> {
        let state=self.state()?;Ok(state.closed&&state.entries.is_empty())
    }
    pub(super) fn observation(&self)->RuntimeUseObservation {RuntimeUseObservation {ledger:self.clone()}}
    pub(super) fn try_contains_helper(&self,actual:&Arc<Mutex<ClientRequest>>)->Result<()> {
        let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("original runtime-use ledger busy/poisoned; owners retained"))?;
        ensure!(state.entries.iter().any(|entry|matches!(entry,Entry::Helper(old)if Arc::ptr_eq(old,actual))),"original runtime helper ledger entry absent");Ok(())
    }
    pub(super) fn helper(&self,actual:&Arc<Mutex<ClientRequest>>)->Result<()> {
        let mut state=self.state()?;Self::admit(&state)?;
        ensure!(state.entries.iter().filter(|entry|matches!(entry,Entry::Helper(_))).count()<MAX_HELPERS,
            "original runtime helper-use bound reached");
        ensure!(!state.entries.iter().any(|entry|matches!(entry,Entry::Helper(old)if Arc::ptr_eq(old,actual))),"original runtime helper registered twice");
        state.entries.push(Entry::Helper(actual.clone()));Ok(())
    }
    pub(super) fn backing(&self,actual:&Arc<Mutex<NativeBackingState>>)->Result<()> {
        let mut state=self.state()?;Self::admit(&state)?;
        ensure!(!state.entries.iter().any(|entry|matches!(entry,Entry::Backing(_))),"original runtime export already registered");
        // Retain the actual state, including FD packet and failed/pending reads,
        // before its first fallible capture, verification or FD publication.
        state.entries.push(Entry::Backing(actual.clone()));Ok(())
    }
    pub(super) fn preparation_retaining(&self,out:&mut Option<RuntimePreparationUse>)->Result<()> {
        ensure!(out.is_none(),"original runtime preparation output occupied");
        let mut state=self.state()?;Self::admit(&state)?;
        ensure!(!state.entries.iter().any(|entry|matches!(entry,Entry::Preparation{..})),"original runtime preparation already registered");
        let id=state.entries.len();state.entries.push(Entry::Preparation {worker:None,joined:None});
        *out=Some(RuntimePreparationUse {ledger:self.clone(),id});Ok(())
    }
    pub(super) fn contains_backing(&self,actual:&Arc<Mutex<NativeBackingState>>)->Result<()> {
        ensure!(self.state()?.entries.iter().any(|entry|matches!(entry,Entry::Backing(old)if Arc::ptr_eq(old,actual))),"original runtime read-custody ledger entry absent");Ok(())
    }
    pub(super) fn contains_helper(&self,actual:&Arc<Mutex<ClientRequest>>)->Result<()> {
        ensure!(self.state()?.entries.iter().any(|entry|matches!(entry,Entry::Helper(old)if Arc::ptr_eq(old,actual))),"original runtime helper ledger entry absent");Ok(())
    }
}
impl RuntimeUseObservation {
    /// Only clone original handles under the ledger try-lock. No worker poll,
    /// capsule lock or callback is allowed until this lock has been dropped.
    pub(super) fn try_sample(&self)->Result<Option<RuntimeUseSample>> {
        let state=match self.ledger.state.try_lock() {
            Ok(state)=>state,Err(TryLockError::WouldBlock)=>return Ok(None),
            Err(_)=>anyhow::bail!("original runtime-use ledger poisoned; observation owner retained"),
        };
        ensure!(state.entries.len()<=MAX_USES,"original runtime-use observation bound exceeded");
        let mut sample=RuntimeUseSample {closed_at_sample:state.closed,helpers:Vec::new(),backings:Vec::new(),preparation:None};
        for entry in &state.entries {match entry {
            Entry::Helper(actual)=>sample.helpers.push(actual.clone()),
            Entry::Backing(actual)=>sample.backings.push(actual.clone()),
            Entry::Preparation {worker,joined}=>{
                ensure!(sample.preparation.is_none(),"duplicate original runtime preparation");
                sample.preparation=Some((worker.clone(),joined.clone()));
            },
        }}
        ensure!(sample.helpers.len()<=MAX_HELPERS&&sample.backings.len()<=1,"original runtime-use kind bound exceeded");
        Ok(Some(sample))
    }
}
impl RuntimeUseSample {
    pub(super) fn data(&self)->Result<RuntimeNativeUseData> {
        let mut data=RuntimeNativeUseData {sampled:true,ledger_closed_at_sample:self.closed_at_sample,
            helper_uses:self.helpers.len(),unmatched_helpers:self.helpers.len(),backing_uses:self.backings.len(),
            preparation_uses:usize::from(self.preparation.is_some()),..RuntimeNativeUseData::default()};
        if let Some((worker,recorded))=&self.preparation {
            data.preparation_worker_registered=worker.is_some();
            if let Some(worker)=worker {
                // try_joined reads ONLY the actual cached join/TLS result. It
                // neither starts an observer nor waits/renews an old deadline.
                if let Some(actual)=worker.try_joined()? {
                    if let Some(recorded)=recorded {ensure!(recorded.same_worker(&actual),"original preparation join identity changed");}
                    data.preparation_join_observed=true;
                }
            } else {ensure!(recorded.is_none(),"original preparation receipt has no worker");}
        }
        for actual in &self.helpers {
            match actual.try_lock() {
                Ok(actual)=>{
                    data.unknown_helpers+=usize::from(actual.unknown);
                    data.unattached_helpers+=usize::from(!actual.ready||!actual.attached);
                },
                Err(TryLockError::WouldBlock)=>data.borrowed_helper_capsules+=1,
                Err(_)=>anyhow::bail!("original runtime helper capsule poisoned; same use remains charged"),
            }
        }
        Ok(data)
    }
}
/// Pre-spawn reservation only, never a task/read/actor drain proof. Dropping
/// this handle leaves the original ledger entry charged, including spawn failure.
#[must_use="retain original runtime preparation reservation and actual worker"]
pub struct RuntimePreparationUse {ledger:RuntimeUseLedger,id:usize}
impl RuntimePreparationUse {
    /// Associate the SAME actual owning handle already retained by the caller.
    /// An absorbing admission close does not prevent finishing an admitted use.
    pub fn retain_original_worker(&self,actual:&OwnedWorkerJoin)->Result<()> {
        let mut state=self.ledger.state()?;
        match state.entries.get_mut(self.id).context("original runtime preparation absent")? {
            Entry::Preparation {worker,joined}=>{ensure!(worker.is_none()&&joined.is_none(),"original runtime preparation worker already registered");*worker=Some(actual.clone());Ok(())},
            _=>anyhow::bail!("original runtime preparation kind differs"),
        }
    }
    /// Additional identity check against the original handle's ACTUAL cached
    /// join/TLS result. No caller bool, task id or substitute observer can settle it.
    pub fn retain_original_joined(&self,actual:&WorkerJoined)->Result<()> {
        let mut state=self.ledger.state()?;
        match state.entries.get_mut(self.id).context("original runtime preparation absent")? {
            Entry::Preparation {worker,joined}=>{
                let observed=worker.as_ref().context("original runtime preparation worker absent")?.try_joined()?.context("original runtime preparation/TLS not joined")?;
                ensure!(observed.same_worker(actual),"runtime preparation receipt belongs to another actual worker");
                if let Some(prior)=joined {ensure!(prior.same_worker(actual),"original runtime preparation join replaced");}
                else {*joined=Some(observed);}Ok(())
            },
            _=>anyhow::bail!("original runtime preparation kind differs"),
        }
    }
}
#[cfg(test)]
#[path="root_runtime_use_tests.rs"]
mod tests;

#[cfg(test)]
mod unshared_retirement_tests {
    use super::*;
    #[test]
    fn retirement_requires_absorbing_close_and_no_original_enrollment() {
        let unused=RuntimeUseLedger::new();assert!(!unused.closed_and_empty().unwrap());
        unused.close().unwrap();assert!(unused.closed_and_empty().unwrap());
        let mut forbidden=None;assert!(unused.preparation_retaining(&mut forbidden).is_err());
        let used=RuntimeUseLedger::new();let mut actual=None;used.preparation_retaining(&mut actual).unwrap();
        used.close().unwrap();assert!(!used.closed_and_empty().unwrap());
        let worker=OwnedWorkerJoin::new(std::thread::spawn(||{}));actual.as_ref().unwrap().retain_original_worker(&worker).unwrap();
        let joined=worker.await_joined(std::time::Duration::from_secs(2)).unwrap();actual.as_ref().unwrap().retain_original_joined(&joined).unwrap();
        // Joining preparation does not retire exported backing/actors. The
        // actual reservation remains charged to the original machine census.
        drop(actual);assert!(!used.closed_and_empty().unwrap());
    }
}
