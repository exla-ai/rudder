// Included in managed_machine_seed. One fixed native issuer; no bool/JSON/Any
// callback can claim that an original task/helper stopped.
struct NeverStartedScope {
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,seal:Option<SealedSeedTaskUse>,
    unused:Option<NeverStartedManagedTask>,dependency:Option<RetainedUnusedFork>,dependency_attempted:bool,
    fork_value:Option<NeverForkClose>,fork_close:Option<RetainedSeedJoin<NeverForkClose>>,fork_started:bool,
    task_value:Option<NeverTaskClose>,task_close:Option<RetainedSeedJoin<NeverTaskClose>>,task_started:bool,
}
struct NeverForkClose {_cell:Option<Arc<ManagedSeedForkCell>>,proof:RetainedUnusedFork,original:FinalizationBudget,cancelled:Arc<AtomicBool>}
fn close_never_fork(value:&mut NeverForkClose)->Result<()> {value.proof.check_current(&value.original,&value.cancelled)}
enum NeverTaskPayload {Factory {prepared:Option<PreparedManagedTask>,packet:Option<crate::managed_task_launch::ManagedTaskLaunchPacket>,_error:Option<anyhow::Error>},App(NeverStartedManagedTask)}
struct NeverTaskClose {payload:NeverTaskPayload,_proof:RetainedUnusedFork,original:FinalizationBudget,cancelled:Arc<AtomicBool>}
fn close_never_task(value:&mut NeverTaskClose)->Result<()> {
    time(&value.original,&value.cancelled)?;
    match &mut value.payload {
        NeverTaskPayload::Factory {..}=>{}, // all original inputs/admission survive this worker's actual TLS
        NeverTaskPayload::App(unused)=>unused.settle_off_app()?,
    }
    time(&value.original,&value.cancelled)
}
fn finish_never_task_after_join(value:&mut NeverTaskClose)->Result<()> {
    match &mut value.payload {
        NeverTaskPayload::Factory {prepared,packet,..}=>{drop(prepared.take());drop(packet.take());Ok(())},
        NeverTaskPayload::App(unused)=>unused.finalize_after_cleanup_join(),
    }
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(crate) enum NeverStartedProgress {WaitingPreparation,Joined}
impl SeedTaskTicket {
    /// OFF-App ORIGINAL factory thread, before it returns. Store the WHOLE
    /// actual success or failure, not a flag saying no helper exists. Errors
    /// and failed result delivery stay in this original machine-owned cell.
    /// Old external/untracked factory-result callers cannot use this path.
    pub(crate) fn retain_original_factory_outcome(&self,outcome:&mut Option<Result<PreparedManagedTask>>)->Result<()> {
        let mut held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("original result cell busy/poisoned; same outcome remains with caller"))?;
        held.factory.check_original_result_thread()?;
        ensure!(held.factory_outcome.is_none()&&held.completion.is_none()&&held.never.is_none(),"original factory outcome already transferred");
        let actual=outcome.as_ref().context("whole original factory outcome absent")?;
        if let Ok(prepared)=actual {ensure!(held.bound&&held.fork.as_ref().is_some_and(|fork|prepared.has_seed_fork(fork))
            &&held.expected_run.as_deref()==Some(prepared.target_run_id()),"original prepared result differs from source scope");}
        held.factory_outcome=outcome.take();Ok(())
    }
    /// OFF-App pure transfer of an actual unqueued App refusal. Original
    /// factory/TLS must already be joined; any dispatched/Retained result is
    /// refused intact. No record/path read can replace the SAME cell Arc.
    pub(crate) fn retain_undispatched_refusal(&self,refusal:&mut Option<ManagedTaskStartFailure>)->Result<()> {
        ensure!(self.app_thread!=std::thread::current().id(),"refusal retention requires off App");
        let mut held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("original refusal cell busy/poisoned"))?;
        ensure!(held.factory.is_joined()&&held.factory_outcome.is_none()&&held.factory_refusal.is_none()&&held.never.is_none(),"original factory result still present/unjoined or cleanup started");
        let fork=held.fork.as_ref().context("original factory source cell absent")?;
        match refusal.as_ref().context("whole original refusal absent")? {
            ManagedTaskStartFailure::Prepared(actual,_)=>ensure!(actual.has_seed_fork(fork)&&held.expected_run.as_deref()==Some(actual.target_run_id())
                &&actual.original_budget().constraint()==self.original.constraint(),"refused prepared task differs from original scope"),
            ManagedTaskStartFailure::Packet(actual,_)=>ensure!(actual.seed_fork.as_ref().is_some_and(|cell|Arc::ptr_eq(cell,fork))
                &&held.expected_run.as_deref()==Some(actual.target_run_id())&&actual.budget.constraint()==self.original.constraint(),"refused packet differs from original scope"),
            ManagedTaskStartFailure::Retained(_,_)=>anyhow::bail!("queued task must retain its actual App preparation capsule"),
        }
        // Ownership transfer only, not never-started authority; the original
        // dispatch/helper/enrollment permits are consumed into a seal later.
        held.factory_refusal=refusal.take();Ok(())
    }
    /// OFF-App observation of a real cached cleanup/TLS join only. This can
    /// finish bookkeeping after expiry; no paths, Root RPC or new work runs.
    pub(crate) fn observe_never_started_cleanup_off_app(&self)->Result<bool> {
        ensure!(self.app_thread!=std::thread::current().id(),"cleanup observation requires off App");
        let held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("original cleanup cell busy/poisoned"))?;
        let actual=held.never.as_ref().and_then(|n|n.task_close.as_ref()).context("original unused cleanup not staged")?;
        let Some(joined)=actual.reconcile_joined()? else {return Ok(false);};
        joined.with_value_mut(finish_never_task_after_join)?;Ok(true)
    }
    /// App-pure transfer of the actual successful result only AFTER genuine
    /// original factory/TLS join. An error result remains in the same cell.
    pub(crate) fn take_original_prepared_on_app(&self)->Result<Option<PreparedManagedTask>> {
        ensure!(self.app_thread==std::thread::current().id(),"original prepared transfer requires App");
        let mut held=self.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("original factory result busy/poisoned"))?;
        ensure!(held.factory.is_joined()&&held.start.as_ref().is_some_and(|gate|gate.original_seal().is_none())&&held.never.is_none(),"original factory/TLS unjoined or task closed");
        if !matches!(held.factory_outcome.as_ref(),Some(Ok(_))){return Ok(None);}
        match held.factory_outcome.take().unwrap(){Ok(prepared)=>Ok(Some(prepared)),Err(_)=>unreachable!()}
    }
}
impl MachineControl {
    /// Explicit OFF-App cleanup of one exact admitted task. This NEVER closes
    /// sibling/seed admission, retries a helper or manufactures Root ACKs.
    /// Original factory must first be joined without machine/source locks.
    pub(crate) fn settle_never_started_task_off_app(&self,seed:&SeedTicket,ticket:&SeedTaskTicket,
        supervisor:Option<&Arc<ManagedTaskLifetimeSupervisor>>,original:&FinalizationBudget,
        cancelled:Arc<AtomicBool>,reconcile:bool)->Result<NeverStartedProgress> {
        let mut c=self.carrier_off_app(original)?;original_seed(&c,seed)?;
        let s=lock(&seed.cell,original)?;
        ensure!(s.tasks.get(&ticket.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&ticket.cell))&&ticket.seed_id==seed.id,"foreign actual task scope");
        let mut task=lock(&ticket.cell,original)?;
        ensure!(task.factory.is_joined()&&task.completion.is_none()&&task.enrollment.is_none(),"original factory/TLS unjoined, completion or helper enrollment exists");
        if let Some(work)=task.never.as_ref().and_then(|n|n.task_close.as_ref()) {
            if let Some(joined)=work.reconcile_joined()? {joined.with_value_mut(finish_never_task_after_join)?;return Ok(NeverStartedProgress::Joined);}
        }
        if task.never.is_none() {task.never=Some(NeverStartedScope {original:original.clone(),cancelled:cancelled.clone(),seal:None,
            unused:None,dependency:None,dependency_attempted:false,fork_value:None,fork_close:None,fork_started:false,
            task_value:None,task_close:None,task_started:false});}
        let gate=task.start.as_ref().context("original task start permits absent; legacy result remains retained")?.clone();
        let TaskScope {never,fork,factory_outcome,factory_refusal,..}=&mut *task;
        let n=never.as_mut().unwrap();
        ensure!(n.original.constraint()==original.constraint()&&Arc::ptr_eq(&n.cancelled,&cancelled),"never-started original cleanup clock/cancellation changed");
        gate.seal_retaining(&mut n.seal,&n.original,&n.cancelled)?;
        ticket.cell.phase.store(CLOSING,Ordering::Release);
        let seal=n.seal.as_ref().unwrap().clone();
        if seal.before_dispatch() {
            ensure!(factory_outcome.is_some()||factory_refusal.is_some()||n.task_value.is_some()||n.task_close.is_some(),"whole original factory outcome not retained; no absence inference");
        } else if n.unused.is_none()&&n.task_value.is_none()&&n.task_close.is_none() {
            let fork=fork.as_ref().context("original dispatched fork cell absent")?.clone();
            let supervisor=supervisor.context("original preparation supervisor required")?;
            // Only the original slot can issue this capsule, after original
            // preparation TLS and actual broker/projection joins. Pending App
            // results must still be polled/rejected by App; no off-App PTY work.
            let capsule=supervisor.take_never_started_off_app(&fork,&seal,original,cancelled.clone())?;
            if capsule.is_none(){supervisor.poll();return Ok(NeverStartedProgress::WaitingPreparation);}
            n.unused=capsule;
        }
        drop(task);
        let client=c.bootstrap.client_mut_retaining()?;client.assert_authenticated_current()?;
        close_never_started(client,s.client_owner.as_ref().context("actual Root owner absent")?,ticket,original,&cancelled,reconcile)?;
        Ok(NeverStartedProgress::Joined)
    }
}
fn close_never_started(client:&mut rudder_native::root_storage_client::RootStorageClient,seed:&ClientSeedOwner,ticket:&SeedTaskTicket,
    outer:&FinalizationBudget,_cancelled:&Arc<AtomicBool>,reconcile:bool)->Result<()> {
    outer.remaining()?;let mut task=lock(&ticket.cell,outer)?;
    ensure!(task.factory.is_joined(),"original source factory/TLS not joined");
    let TaskScope {never,guard,fork_guard,fork,factory_outcome,factory_refusal,..}=&mut *task;
    let n=never.as_mut().context("original never-started campaign absent")?;
    if let Some(work)=&n.task_close {if let Some(joined)=work.reconcile_joined()? {
        joined.with_value_mut(finish_never_task_after_join)?;outer.remaining()?;return Ok(());
    }}
    let original=n.original.clone();let cancelled=n.cancelled.clone();time(&original,&cancelled)?;
    let seal=n.seal.as_ref().context("original unspent helper/enrollment permits not sealed")?;
    if n.dependency.is_none() {
        if !seal.fork_was_never_claimed() {
            let cell=fork.as_ref().context("actual original fork owner absent")?;
            let capsule=n.unused.as_ref().context("actual unused App capsule absent")?;
            if n.dependency_attempted {ensure!(reconcile,"original VCS release unresolved; explicit reconciliation required");}
            let resume=n.dependency_attempted;n.dependency_attempted=true;
            cell.release_after_never_started(capsule,&original,&cancelled,resume)?;
            let binding=cell.original_binding().context("original fork binding absent")?;
            guard.as_ref().context("original Task guard absent")?.bind_original_attempt(binding)?;
            fork_guard.as_ref().context("original Fork guard absent")?.bind_original_attempt(binding)?;
            cell.with_unused_dependency(&original,&cancelled,|localized,never_linked|
                client.acknowledge_never_started_fork_retaining(seed,&mut n.dependency,guard.as_ref().unwrap(),fork_guard.as_ref().unwrap(),
                    seal,localized,never_linked,&original,&cancelled))?;
        } else {
            // Unspent ORIGINAL fork-creation permit is the positive proof. No
            // missing pathname/owner/Child is inspected or converted to proof.
            let (mut localized,mut never_linked)=(None,None);
            client.acknowledge_never_started_fork_retaining(seed,&mut n.dependency,guard.as_ref().context("original Task guard absent")?,
                fork_guard.as_ref().context("original Fork guard absent")?,seal,&mut localized,&mut never_linked,&original,&cancelled)?;
        }
    }
    if n.fork_close.is_none() {
        if n.fork_value.is_none(){n.fork_value=Some(NeverForkClose {_cell:fork.clone(),proof:n.dependency.as_ref().unwrap().clone(),original:original.clone(),cancelled:cancelled.clone()});}
        RetainedSeedJoin::stage_retaining(&mut n.fork_close,fork_guard,&mut n.fork_value,&original)?;
    }
    if !n.fork_started {n.fork_close.as_mut().unwrap().start_retaining(close_never_fork)?;n.fork_started=true;}
    n.fork_close.as_mut().unwrap().await_joined()?;
    if n.task_close.is_none() {
      if n.task_value.is_none(){
        let payload=if seal.before_dispatch() {
            if let Some(outcome)=factory_outcome.take() {match outcome {
                Ok(prepared)=>NeverTaskPayload::Factory {prepared:Some(prepared),packet:None,_error:None},
                Err(error)=>NeverTaskPayload::Factory {prepared:None,packet:None,_error:Some(error)},
            }} else {match factory_refusal.take().context("whole original factory outcome/refusal absent")? {
                ManagedTaskStartFailure::Prepared(prepared,error)=>NeverTaskPayload::Factory {prepared:Some(prepared),packet:None,_error:Some(error)},
                ManagedTaskStartFailure::Packet(packet,error)=>NeverTaskPayload::Factory {prepared:None,packet:Some(packet),_error:Some(error)},
                ManagedTaskStartFailure::Retained(handle,error)=>{
                    *factory_refusal=Some(ManagedTaskStartFailure::Retained(handle,error));anyhow::bail!("queued ownership requires actual App capsule");
                }
            }}
        } else {NeverTaskPayload::App(n.unused.take().context("whole original unused App capsule absent")?)};
        // Store WHOLE actual payload before stage/spawn; failures retain it.
        n.task_value=Some(NeverTaskClose {payload,_proof:n.dependency.as_ref().unwrap().clone(),original:original.clone(),cancelled:cancelled.clone()});
      }
        RetainedSeedJoin::stage_retaining(&mut n.task_close,guard,&mut n.task_value,&original)?;
    }
    if !n.task_started {n.task_close.as_mut().unwrap().start_retaining(close_never_task)?;n.task_started=true;}
    n.task_close.as_mut().unwrap().await_joined()?.with_value_mut(finish_never_task_after_join)?;outer.remaining()?;Ok(())
}
