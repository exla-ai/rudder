//! Actual finite machine cache/index/environment worker. One original30 is
//! captured before its queue, and original JoinHandle/TLS is retained. No
//! source/project/SDK/lease authority comes from this immutable input owner.
use super::*;
use crate::managed_machine_cas::ManagedMachineCas;
use super::backed_environment::{ManagedRuntimeEnvironmentFactory,ManagedRuntimeBackedEnvironment};
use rudder_native::root_storage_bootstrap::runtime_image::{RuntimeImageClient,environment::RuntimeEnvironmentBacking};
use rudder_engine::{build::{BuildIndex,index_acquisition::RetainedIndexAcquisition},
    execution::handoff::{OwnedWorkerJoin,WorkerJoined},store::{ImageImportFence,backed_retention::BackedPinOwner}};
use std::{any::Any,sync::{Mutex,OnceLock},thread,time::Duration};
struct Restriction {machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for Restriction {fn remaining(&self)->Result<Duration>{clock(&self.original,&self.cancelled)?;
    self.client.assert_environment_scope(self.machine.config(),self.machine.registration())?;self.machine.assert_custody()?;self.original.remaining()}}
struct Owner {machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    index_open:Option<RetainedIndexAcquisition>,index:Option<Arc<BuildIndex>>,factory:Option<ManagedRuntimeEnvironmentFactory>,attempted:bool,ready:bool,
    error:Option<anyhow::Error>,panic:Option<Box<dyn Any+Send>>}
struct OriginalJoined {_join:WorkerJoined,_budget:FinalizationBudget,backing:Arc<RuntimeEnvironmentBacking>,root:ObjectRef,retention:Arc<BackedPinOwner>}
#[derive(Debug)]struct EnvironmentBusy;
impl std::fmt::Display for EnvironmentBusy{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("original runtime environment temporarily borrowed")}}
impl std::error::Error for EnvironmentBusy{}
pub(crate) fn is_busy(error:&anyhow::Error)->bool{error.downcast_ref::<EnvironmentBusy>().is_some()||error.downcast_ref::<rudder_native::root_storage_bootstrap::runtime_image::environment::RuntimeBackingBusy>().is_some()}
fn borrowed<'a>(owner:&'a Mutex<Owner>)->Result<std::sync::MutexGuard<'a,Owner>>{match owner.try_lock(){Ok(g)=>Ok(g),Err(std::sync::TryLockError::WouldBlock)=>Err(EnvironmentBusy.into()),Err(_)=>anyhow::bail!("original runtime environment custody poisoned")}}
pub(crate) struct SharedRuntimeEnvironment {machine:Arc<ManagedMachineCas>,owner:Mutex<Owner>,worker:Mutex<Option<OwnedWorkerJoin>>,owned:AtomicBool,
    qualified:OnceLock<OriginalJoined>,late:Mutex<Option<WorkerJoined>>}
impl SharedRuntimeEnvironment {
    pub(crate) fn capture(machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Arc<Self>{
        Arc::new(Self{machine:machine.clone(),owner:Mutex::new(Owner{machine,client,original,cancelled,index_open:None,index:None,factory:None,attempted:false,ready:false,error:None,panic:None}),
            worker:Mutex::new(None),owned:AtomicBool::new(false),qualified:OnceLock::new(),late:Mutex::new(None)})}
    pub(crate) fn start_retaining(self:&Arc<Self>)->Result<()> {
        let mut slot=self.worker.try_lock().map_err(|_|anyhow::anyhow!("original environment worker registration busy"))?;
        ensure!(slot.is_none()&&!self.owned.load(Ordering::Acquire),"original environment worker already registered");let shared=self.clone();
        let worker=thread::Builder::new().name("rudder-runtime-backed-environment".into()).spawn(move||{
            while !shared.owned.load(Ordering::Acquire){thread::yield_now();}let mut owner=shared.owner.lock().unwrap_or_else(|p|p.into_inner());
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||owner.run())){Ok(Ok(()))=>{},Ok(Err(e))=>owner.error=Some(e),Err(p)=>owner.panic=Some(p)}
        })?;*slot=Some(OwnedWorkerJoin::new(worker));self.owned.store(true,Ordering::Release);Ok(())
    }
    pub(crate) fn poll_ready_retaining(&self)->Result<bool>{
        if self.qualified.get().is_some(){return Ok(true)}let worker=match self.worker.try_lock(){Ok(w)=>w,Err(std::sync::TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("original environment worker custody poisoned")};
        if self.qualified.get().is_some(){return Ok(true)}
        let worker=worker.as_ref().context("actual original environment worker absent")?;worker.start_join_observer()?;
        let Some(joined)=worker.try_joined()?else{return Ok(false)};
        *self.late.lock().unwrap_or_else(|p|p.into_inner())=Some(joined);
        let owner=match self.owner.try_lock(){Ok(o)=>o,Err(std::sync::TryLockError::WouldBlock)=>return Ok(false),Err(_)=>anyhow::bail!("original environment result poisoned")};
        let known=clock(&owner.original,&owner.cancelled).is_ok()&&owner.ready&&owner.error.is_none()&&owner.panic.is_none();
        if !known{anyhow::bail!("original environment failed/late; retain all owners and actual join")}
        let env=owner.factory.as_ref().unwrap().environment()?;env.assert_custody(&owner.original,&owner.cancelled)?;
        let backing=env.backing().clone();let root=env.preparation_ack()?.prepared().root.clone();let retention=env.preparation_ack()?.retention().clone();clock(&owner.original,&owner.cancelled)?;
        let joined=self.late.lock().unwrap_or_else(|p|p.into_inner()).take().context("actual original environment join absent")?;
        if let Err(held)=self.qualified.set(OriginalJoined{_join:joined,_budget:owner.original.clone(),backing,root,retention}){*self.late.lock().unwrap_or_else(|p|p.into_inner())=Some(held._join);anyhow::bail!("original environment join already adopted")};Ok(true)
    }
    fn check_owner<'a>(&self,owner:&'a Owner)->Result<&'a ManagedRuntimeBackedEnvironment>{
        ensure!(self.qualified.get().is_some()&&owner.ready&&owner.error.is_none()&&owner.panic.is_none(),"genuine original environment/TLS ACK absent");
        let env=owner.factory.as_ref().context("original environment factory absent")?.environment()?;env.assert_original_custody()?;Ok(env)
    }
    pub(crate) fn assert_original_custody(&self)->Result<()>{let held=borrowed(&self.owner)?;self.check_owner(&held)?;Ok(())}
    pub(crate) fn machine(&self)->Arc<ManagedMachineCas>{self.machine.clone()}
    pub(crate) fn compiler_sdk_client(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<Arc<rudder_native::root_storage_bootstrap::runtime_image::CompilerSdkClient>>{
        loop{clock(original,cancel)?;match borrowed(&self.owner){Ok(held)=>{self.check_owner(&held)?;held.client.assert_environment_scope(self.machine.config(),self.machine.registration())?;
            let sdk=held.client.compiler_sdk_client()?.context("fixed source initialization needs actual original SDK client")?;sdk.current(original)?;sdk.require_ordinary_execution()?;clock(original,cancel)?;return Ok(sdk)},
            Err(e)if is_busy(&e)=>thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(e)=>return Err(e)}}
    }
    // Immutable original-ACK aliases only. Callers check current custody under
    // their actual phase clock before/after effects; these getters never turn
    // a transient owner borrow into a failed source/preparation attempt.
    pub(crate) fn backing(&self)->Result<Arc<RuntimeEnvironmentBacking>>{Ok(self.qualified.get().context("genuine original environment/TLS ACK absent")?.backing.clone())}
    pub(crate) fn root(&self)->Result<ObjectRef>{Ok(self.qualified.get().context("genuine original environment/TLS ACK absent")?.root.clone())}
    pub(crate) fn retention(&self)->Result<Arc<BackedPinOwner>>{Ok(self.qualified.get().context("genuine original environment/TLS ACK absent")?.retention.clone())}
    pub(crate) fn derive_environment_subgraph_retaining(&self,out:&mut Option<Arc<rudder_engine::store::environment_dependencies::RetainedEnvironmentSubgraph>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        loop{original.remaining()?;match borrowed(&self.owner){Ok(held)=>return self.check_owner(&held)?.derive_environment_subgraph_retaining(out,original),
            Err(e)if is_busy(&e)=>thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(e)=>return Err(e)}}
    }
    pub(crate) fn network_mounts(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<Vec<ReadOnlyRuntime>>{loop{clock(original,cancel)?;match borrowed(&self.owner){
        Ok(held)=>{let mounts=self.check_owner(&held)?.network_mounts()?;clock(original,cancel)?;return Ok(mounts)},Err(e)if is_busy(&e)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(e)=>return Err(e)}}}
    pub(crate) fn current<B:RuntimeReadBudget+?Sized>(&self,original:&B,cancel:&AtomicBool)->Result<()>{loop{clock(original,cancel)?;match self.assert_original_custody(){Ok(())=>return clock(original,cancel),Err(e)if is_busy(&e)=>thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(e)=>return Err(e)}}}
}
impl Owner {fn run(&mut self)->Result<()> {
    ensure!(!self.attempted,"original machine environment already attempted");self.attempted=true;
    let fence=Arc::new(Restriction{machine:self.machine.clone(),client:self.client.clone(),original:self.original.clone(),cancelled:self.cancelled.clone()});fence.remaining()?;
    let root=self.machine.config().data().state_directory.join("build-index-v1");
    BuildIndex::create_shared_retaining(&mut self.index_open,root,self.machine.store()?.clone(),self.original.clone(),self.cancelled.clone(),fence.clone())?;
    self.index=Some(self.index_open.as_ref().unwrap().index()?.clone());
    ManagedHostRuntime::capture_runtime_backed_environment_retaining(&mut self.factory,self.machine.clone(),self.client.clone(),self.index.as_ref().unwrap().clone(),self.original.clone(),self.cancelled.clone())?;
    loop{fence.remaining()?;if self.factory.as_mut().unwrap().poll_retaining()?{break;}thread::sleep(self.original.remaining()?.min(Duration::from_millis(2)));}
    self.factory.as_ref().unwrap().environment()?.assert_custody(&self.original,&self.cancelled)?;fence.remaining()?;self.ready=true;Ok(())
}}
