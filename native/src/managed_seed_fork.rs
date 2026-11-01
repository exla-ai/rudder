//! Native-owned ORIGINAL VCS fork cell. No late pathname certification. Only
//! actual Joined custody and closed seed admission authorize localization.
#![cfg(target_os="linux")]
use crate::{managed_app::{JoinedManagedTask,NeverStartedManagedTask},managed_launch::ManagedLocalLaunchInputs};
use anyhow::{ensure,Context,Result};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,protocol::ObjectRef};
use rudder_vcs_bridge::{AttemptBinding,LiveObservation,seed::{VerifiedMetadataTemplate,PrivateMetadataFork,
    PrivateMetadataForkOwner,LocalizedMetadataFork,NeverLinkedMetadataFork,ForkLocalizationLimits,fork_private_metadata_retaining}};
use rudder_native::root_storage_uses::{SeedTaskUseGate,SealedSeedTaskUse};
use std::{path::{Path,PathBuf},sync::{Arc,Weak,Mutex,OnceLock,TryLockError,atomic::{AtomicBool,AtomicUsize,Ordering}},time::Duration};

/// A refusal barrier, never a helper/actor/reader join receipt. Every task
/// factory and live metadata user is separately retained by the machine scope.
pub(crate) struct SeedAdmission {closed:AtomicBool,factories:AtomicUsize}
impl SeedAdmission {
    pub(crate) fn new()->Arc<Self> {Arc::new(Self {closed:AtomicBool::new(false),factories:AtomicUsize::new(0)})}
    pub(crate) fn close(&self) {self.closed.store(true,Ordering::Release);}
    pub(crate) fn check_open(&self)->Result<()> {ensure!(!self.closed.load(Ordering::Acquire),"seed admission closed");Ok(())}
    pub(crate) fn check_closed(&self)->Result<()> {ensure!(self.closed.load(Ordering::Acquire),"seed admission still open");Ok(())}
}
/// Actual original factory thread custody. This is an admission owner, not a
/// seed drain receipt; never releases on Drop or callback return.
pub(crate) struct SeedFactoryOwner {
    gate:Arc<SeedAdmission>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    thread:Option<std::thread::ThreadId>,worker:Option<rudder_engine::execution::handoff::OwnedWorkerJoin>,
    joined:Option<rudder_engine::execution::handoff::WorkerJoined>,charged:bool,start:Option<SeedTaskUseGate>,
}
impl SeedFactoryOwner {
    pub(crate) fn reserve_retaining(out:&mut Option<Self>,gate:&Arc<SeedAdmission>,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        time(original,&cancelled)?;gate.check_open()?;ensure!(out.is_none(),"factory reservation already exists");
        gate.factories.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<2).then_some(n+1))
            .map_err(|_|anyhow::anyhow!("two actual seed factory workers still unjoined"))?;
        *out=Some(Self {gate:gate.clone(),original:original.clone(),cancelled,thread:None,worker:None,joined:None,charged:true,start:None});
        time(original,&out.as_ref().unwrap().cancelled)
    }
    /// Register before allowing that actual thread to access source/template.
    /// Every Err leaves a staged original handle or the caller's same handle.
    pub(crate) fn register_retaining(&mut self,worker:&mut Option<std::thread::JoinHandle<()>>)->Result<()> {
        time(&self.original,&self.cancelled)?;self.gate.check_open()?;
        if self.worker.is_none() {
            ensure!(worker.is_some(),"original factory thread absent");
            self.thread=Some(worker.as_ref().unwrap().thread().id());
            self.worker=Some(rudder_engine::execution::handoff::OwnedWorkerJoin::new(worker.take().unwrap()));
        } else {ensure!(worker.is_none(),"another factory thread supplied after original registration");}
        // Actual handle is staged above BEFORE the linear gate is bound. A
        // refused try-lock leaves this SAME handle; retry only finishes binding.
        if let Some(start)=&self.start {if !start.factory_is_bound(){start.bind_original_factory(self.worker.as_ref().unwrap())?;}}
        time(&self.original,&self.cancelled)
    }
    pub(crate) fn attach_start_retaining(&mut self,start:&SeedTaskUseGate)->Result<()> {
        ensure!(self.start.is_none()&&self.worker.is_none(),"start gate must precede original factory/source access");self.start=Some(start.clone());Ok(())
    }
    /// Pure late ownership capture only. This grants no source read/adoption
    /// and works after original timeout/cancel so the real outcome is retained.
    pub(crate) fn check_original_result_thread(&self)->Result<()> {
        ensure!(self.worker.is_some()&&self.thread==Some(std::thread::current().id())&&self.joined.is_none(),"outcome is outside original registered factory thread");
        if let Some(start)=&self.start {ensure!(start.factory_is_bound(),"original factory start binding absent");}Ok(())
    }
    pub(crate) fn registration_present(&self)->bool {self.worker.is_some()}
    pub(crate) fn check_original_thread(&self)->Result<()> {
        time(&self.original,&self.cancelled)?;self.gate.check_open()?;
        ensure!(self.worker.is_some()&&self.thread==Some(std::thread::current().id())&&self.joined.is_none(),"source access is outside original registered factory thread");
        if let Some(start)=&self.start {ensure!(start.factory_is_bound(),"original source gate not bound to retained factory");}Ok(())
    }
    /// Clone ACTUAL join custody, then release every machine/scope lock before
    /// waiting: the original factory may still need those locks to finish.
    pub(crate) fn join_driver(&self)->Result<(rudder_engine::execution::handoff::OwnedWorkerJoin,FinalizationBudget)> {
        Ok((self.worker.as_ref().context("original factory thread not registered")?.clone(),self.original.clone()))
    }
    /// OFF App: retain/start one original join observer, then sample its actual
    /// cached result. No timeout/event or
    /// callback can release this permit. A lost receipt remains in OwnedWorkerJoin.
    pub(crate) fn poll_joined(&mut self)->Result<bool> {
        if self.joined.is_none() {
            let worker=self.worker.as_ref().context("original factory thread absent")?;worker.start_join_observer()?;
            self.joined=worker.try_joined()?;
        }
        if self.joined.is_some()&&self.charged {self.charged=false;self.gate.factories.fetch_sub(1,Ordering::AcqRel);}
        Ok(self.joined.is_some())
    }
    pub(crate) fn is_joined(&self)->bool {self.joined.is_some()}
    pub(crate) fn original(&self)->&FinalizationBudget {&self.original}
    pub(crate) fn cancellation(&self)->Arc<AtomicBool> {self.cancelled.clone()}
}
enum UnusedRoute {Localize,NeverLinked}
struct State {unused_route:Option<UnusedRoute>,owner:Option<PrivateMetadataForkOwner>,localized:Option<LocalizedMetadataFork>,never_linked:Option<NeverLinkedMetadataFork>,attempted:bool,acknowledged:bool}
pub(crate) struct ManagedSeedForkCell {
    template:Weak<VerifiedMetadataTemplate>,seed:ObjectRef,manifest:ObjectRef,root:ObjectRef,source:ObjectRef,cas:PathBuf,
    cache:Option<Weak<crate::managed_preparation_cache::ManagedPreparationCache>>,
    admission:Arc<SeedAdmission>,start:SeedTaskUseGate,original:FinalizationBudget,cancelled:Arc<AtomicBool>,binding:OnceLock<AttemptBinding>,state:Mutex<State>,
}
impl ManagedSeedForkCell {
    /// Called by actual seed-source admission while Root/template/source owners
    /// are held. Exact source/Root identities, not a persisted ready claim.
    pub(crate) fn new(template:&Arc<VerifiedMetadataTemplate>,root:ObjectRef,source:ObjectRef,
        admission:Arc<SeedAdmission>,start:SeedTaskUseGate,cas:&Path,cache:Option<&Arc<crate::managed_preparation_cache::ManagedPreparationCache>>,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<Arc<Self>> {
        admission.check_open()?;let seed=template.retained_seed().context("actual retained template required")?.clone();
        Ok(Arc::new(Self {template:Arc::downgrade(template),seed,manifest:template.identity().clone(),root,source,cas:cas.into(),cache:cache.map(Arc::downgrade),admission,start,original:original.clone(),cancelled,binding:OnceLock::new(),
            state:Mutex::new(State {unused_route:None,owner:None,localized:None,never_linked:None,attempted:false,acknowledged:false})}))
    }
    pub(crate) fn start_gate(&self)->&SeedTaskUseGate {&self.start}
    pub(crate) fn original_binding(&self)->Option<&AttemptBinding> {self.binding.get()}
    pub(crate) fn matches_inputs(&self,inputs:&ManagedLocalLaunchInputs)->Result<()> {
        self.admission.check_open()?;
        ensure!(Weak::ptr_eq(&self.template,&Arc::downgrade(&inputs.seed))&&inputs.runnable_root==self.root
            &&inputs.seed_source==self.source&&inputs.store.root()==self.cas&&inputs.seed.retained_seed()==Some(&self.seed),"task source/template differs from actual admitted seed");Ok(())
    }
    pub(crate) fn matches_lifetime(&self,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        ensure!(self.original.constraint()==original.constraint()&&Arc::ptr_eq(&self.cancelled,cancelled),"task replaced original source budget/cancellation");Ok(())
    }
    pub(crate) fn matches_cache(&self,actual:&Option<Arc<crate::managed_preparation_cache::ManagedPreparationCache>>)->Result<()> {
        ensure!(match (&self.cache,actual) {(None,None)=>true,(Some(expected),Some(actual))=>Weak::ptr_eq(expected,&Arc::downgrade(actual)),_=>false},"task uses another untracked cache owner");Ok(())
    }
    fn lock(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<std::sync::MutexGuard<'_,State>> {
        loop {time(original,cancelled)?;match self.state.try_lock() {
            Ok(held)=>return Ok(held),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),
            Err(_)=>anyhow::bail!("original fork cell poisoned; owner retained")}}
    }
    pub(crate) fn prepare_retaining(&self,template:&Arc<VerifiedMetadataTemplate>,destination:&Path,binding:AttemptBinding,
        activation:Option<&LiveObservation>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<PrivateMetadataFork> {
        time(original,cancelled)?;time(&self.original,&self.cancelled)?;self.admission.check_open()?;
        ensure!(self.original.constraint()==original.constraint()&&std::ptr::eq(cancelled,Arc::as_ptr(&self.cancelled)),"fork replaced original task budget/cancellation");
        ensure!(Weak::ptr_eq(&self.template,&Arc::downgrade(template))&&binding.seed==self.seed,"fork differs from original admitted template");
        let mut held=self.lock(original,cancelled)?;ensure!(!held.attempted,"original fork already attempted");
        self.start.claim_fork_creation()?;held.attempted=true;
        self.binding.set(binding.clone()).map_err(|_|anyhow::anyhow!("original fork binding already staged"))?;
        // VCS stages the actual directory/source/alternates owner BEFORE writes.
        let receipt=fork_private_metadata_retaining(&mut held.owner,template,destination,binding,activation,original,cancelled)?;
        ensure!(held.owner.as_ref().is_some_and(PrivateMetadataForkOwner::is_ready),"original retaining fork ACK/owner not ready");
        held.acknowledged=true;time(original,cancelled)?;Ok(receipt)
    }
    /// Fixed native quiescence boundary. Main has no private-fork gix/jj ODB
    /// owner: workspace_vcs services run in the actual reaped helper. The
    /// acceptor/initial source use their separate internal integration Git store.
    /// New in-process fork readers must become separately retained scopes.
    fn check_joined(&self,joined:&JoinedManagedTask,binding:&AttemptBinding,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.admission.check_closed()?;joined.check_seed_localization_owner(original,cancelled)?;ensure!(joined.is_settled(),"original task acceptance/discard remains unsettled");
        joined.helper_exit().matches_checkpoint(joined.helper_custody(),joined.shutdown())?;
        let actual=joined.prepared().seed_fork.as_ref().context("original fork carrier absent from actual launch")?;
        ensure!(std::ptr::eq(self,Arc::as_ptr(actual)),"joined task holds another actual fork owner");
        let assignment=&joined.prepared().assignment;
        ensure!(binding.seed==assignment.seed&&binding.project_id==assignment.fence.owner.project_id
            &&binding.owner==assignment.fence.owner_epoch()&&binding.run_id==assignment.fence.run_id
            &&binding.run_epoch==assignment.fence.run_epoch&&binding.attempt_id==assignment.fence.attempt_id
            &&binding.view_id==assignment.observation.view_id,"localized fork differs from exact joined assignment");Ok(())
    }
    /// Caller proves the ORIGINAL task factory worker/TLS joined before this
    /// method; its owning scope remains held. No caller boolean/callback accepted.
    pub(crate) fn localize_after_joined(&self,joined:&JoinedManagedTask,original:&FinalizationBudget,
        cancelled:&AtomicBool,reconcile:bool)->Result<()> {
        let mut held=self.lock(original,cancelled)?;
        ensure!(held.acknowledged,"original fork creation ACK absent");
        let State {owner,localized,..}=&mut *held;
        let owner=owner.as_mut().context("original fork owner absent")?;self.check_joined(joined,owner.binding(),original,cancelled)?;
        if let Some(proof)=localized {return proof.check_current(original,cancelled);}
        if reconcile {owner.reconcile_localization_retaining(localized,ForkLocalizationLimits::default(),original,cancelled,|b|self.check_joined(joined,b,original,cancelled))?;}
        else {owner.localize_retaining(localized,ForkLocalizationLimits::default(),original,cancelled,|b|self.check_joined(joined,b,original,cancelled))?;}
        // Actual opaque ACK was staged by VCS BEFORE fallible postchecks.
        time(original,cancelled)?;localized.as_ref().context("original localization ACK absent")?.check_current(original,cancelled)
    }
    /// Fixed never-started quiescence: the ORIGINAL App slot retains the whole
    /// unused carrier, real factory/preparation TLS and broker joins, and the
    /// original unspent helper/enrollment permits. No callback from a caller.
    pub(crate) fn release_after_never_started(&self,unused:&NeverStartedManagedTask,original:&FinalizationBudget,
        cancelled:&AtomicBool,reconcile:bool)->Result<()> {
        let mut held=self.lock(original,cancelled)?;
        if held.unused_route.is_none() {
            let original_owner=held.owner.as_ref().context("original partial/ready fork owner absent; creation remains held")?;
            held.unused_route=Some(if original_owner.is_ready(){UnusedRoute::Localize}else{UnusedRoute::NeverLinked});
        }
        let State {owner,localized,never_linked,unused_route,..}=&mut *held;
        let owner=owner.as_mut().context("original partial/ready fork owner absent; creation remains held")?;
        unused.check_quiescent(self,owner.binding(),original,cancelled)?;
        if let Some(proof)=localized {return proof.check_current(original,cancelled);}
        if let Some(proof)=never_linked {return proof.check_current(original,cancelled);}
        if matches!(unused_route,Some(UnusedRoute::Localize)) {
            if reconcile {owner.reconcile_localization_retaining(localized,ForkLocalizationLimits::default(),original,cancelled,|b|unused.check_quiescent(self,b,original,cancelled))?;}
            else {owner.localize_retaining(localized,ForkLocalizationLimits::default(),original,cancelled,|b|unused.check_quiescent(self,b,original,cancelled))?;}
        } else if reconcile {owner.reconcile_never_linked_retaining(never_linked,original,cancelled,|b|unused.check_quiescent(self,b,original,cancelled))?;}
        else {owner.release_never_linked_retaining(never_linked,original,cancelled,|b|unused.check_quiescent(self,b,original,cancelled))?;}
        // Genuine VCS proof is staged before all postchecks, retained on Err.
        time(original,cancelled)
    }
    pub(crate) fn with_unused_dependency<R>(&self,original:&FinalizationBudget,cancelled:&AtomicBool,
        use_original:impl FnOnce(&mut Option<LocalizedMetadataFork>,&mut Option<NeverLinkedMetadataFork>)->Result<R>)->Result<R> {
        let mut held=self.lock(original,cancelled)?;let State {localized,never_linked,..}=&mut *held;
        if let Some(proof)=localized.as_ref(){ensure!(proof.seed_manifest()==&self.manifest,"localized manifest differs");}
        if let Some(proof)=never_linked.as_ref(){ensure!(proof.seed_manifest()==&self.manifest,"never-linked manifest differs");}
        use_original(localized,never_linked)
    }
    /// Borrow actual opaque proof; caller stages it into the native ledger. The
    /// closure is crate-private plumbing, never a public quiescence authority.
    pub(crate) fn with_localized<R>(&self,original:&FinalizationBudget,cancelled:&AtomicBool,
        use_original:impl FnOnce(&mut Option<LocalizedMetadataFork>)->Result<R>)->Result<R> {
        let mut held=self.lock(original,cancelled)?;
        if let Some(proof)=&held.localized{ensure!(proof.seed_manifest()==&self.manifest,"localized seed manifest differs from original template");}
        use_original(&mut held.localized)
    }
}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original fork operation cancelled");}original.remaining()?;Ok(())}

#[cfg(test)]
#[path="managed_seed_factory_tests.rs"]
mod factory_tests;

#[cfg(test)]
mod never_started_factory_tests {use super::*;include!("managed_never_started_factory_tests.rs");}
