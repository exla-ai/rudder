//! Child of the actual accepted-source owner. This is retained runtime/source
//! DATA custody, not a Root mount, SDK, lease renewal or task-admission grant.
use super::*;
use rudder_engine::store::{ImageImportFence,environment_dependencies::{RetainedRunnableInputs,RetainedRunnableRead}};
use crate::managed_machine_runtime_profile::runtime_environment::SharedRuntimeEnvironment;
use std::{collections::BTreeMap,sync::{Mutex,TryLockError},time::Duration};
struct ReadClock{original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for ReadClock{fn remaining(&self)->Result<Duration>{time(&self.original,&self.cancelled)?;self.original.remaining()}}
struct State{cas:Option<DirectoryCustody>,project:Option<DirectoryCustody>,pending:Option<RetainedRunnableRead>,metadata:BTreeMap<ObjectRef,Vec<u8>>,ready:bool}
pub(crate) struct ManagedBackedTaskInputs{fresh:Arc<crate::managed_fresh_genesis::ManagedFreshGenesis>,lease:EngineSourceLease,store:LocalStore,
    runtime:Arc<SharedRuntimeEnvironment>,runtime_backing:Arc<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeEnvironmentBacking>,runnable:ObjectRef,source:ObjectRef,environment:ObjectRef,pin:String,retention:RetainedRunnableInputs,
    source_parent:Option<Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>,
    _verified:Arc<rudder_vcs_bridge::projection::VerifiedSourceProjection>,state:Mutex<State>}
impl ManagedBackedTaskInputs{
    pub(super) fn capture_retaining(out:&mut Option<Arc<Self>>,source:&ManagedInitialSourceOwner,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original backed task inputs occupied");let prepared=source.prepared_current(original,cancelled)?;
        let Some(retention)=prepared.runnable_inputs()else{return Ok(())};
        let fresh=source.fresh.as_ref().context("backed task needs its original fresh namespace/authority capsule")?.clone();
        let runtime=fresh.environment()?.runtime_backing().context("backed task original prepared runtime absent")?.clone();
        *out=Some(Arc::new(Self{fresh,lease:source.lease.clone(),store:source.store.clone(),runtime:runtime.clone(),runtime_backing:runtime.backing()?,runnable:prepared.runnable_root.clone(),source:prepared.source_root.clone(),
            environment:runtime.root()?,pin:prepared.runnable_pin.clone(),retention:retention.clone(),source_parent:prepared.source_parent().cloned(),_verified:prepared.verified.clone(),
            state:Mutex::new(State{cas:None,project:None,pending:None,metadata:BTreeMap::new(),ready:false})}));
        let owner=out.as_ref().unwrap();let mut state=owner.state.lock().unwrap_or_else(|p|p.into_inner());
        state.cas=Some(source.cas_anchor.clone_held()?);state.project=Some(source.project_anchor.clone_held()?);
        source.prepared_current(original,cancelled)?;state.ready=true;drop(state);owner.check_retained()?;time(original,cancelled)
    }
    fn state(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<std::sync::MutexGuard<'_,State>>{loop{time(original,cancelled)?;match self.state.try_lock(){
        Ok(state)=>return Ok(state),Err(TryLockError::WouldBlock)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(_)=>anyhow::bail!("original backed task inputs poisoned")}}}
    fn physical(&self,state:&State)->Result<()> {ensure!(state.ready,"original backed task input capture incomplete");self.lease.check_current()?;self.fresh.assert_assigned_retention()?;
        state.cas.as_ref().context("original CAS directory absent")?.check()?;state.project.as_ref().context("original namespace directory absent")?.check()?;
        self.runtime.assert_original_custody().map_err(|e|if crate::managed_machine_runtime_profile::runtime_environment::is_busy(&e){crate::engine_host::EngineSharedAdmissionBusy.into()}else{e})?;ensure!(self.runtime.root()?==self.environment&&self.retention.root_data()==&self.runnable,"original backed task roots changed");
        ensure!(self.retention.matches_record_data(&self.store.retention()?,&self.pin),"actual backed source pin changed");
        if matches!(&self.retention,RetainedRunnableInputs::Environment(_)){self.source_parent()?;}Ok(())}
    /// Old task reads/cleanup do not require that this snapshot remain the
    /// latest HEAD. New task admission separately checks the actual Journal.
    pub(crate) fn check_retained(&self)->Result<()> {let state=match self.state.try_lock(){Ok(s)=>s,Err(TryLockError::WouldBlock)=>return Err(crate::engine_host::EngineSharedAdmissionBusy.into()),Err(_)=>anyhow::bail!("backed task custody poisoned")};self.physical(&state)}
    pub(crate) fn check_for_new_task(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {time(original,cancelled)?;self.lease.wait_shared_current(original,cancelled)?;let state=self.state(original,cancelled)?;self.physical(&state)?;time(original,cancelled)}
    pub(crate) fn matches(&self,store:&LocalStore,runnable:&ObjectRef,source:&ObjectRef)->Result<()> {ensure!(store.root()==self.store.root()&&runnable==&self.runnable&&source==&self.source,"task backing changed original CAS/runnable/source");Ok(())}
    pub(crate) fn metadata(&self,reference:&ObjectRef,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<Vec<u8>> {
        time(original,cancelled)?;ensure!(reference.size_bytes<=64*1024&&matches!(reference.kind,rudder_engine::protocol::ObjectKind::RunnableRoot|rudder_engine::protocol::ObjectKind::SourceRoot|rudder_engine::protocol::ObjectKind::EnvironmentRoot|rudder_engine::protocol::ObjectKind::Blob),"backed task metadata bound/kind");
        let mut state=self.state(original,cancelled)?;self.physical(&state)?;
        if let Some(raw)=state.metadata.get(reference){reference.verify(raw)?;time(original,cancelled)?;return Ok(raw.clone())}
        ensure!(state.pending.is_none()&&state.metadata.len()<64,"original failed metadata read/work bound; no replacement");
        self.retention.read_retaining(&mut state.pending,reference.clone(),Arc::new(ReadClock{original:original.clone(),cancelled:cancelled.clone()}))?;
        let raw=state.pending.as_ref().unwrap().verified_bytes()?.to_vec();self.physical(&state)?;time(original,cancelled)?;
        state.metadata.insert(reference.clone(),raw.clone());state.pending.take();Ok(raw)
    }
    pub(crate) fn check_reads_settled(&self)->Result<()> {let state=self.state.try_lock().map_err(|_|anyhow::anyhow!("actual backed task read still borrowed/poisoned"))?;ensure!(state.pending.is_none(),"actual backed task read retained after failure/unknown; no settlement");Ok(())}
    pub(crate) fn runtime(&self)->&Arc<SharedRuntimeEnvironment>{&self.runtime}
    pub(crate) fn runtime_backing(&self)->&Arc<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeEnvironmentBacking>{&self.runtime_backing}
    pub(crate) fn verified_source(&self)->&Arc<rudder_vcs_bridge::projection::VerifiedSourceProjection>{&self._verified}
    pub(crate) fn source_parent(&self)->Result<&Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>> {
        let parent=self.source_parent.as_ref().context("actual flat source parent absent")?;
        let RetainedRunnableInputs::Environment(owner)=&self.retention else{anyhow::bail!("source parent requires the original flat Environment-dependent source")};
        let binding=parent.binding_data()?;
        ensure!(Arc::ptr_eq(parent.original_owner(),owner)&&binding.runnable==self.runnable&&binding.source==self.source
            &&owner.environment().root_data()?==&self.environment,"task source parent differs from original source/Environment owner");Ok(parent)
    }
    pub(crate) fn retention(&self)->&RetainedRunnableInputs{&self.retention}
    pub(crate) fn environment(&self)->&ObjectRef{&self.environment}
    pub(crate) fn store(&self)->&LocalStore{&self.store}
}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();}original.remaining()?;Ok(())}
