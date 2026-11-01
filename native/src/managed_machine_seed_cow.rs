// Fixed machine-owned Root factory adapter; no caller callback/path grants.
#[derive(Clone)]
pub(crate) struct ManagedSeedCowFactory {machine:Weak<Shared>,task:Weak<Cell<TaskScope>>,seed:Weak<Cell<SeedCarrier>>,seed_id:String,task_id:String,original:FinalizationBudget}
impl ManagedSeedCowFactory {
    fn from_actual(machine:&MachineControl,ticket:&SeedTaskTicket)->Self{Self{machine:Arc::downgrade(&machine.shared),task:Arc::downgrade(&ticket.cell),seed:ticket.seed.clone(),seed_id:ticket.seed_id.clone(),task_id:ticket.id.clone(),original:ticket.original.clone()}}
    pub(crate) fn create_retaining(&self,actual:&ManagedSeedForkCell,template:&VerifiedMetadataTemplate,binding:rudder_vcs_bridge::AttemptBinding,original:&FinalizationBudget)->Result<()> {
        ensure!(original.constraint()==self.original.constraint(),"Root CoW task clock differs");
        let shared=self.machine.upgrade().context("actual machine owner absent")?;
        let control=MachineControl{send:shared.send.clone(),shared};off_app(&control)?;
        let mut c=control.carrier_off_app(original)?;ensure!(!c.root_rpc_held&&!c.root_private_held,"original Root RPC unresolved");
        let seed=SeedTicket{id:self.seed_id.clone(),cell:self.seed.upgrade().context("actual seed owner absent")?};original_seed(&c,&seed)?;
        let s=lock(&seed.cell,original)?;s.admission.check_open()?;s.source.check_shared_admission()?;
        let task=self.task.upgrade().context("original task owner absent")?;
        ensure!(s.tasks.get(&self.task_id).is_some_and(|t|Arc::ptr_eq(&t.cell,&task)),"original task cell differs");
        let held=lock(&task,original)?;
        // This is the actual queued preparation worker, AFTER original source
        // factory/TLS join and dispatch. It may not replace the retained cell.
        ensure!(held.factory.is_joined()&&held.bound&&held.expected_run.as_deref()==Some(binding.run_id.as_str())
            &&held.fork.as_ref().is_some_and(|f|std::ptr::eq(actual,Arc::as_ptr(f))),"CoW caller is not original admitted task/fork");
        let source=s.source.prepared_current(original,&s.cancelled)?;
        ensure!(source.seed_root==binding.seed&&s.template.as_ref().is_some_and(|t|std::ptr::eq(template,Arc::as_ptr(t))),"CoW differs from original acknowledged source/template");
        let result=actual.stage_cow_from_actual_root(c.bootstrap.client_mut_retaining()?,s.client_owner.as_ref().context("original Root seed absent")?,template,binding,held.start.as_ref().context("actual CoW start gate absent")?.clone(),original,held.factory.cancellation());
        if result.is_err(){let held_private=c.bootstrap.client_mut_retaining()?.retain_original_v2_private_io_custody().is_ok();
            if held_private{c.root_private_held=true;}
            else if c.bootstrap.client_mut_retaining()?.assert_authenticated_current().is_err(){control.shared.phase.store(QUARANTINED,Ordering::Release);}result?;}
        s.source.check_shared_admission()?;original.remaining()?;Ok(())
    }
}

impl MachineControl {
    /// Explicit retained recovery observation after timeout. Only the original
    /// actual seed/task/fork cells may receive a past Root resource fact.
    pub(crate) fn observe_original_cow_outcome_off_app(&self,ticket:&SeedTaskTicket)->Result<bool>{
        off_app(self)?;
        let mut c=self.shared.carrier.try_lock().map_err(|_|anyhow::anyhow!("actual machine Root owner busy/poisoned"))?;
        let seed=task_seed(ticket)?;original_seed(&c,&seed)?;
        let s=seed.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual seed owner busy/poisoned"))?;
        s.source.assert_original_retained_custody()?;
        ensure!(s.tasks.get(&ticket.id).is_some_and(|actual|Arc::ptr_eq(&actual.cell,&ticket.cell)),"foreign original CoW task cell");
        let held=ticket.cell.owner.try_lock().map_err(|_|anyhow::anyhow!("actual CoW task busy/poisoned"))?;
        let fork=held.fork.as_ref().context("actual CoW fork cell absent")?;
        fork.observe_cow_outcome_off_app(c.bootstrap.client_mut_retaining()?)
    }
}
