// Included in managed_machine_entry. These fixed methods expose actual retained
// project services only to the connected native ingress, never paths/JSON.
impl ProjectTicket {
    pub(crate) fn ingress_ready(&self)->bool {self.cell.phase.load(Ordering::Acquire)==READY}
    pub(crate) fn ingress_recovery(&self)->bool {matches!(self.cell.phase.load(Ordering::Acquire),RECOVERY|SHARED_FENCED|STARTUP_CLOSED)}
}
impl MachineControl {
    pub(crate) fn ingress_worker_serving(&self)->bool {self.shared.worker_owned.load(Ordering::Acquire)&&self.shared.phase.load(Ordering::Acquire)==SERVING}
    fn ingress_project_current(&self,ticket:&ProjectTicket,original:&FinalizationBudget)->Result<()> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"project service access requires off App");
        loop {original.remaining()?;match self.shared.carrier.try_lock() {
            Ok(mut actual)=>{ensure!(actual.projects.get(&ticket.id).is_some_and(|p|Arc::ptr_eq(&p.cell,&ticket.cell)),"foreign actual project ticket");
                actual.bootstrap.check_current_retaining()?;ensure!(ticket.cell.phase.load(Ordering::Acquire)==READY,"actual project not ready/recovery");return Ok(());},
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual machine carrier poisoned")}}
    }
    pub(crate) fn ingress_task_services(&self,ticket:&ProjectTicket,original:&FinalizationBudget)
        ->Result<(Arc<ManagedTaskProject>,Arc<ManagedTaskLifetimeSupervisor>)> {
        self.ingress_project_current(ticket,original)?;
        loop {original.remaining()?;match ticket.cell.owner.try_lock() {
            Ok(actual)=>{actual.owner.assert_current()?;ensure!(matches!(&actual.purpose,ProjectPurpose::Task(_)),"source-only project has no model/task opt-in");let result=(actual.project.as_ref().context("actual project absent")?.clone(),
                actual.supervisor.as_ref().context("actual task supervisor absent")?.clone());original.remaining()?;return Ok(result);},
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual project carrier poisoned")}}
    }
    pub(crate) fn capture_ingress_initial_source_retaining(&self,out:&mut Option<ManagedInitialSourceOwner>,ticket:&ProjectTicket,
        store:LocalStore,original:&FinalizationBudget)->Result<()> {
        ensure!(out.is_none(),"original initial-source output already present");self.ingress_project_current(ticket,original)?;
        loop {original.remaining()?;match ticket.cell.owner.try_lock() {
            Ok(mut actual)=>{actual.owner.assert_current()?;
                *out=Some(ManagedInitialSourceOwner::capture_source_only(&mut actual.owner,store)?);original.remaining()?;return Ok(());},
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual project carrier poisoned")}}
    }
}

impl MachineControl {
    /// OFF App, actual original registered Arc from the already-latched worker.
    /// No config/JSON ID can construct this capability.
    pub(crate) fn ingress_registered_engine(&self,original:&FinalizationBudget)->Result<Arc<RegisteredEngineOwner>> {
        ensure!(self.shared.app_thread!=std::thread::current().id(),"registered owner transfer requires off App");
        loop{original.remaining()?;match self.shared.carrier.try_lock(){Ok(actual)=>{ensure!(self.shared.worker_owned.load(Ordering::Acquire)&&self.shared.phase.load(Ordering::Acquire)==SERVING,"actual Root worker not serving");let owner=actual.registered_engine.as_ref().context("original registered engine pending")?.clone();owner.assert_current()?;original.remaining()?;return Ok(owner);},Err(TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),Err(_)=>anyhow::bail!("actual Root worker carrier poisoned")}}
    }
}
