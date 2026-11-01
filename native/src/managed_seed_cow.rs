// V2 actual factory/whole-task bridge. Included inside managed_seed_fork.
use rudder_native::root_storage_client::{ClientMetadataCowOwner,ClientSeedOwner,RootStorageClient,EnrolledSeedHelper};
use rudder_vcs_bridge::seed::{SeedProfile,CowMetadataForkQuiesced,fork_private_metadata_cow_retaining};
impl ManagedSeedForkCell {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_cow(template:&Arc<VerifiedMetadataTemplate>,root:ObjectRef,source:ObjectRef,admission:Arc<SeedAdmission>,
        start:SeedTaskUseGate,cas:&Path,cache:Option<&Arc<crate::managed_preparation_cache::ManagedPreparationCache>>,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>,factory:crate::managed_machine_entry::ManagedSeedCowFactory)->Result<Arc<Self>> {
        Self::new_inner(template,root,source,admission,start,cas,cache,original,cancelled,Some(factory))
    }
    pub(crate) fn is_cow(&self)->bool{self.cow_factory.is_some()}
    /// Called only under the actual Machine/Seed/Task factory custody, in that
    /// lock order. The original fork slot owns the Root request before IO.
    pub(crate) fn stage_cow_from_actual_root(&self,client:&mut RootStorageClient,seed:&ClientSeedOwner,template:&VerifiedMetadataTemplate,
        binding:rudder_vcs_bridge::AttemptBinding,start:SeedTaskUseGate,original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        let mut held=self.lock(original,&cancelled)?;
        ensure!(held.attempted&&held.cow.is_none()&&self.original_binding()==Some(&binding),"original CoW task/fork staging differs");
        client.create_metadata_cow_retaining(&mut held.cow,seed,template,binding,start,original,cancelled)
    }

    pub(crate) fn prepared_private_path(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<PathBuf>{
        let held=self.lock(original,cancelled)?;ensure!(held.acknowledged,"original fork preparation ACK absent");
        held.private_path.clone().context("original private metadata root absent")
    }
    pub(crate) fn bind_cow_helper_retaining(&self,client:&mut RootStorageClient,seed:&mut ClientSeedOwner,
        helper:&EnrolledSeedHelper,original:&FinalizationBudget)->Result<()> {
        if !self.is_cow(){return Ok(())}let mut held=self.lock(original,&self.cancelled)?;
        client.bind_metadata_cow_helper_retaining(held.cow.as_mut().context("original CoW Root owner absent")?,seed,helper,original)
    }
    pub(crate) fn archive_cow_after_joined(&self,client:&mut RootStorageClient,seed:&ClientSeedOwner,joined:&JoinedManagedTask,
        original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        let mut held=self.lock(original,cancelled)?;ensure!(self.is_cow()&&held.acknowledged,"original CoW factory ACK absent");
        let binding=held.owner.as_ref().context("original VCS CoW owner absent")?.binding().clone();self.check_joined(joined,&binding,original,cancelled)?;
        let State{owner,quiesced,cow,..}=&mut *held;
        if quiesced.is_none()&&cow.as_ref().is_some_and(|c|c.archive().is_none()&&!c.archive_input_retained()){
            owner.as_mut().unwrap().quiesce_cow_retaining(quiesced,original,cancelled,|b|self.check_joined(joined,b,original,cancelled))?;}
        let cow=cow.as_mut().context("original Root CoW owner absent")?;
        if cow.archive_request_started(){ensure!(client.reconcile_cow_archive_fact_retaining(cow)?,"original CoW archive ACK pending");Ok(())}
        else{client.archive_metadata_cow_retaining(cow,seed,quiesced,original,cancelled)}
    }
    pub(crate) fn archive_cow_after_never_started(&self,client:&mut RootStorageClient,seed:&ClientSeedOwner,unused:&NeverStartedManagedTask,
        original:&FinalizationBudget,cancelled:&Arc<AtomicBool>,seal:&SealedSeedTaskUse)->Result<()> {
        let mut held=self.lock(original,cancelled)?;ensure!(self.is_cow(),"V2 archive requested for V1 fork");
        let binding=held.cow.as_ref().context("original CoW Root outcome absent")?.binding().clone();
        unused.check_quiescent(self,&binding,original,cancelled)?;
        let State{owner,quiesced,cow,..}=&mut *held;
        if owner.is_none(){let cow=cow.as_mut().unwrap();
            if cow.archive_request_started(){ensure!(client.reconcile_cow_archive_fact_retaining(cow)?,"original unused CoW archive ACK pending");return Ok(())}
            if !client.reconcile_cow_ready_fact_retaining(cow)?{anyhow::bail!("original CoW creation ACK pending; same owner retained")}
            return client.archive_unadopted_cow_retaining(cow,seed,seal,original,cancelled)}
        if quiesced.is_none()&&cow.as_ref().is_some_and(|c|c.archive().is_none()&&!c.archive_input_retained()){
            owner.as_mut().unwrap().quiesce_cow_retaining(quiesced,original,cancelled,|b|unused.check_quiescent(self,b,original,cancelled))?;}
        let cow=cow.as_mut().context("original Root CoW owner absent")?;
        if cow.archive_request_started(){ensure!(client.reconcile_cow_archive_fact_retaining(cow)?,"original CoW archive ACK pending");Ok(())}
        else{client.archive_metadata_cow_retaining(cow,seed,quiesced,original,cancelled)}
    }
    pub(crate) fn with_cow_owner<R>(&self,original:&FinalizationBudget,cancelled:&AtomicBool,read:impl FnOnce(&ClientMetadataCowOwner)->Result<R>)->Result<R>{
        let held=self.lock(original,cancelled)?;read(held.cow.as_ref().context("original Root CoW owner absent")?)
    }
}

impl ManagedSeedForkCell {
    pub(crate) fn prepare_cow_enrollment_retaining(&self,client:&RootStorageClient,seed:&mut ClientSeedOwner,original:&FinalizationBudget)->Result<()> {
        if !self.is_cow(){return Ok(())}let held=self.lock(original,&self.cancelled)?;
        client.prepare_metadata_cow_enrollment_retaining(held.cow.as_ref().context("original Root CoW owner absent")?,seed,original)
    }
}

impl ManagedSeedForkCell {
    /// OFF App, one original Root reply observation only. No task deadline
    /// renewal, source adoption, new request or private namespace access.
    pub(crate) fn observe_cow_outcome_off_app(&self,client:&mut RootStorageClient)->Result<bool>{
        if !self.is_cow(){return Ok(true)}
        let mut held=self.state.try_lock().map_err(|_|anyhow::anyhow!("original CoW outcome busy/poisoned; retained"))?;
        let cow=held.cow.as_mut().context("original Root CoW request not yet staged")?;
        if cow.archive_request_started(){client.reconcile_cow_archive_fact_retaining(cow)}else{client.reconcile_cow_ready_fact_retaining(cow)}
    }
}
