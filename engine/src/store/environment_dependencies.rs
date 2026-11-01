//! One genuine prepared Environment boundary plus a bounded LOCAL source
//! graph. A persistent binding alone never supplies actual nested ownership.
use super::*;
use crate::store::environment_dependency_data::{self as data,EnvironmentBindingData,DependentPinData,DependentHoldData,EnvironmentDependenciesData};
use crate::build::environment_preparation_lineage::EnvironmentPreparationLineage;
use crate::build::RetainedBackedPreparation;
use crate::{directory::{DirectoryStore,PagedDirectory},snapshot};
use std::sync::{Mutex,MutexGuard,TryLockError,atomic::{AtomicBool,Ordering}};

#[path="environment_subgraph.rs"]mod subgraph;
pub use subgraph::{RetainedEnvironmentSubgraph,EnvironmentSubgraphRead};

// All publication/read/GC code below uses the EXISTING StoreIo and its actual
// original GC descriptor. Nested parents never acquire another local flock.
pub(super) fn parent_bytes(io:&mut StoreIo,state:&RetentionState,parent:&Arc<BackedPinOwner>,r:&ObjectRef)->Result<Vec<u8>>{
    parent.check(io,state)?;ensure!(parent.members.contains(r),"reference outside original prepared parent graph");let local=io.read_object(r,bound(r,&parent.sources)?)?;
    if parent.record.local_objects.binary_search(r).is_ok(){return local.context("original prepared local object absent")}
    // A local replica is verified above, but cannot replace the original
    // foreign obligation. Read/hash that original for this explicit parent IO.
    let source=parent.sources.iter().find(|s|s.contains_data(r)).context("original prepared foreign source absent")?;
    ensure!(io.foreign.is_none(),"original parent foreign read still retained");source.read_retaining(&mut io.foreign,r.clone(),io.original.clone())?;
    let raw=io.foreign.as_ref().unwrap().verified_bytes()?.to_vec();parent.check(io,state)?;io.original.remaining()?;io.foreign.take();Ok(raw)
}

// Continued implementations in separate children keep original ownership and
// profile limits explicit; this module does not expose a generic skip callback.
#[path="environment_dependency_publication.rs"]mod publication;
pub use publication::{EnvironmentDependentPublication,EnvironmentDependentPinOwner,EnvironmentDependentRead,EnvironmentDependentReattachment,DependentOwnerOrigin};
pub(super) use publication::{gc_check,gc_local};

/// These variants retain genuine owners. The enum supplies no constructor
/// from a root, path, saved record or callback and adds no execution authority.
#[derive(Clone)]
pub enum RetainedRunnableInputs{Flat(Arc<BackedPinOwner>),Environment(Arc<EnvironmentDependentPinOwner>)}
impl RetainedRunnableInputs{
    pub fn root_data(&self)->&ObjectRef{match self{Self::Flat(p)=>p.root_data(),Self::Environment(p)=>p.root_data()}}
    pub fn pin_name_data(&self)->&str{match self{Self::Flat(p)=>&p.pin,Self::Environment(p)=>p.pin_name_data()}}
    pub fn store_root_data(&self)->&std::path::Path{match self{Self::Flat(p)=>p.store.root(),Self::Environment(p)=>p.store_root_data()}}
    pub fn contains_data(&self,r:&ObjectRef,original:&dyn ImageImportFence)->Result<bool>{original.remaining()?;match self{Self::Flat(p)=>Ok(p.members.contains(r)),Self::Environment(p)=>p.contains_data(r,original)}}
    pub fn matches_record_data(&self,state:&RetentionState,pin:&str)->bool{match self{
        Self::Flat(p)=>p.pin==pin&&state.backed_retention.as_ref().and_then(|d|d.pins.get(pin))==Some(p.record_data()),
        Self::Environment(p)=>p.matches_record_data(state,pin)}}
    pub fn read_retaining(&self,out:&mut Option<RetainedRunnableRead>,reference:ObjectRef,original:Arc<dyn ImageImportFence>)->Result<()>{
        ensure!(out.is_none(),"original retained Runnable read occupied");*out=Some(match self{Self::Flat(_)=>RetainedRunnableRead::Flat(None),Self::Environment(_)=>RetainedRunnableRead::Environment(None)});
        match(self,out.as_mut().unwrap()){(Self::Flat(p),RetainedRunnableRead::Flat(r))=>p.read_retaining(r,reference,original),
            (Self::Environment(p),RetainedRunnableRead::Environment(r))=>p.read_retaining(r,reference,original),_=>unreachable!()}}
}
pub enum RetainedRunnableRead{Flat(Option<BackedRead>),Environment(Option<EnvironmentDependentRead>)}
impl RetainedRunnableRead{pub fn verified_bytes(&self)->Result<&[u8]>{match self{
    Self::Flat(r)=>r.as_ref().context("original flat read not staged")?.verified_bytes(),
    Self::Environment(r)=>r.as_ref().context("original dependent read not staged")?.verified_bytes()}}}
impl LocalStore{
    /// ALL actual flat parents and ALL actual dependent roots are mandatory.
    /// Saved records, absent owners or unknown holds cannot authorize a sweep.
    pub fn gc_environment_dependencies_retaining(&self,out:&mut Option<BackedGc>,parents:Vec<Arc<BackedPinOwner>>,dependents:Vec<Arc<EnvironmentDependentPinOwner>>,
        grace:Duration,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original dependency-aware GC output occupied");*out=Some(BackedGc{io:StoreIo::staged(self.clone(),original),owners:parents,dependency_owners:dependents,grace,report:None,object_sets:None,candidates:Vec::new(),unlink_intent:None,attempted:false,ready:false,failure:None});out.as_mut().unwrap().run()
    }
}
