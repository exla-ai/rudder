//! Explicit actual backed Environment/RunnableRoot retention for the existing
//! initial jj/source producer. No Root/project/task authority from these DATA
//! readers; Native retains its genuine environment/factory/namespace owners.
use super::*;
use rudder_engine::store::{ImageImportFence,backed_inputs::VerifiedBakedInputs,
    backed_retention::{BackedPinOwner,BackedPublication,BackedRead},environment_dependencies::{RetainedEnvironmentSubgraph,EnvironmentSubgraphRead,EnvironmentDependentPublication,EnvironmentDependentPinOwner}};
use std::sync::atomic::{AtomicBool,Ordering};
pub(super) struct Clock {pub(super) original:FinalizationBudget,pub(super) cancelled:Arc<AtomicBool>}
impl ImageImportFence for Clock{fn remaining(&self)->Result<Duration>{if self.cancelled.load(Ordering::Acquire){self.original.cancel();}self.original.remaining()}}
/// Staged original read/publication cell. Non-Serde/non-Clone, no readback ACK
/// recovery. A failed read/pin/late ACK stays in this whole original carrier.
pub struct BackedInitialEnvironment {environment:ObjectRef,owner:Option<Arc<BackedPinOwner>>,subgraph:Option<Arc<RetainedEnvironmentSubgraph>>,sources:Vec<Arc<VerifiedBakedInputs>>,
    pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>,clock:Arc<Clock>,read:Option<BackedRead>,publication:Option<BackedPublication>,
    environment_read:Option<EnvironmentSubgraphRead>,dependent:Option<EnvironmentDependentPublication>,
    source_parent:Option<Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>,
    runnable:Option<ObjectRef>,environment_attempted:bool,environment_checked:bool}
impl BackedInitialEnvironment {
    pub fn stage_retaining(out:&mut Option<Self>,environment:ObjectRef,owner:Arc<BackedPinOwner>,sources:Vec<Arc<VerifiedBakedInputs>>,
        pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original backed initial environment occupied");let clock=Arc::new(Clock{original:original.clone(),cancelled:cancelled.clone()});
        *out=Some(Self{environment,owner:Some(owner),subgraph:None,sources,pin,original,cancelled,clock,read:None,publication:None,environment_read:None,dependent:None,source_parent:None,runnable:None,environment_attempted:false,environment_checked:false});Ok(())
    }
    pub fn stage_subgraph_retaining(out:&mut Option<Self>,subgraph:Arc<RetainedEnvironmentSubgraph>,pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original dependent initial environment occupied");let environment=subgraph.root_data()?.clone();let clock=Arc::new(Clock{original:original.clone(),cancelled:cancelled.clone()});
        *out=Some(Self{environment,owner:None,subgraph:Some(subgraph),sources:Vec::new(),pin,original,cancelled,clock,read:None,publication:None,environment_read:None,dependent:None,source_parent:None,runnable:None,environment_attempted:false,environment_checked:false});Ok(())
    }
    fn same_request(&self,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        ensure!(original.constraint()==self.original.constraint()&&Arc::ptr_eq(cancelled,&self.cancelled),"backed initial source replaced original clock/cancellation");self.clock.remaining()?;Ok(())
    }
    pub(super) fn check_environment_retaining(&mut self,environment:&ObjectRef,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        self.same_request(original,cancelled)?;ensure!(!self.environment_attempted&&environment==&self.environment,"original backed environment attempted/different root");
        self.environment_attempted=true;ensure!(environment.kind==ObjectKind::EnvironmentRoot,"backed initial environment kind differs");
        if let Some(subgraph)=&self.subgraph{subgraph.read_retaining(&mut self.environment_read,environment.clone(),self.clock.clone())?;
            decode_environment(self.environment_read.as_ref().unwrap().verified_bytes()?)?;
        }else{ensure!(!self.sources.is_empty()&&self.sources.len()<=4,"actual initial backing owners absent");
            self.owner.as_ref().context("original flat parent absent")?.read_retaining(&mut self.read,environment.clone(),self.clock.clone())?;
            decode_environment(self.read.as_ref().unwrap().verified_bytes()?)?;}
        self.same_request(original,cancelled)?;self.read.take();self.environment_read.take();self.environment_checked=true;Ok(())
    }
    pub(super) fn publish_runnable_retaining(&mut self,store:&LocalStore,stage:&str,runnable:&ObjectRef,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        self.same_request(original,cancelled)?;ensure!(self.environment_checked&&self.publication.is_none()&&self.runnable.is_none(),"backed initial environment ACK absent/original publication attempted");
        self.runnable=Some(runnable.clone());if let Some(subgraph)=&self.subgraph{
            store.publish_environment_dependent_retaining(&mut self.dependent,stage.into(),self.pin.clone(),runnable.clone(),subgraph.clone(),self.clock.clone())?;
            let owner=self.dependent.as_ref().unwrap().acknowledgment()?;
            ensure!(owner.root_data()==runnable,"original dependent runnable ACK differs");
            // Stage in the whole original environment carrier before deriving;
            // uncertain capture is retained and never replaced or replayed.
            owner.derive_source_parent_retaining(&mut self.source_parent,self.clock.clone())?;
        }else{store.publish_backed_retaining(&mut self.publication,stage.into(),self.pin.clone(),runnable.clone(),self.sources.clone(),self.clock.clone())?;
            ensure!(self.publication.as_ref().unwrap().acknowledgment()?.root_data()==runnable,"original backed runnable ACK differs");}self.same_request(original,cancelled)
    }
    pub fn source_parent(&self)->Result<Option<Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>>{
        if let Some(parent)=&self.source_parent{parent.binding_data()?;}
        ensure!(self.dependent.is_none()||self.source_parent.is_some(),"original dependent source parent absent");Ok(self.source_parent.clone())
    }
    /// Borrow original retained failure without consuming, retrying or reading
    /// back a pin. Product callers can use closed diagnostic DATA instead.
    pub fn dependent_publication_failure(&self)->Option<&anyhow::Error>{self.dependent.as_ref().and_then(|p|p.failure())}
    pub fn source_parent_failure(&self)->Option<&anyhow::Error>{self.source_parent.as_ref().and_then(|p|p.failure())}
    pub fn dependent_publication_diagnostic_data(&self)->Option<serde_json::Value>{self.dependent.as_ref().map(|p|p.diagnostic_data())}
    pub fn pin_name(&self)->&str{&self.pin}
    pub fn runnable_owner(&self)->Result<&Arc<BackedPinOwner>>{self.publication.as_ref().context("actual backed initial publication absent")?.acknowledgment()}
    pub fn flat_runnable_owner(&self)->Result<Option<Arc<BackedPinOwner>>>{self.publication.as_ref().map(|p|p.acknowledgment().cloned()).transpose()}
    pub fn dependent_runnable_owner(&self)->Result<Option<Arc<EnvironmentDependentPinOwner>>>{self.dependent.as_ref().map(|p|p.acknowledgment().cloned()).transpose()}
}
#[allow(clippy::too_many_arguments)]
pub async fn prepare_initial_source_backed_retaining(store:LocalStore,request:InitialSourceRequest<'_>,profile:crate::seed::SeedProfile,
    paged:&mut Option<crate::seed::PagedSeedGeneration>,import:&mut Option<RetainedGitImport>,backed:&mut BackedInitialEnvironment,
    original:&FinalizationBudget,cancelled:Arc<AtomicBool>,validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    prepare_initial_source_backed_history_retaining(store,request,profile,GitHistoryProfile::RawObjectsV1,paged,import,backed,original,cancelled,validate_owner).await
}
#[allow(clippy::too_many_arguments)]
pub async fn prepare_initial_source_backed_history_retaining(store:LocalStore,request:InitialSourceRequest<'_>,profile:crate::seed::SeedProfile,history:GitHistoryProfile,
    paged:&mut Option<crate::seed::PagedSeedGeneration>,import:&mut Option<RetainedGitImport>,backed:&mut BackedInitialEnvironment,
    original:&FinalizationBudget,cancelled:Arc<AtomicBool>,validate_owner:impl FnMut()->Result<()>)->Result<PreparedInitialSource>{
    prepare_initial_source_with_retention(store,request,profile,history,paged,import,original,cancelled,InitialEnvironmentRetention::Backed(backed),validate_owner).await
}
