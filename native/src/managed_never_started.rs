// Included in managed_app::local. Private constructors stay beside actual join
// and dispatch boundaries; this is not a generic bool/path/JSON issuer.
use rudder_native::root_storage_uses::SealedSeedTaskUse;

/// Born ONLY after joining the original preparation JoinHandle, including TLS.
struct ManagedPreparationJoined { _thread:thread::Thread }

/// Retains the WHOLE original supervisor carrier in place. That carrier cannot
/// be reused/dropped while this capsule is outstanding, including on dropped
/// result delivery. It exposes no live pane/control/input/resume API.
#[must_use="retain actual unused preparation owners through dependency release and cleanup"]
pub(crate) struct NeverStartedManagedTask {
    slot:Arc<TaskSlot>,generation:u64,fork:Arc<crate::managed_seed_fork::ManagedSeedForkCell>,
    seal:SealedSeedTaskUse,original:FinalizationBudget,cancelled:Arc<AtomicBool>,settled:bool,finalized:bool,
}
impl NeverStartedManagedTask {
    pub(crate) fn fork(&self)->&Arc<crate::managed_seed_fork::ManagedSeedForkCell> {&self.fork}
    pub(crate) fn seal(&self)->&SealedSeedTaskUse {&self.seal}
    fn check_carrier(&self,carrier:&TaskCarrier)->Result<()> {
        anyhow::ensure!(self.slot.generation.load(Ordering::Acquire)==self.generation&&self.slot.acceptance_pending.load(Ordering::Acquire),"original unused capsule was replaced/released");
        self.seal.check_original(self.fork.start_gate())?;
        anyhow::ensure!(self.slot.joining.try_lock().map_err(|_|anyhow::anyhow!("original join result busy/poisoned"))?.panics.is_empty(),"original worker/TLS panic remains retained");
        anyhow::ensure!(carrier.scoped_fork.as_ref().is_some_and(|fork|Arc::ptr_eq(fork,&self.fork))
            &&carrier.preparation_proof.is_some()&&carrier.broker_joined.is_some()&&carrier.projection.is_settled()
            &&carrier.panics.is_empty()&&self.slot.handles.load(Ordering::Acquire)==0,"actual preparation/broker/TLS/projection receipts absent");
        Ok(())
    }
    /// Fixed OFF-App quiescence. All source-repository readers in this launch
    /// path were original factory/preparation workers; helper admission is
    /// sealed BEFORE spawn. No runtime/link actor can have been adopted before
    /// this gate. New native ODB readers must acquire independent ledger uses.
    pub(crate) fn check_quiescent(&self,fork:&crate::managed_seed_fork::ManagedSeedForkCell,binding:&rudder_vcs_bridge::AttemptBinding,
        budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        task_time(&self.original,&self.cancelled)?;task_time(budget,cancelled)?;
        anyhow::ensure!(!self.settled&&std::ptr::eq(fork,Arc::as_ptr(&self.fork))&&fork.original_binding()==Some(binding),"unused fork/binding differs");
        let carrier=self.slot.carrier.try_lock().map_err(|_|anyhow::anyhow!("original unused carrier busy/poisoned"))?;self.check_carrier(&carrier)?;
        if let Some(ready)=&carrier.ready_owner {
            let a=&ready.prepared.assignment;
            anyhow::ensure!(binding.project_id==a.fence.owner.project_id&&binding.owner==a.fence.owner_epoch()
                &&binding.run_id==a.fence.run_id&&binding.run_epoch==a.fence.run_epoch&&binding.attempt_id==a.fence.attempt_id
                &&binding.view_id==a.observation.view_id&&binding.seed==a.seed,"unused launch assignment differs from original fork");
        }
        drop(carrier);self.slot.project.with_writer(budget,cancelled,|_|Ok(()))?;
        task_time(&self.original,&self.cancelled)?;task_time(budget,cancelled)
    }
    /// After the genuine VCS dependency ACK has been retained in the ledger.
    /// This does not remove private history, run records, checkpoints or pins.
    /// Original carrier stays intact across each cleanup error/panic.
    pub(crate) fn settle_off_app(&mut self)->Result<()> {
        if self.settled {return Ok(());}task_time(&self.original,&self.cancelled)?;
        let mut carrier=self.slot.carrier.lock().unwrap_or_else(|p|p.into_inner());self.check_carrier(&carrier)?;
        if let Some(ready)=&mut carrier.ready_owner {ready.prepared.settle_unused_retaining()?;}
        carrier.preparation.settle_unused()?;task_time(&self.original,&self.cancelled)?;
        // Handoff was NEVER activated: the actual helper/enrollment permits
        // are in this seal. Closing these local descriptor owners starts no
        // helper/actor and does not stand in for any thread join.
        carrier.ready_owner=None;carrier.request=None;carrier.packet=None;carrier.native=None;
        carrier.preparation=Default::default();carrier.scoped_fork=None;
        carrier.broker_joined=None;carrier.broker=crate::agent_broker::BrokerThreadOwner::new();
        carrier.projection=fabric_runtime::AgentToolsProjectionOwner::new();carrier.preparation_proof=None;
        self.settled=true;Ok(())
    }
    // Only the fixed JoinedSeedUse<NeverTaskClose> bridge calls this after the
    // actual cleanup worker/TLS has joined. No filesystem/Journal/cleanup IO.
    pub(crate) fn finalize_after_cleanup_join(&mut self)->Result<()> {
        if self.finalized{return Ok(());}anyhow::ensure!(self.settled,"original unused cleanup effects not settled");
        let mut carrier=self.slot.carrier.lock().unwrap_or_else(|p|p.into_inner());
        anyhow::ensure!(self.slot.generation.load(Ordering::Acquire)==self.generation&&self.slot.acceptance_pending.load(Ordering::Acquire),"original unused custody replaced");
        drop(carrier.admission.take());self.finalized=true;
        self.slot.state.store(TASK_SETTLED,Ordering::Release);self.slot.acceptance_pending.store(false,Ordering::Release);
        self.slot.work.store(true,Ordering::Release);Ok(())
    }
}
impl ManagedTaskLifetimeSupervisor {
    /// OFF-App; nonblocking typed extraction of ONE exact unused original slot.
    /// No Child/path absence, elapsed deadline, worker event or boolean issues
    /// this token. Dropped tokens keep acceptance_pending and real owners held.
    pub(crate) fn take_never_started_off_app(&self,fork:&Arc<crate::managed_seed_fork::ManagedSeedForkCell>,
        seal:&SealedSeedTaskUse,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<Option<NeverStartedManagedTask>> {
        task_time(original,&cancelled)?;seal.check_original(fork.start_gate())?;
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire)!=TASK_NEVER_STARTED {continue;}
            let Ok(carrier)=slot.carrier.try_lock() else {continue;};
            if !carrier.scoped_fork.as_ref().is_some_and(|actual|Arc::ptr_eq(actual,fork)){continue;}
            anyhow::ensure!(carrier.preparation_proof.is_some()&&carrier.broker_joined.is_some()&&carrier.projection.is_settled()
                &&carrier.panics.is_empty()&&slot.handles.load(Ordering::Acquire)==0,"actual unused joins uncertain");
            anyhow::ensure!(slot.joining.try_lock().map_err(|_|anyhow::anyhow!("actual join result busy/poisoned"))?.panics.is_empty(),"actual worker/TLS panic remains retained");
            anyhow::ensure!(carrier.unused_budget.as_ref().is_some_and(|held|held.constraint()==original.constraint()),"unused original cleanup budget changed");
            anyhow::ensure!(!slot.acceptance_pending.swap(true,Ordering::AcqRel),"original unused capsule already transferred");
            return Ok(Some(NeverStartedManagedTask {slot:slot.clone(),generation:slot.generation.load(Ordering::Acquire),fork:fork.clone(),
                seal:seal.clone(),original:original.clone(),cancelled,settled:false,finalized:false}));
        }
        Ok(None)
    }
}
