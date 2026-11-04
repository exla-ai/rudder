//! Bounded observations of original producer owners. These are diagnostic
//! samples, never runtime-drained, resource-retirement or release receipts.
use anyhow::Result;
use rudder_engine::execution::handoff::OwnedWorkerJoin;
use std::sync::{Mutex,MutexGuard,TryLockError};

#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub(crate) struct WorkerCensus {
    pub slots:usize,pub registered:usize,pub joined:usize,
}
impl WorkerCensus {
    pub(crate) fn observe(actual:Option<&OwnedWorkerJoin>)->Result<Self> {
        let mut data=Self{slots:1,..Self::default()};
        if let Some(actual)=actual {
            data.registered=1;actual.start_join_observer()?;
            data.joined=usize::from(actual.try_joined()?.is_some());
        }
        Ok(data)
    }
    pub(crate) fn all_registered_joined(&self)->bool{self.registered==self.joined}
    pub(crate) fn include(&mut self,other:Self) {
        self.slots+=other.slots;self.registered+=other.registered;self.joined+=other.joined;
    }
}
/// None means contention, never an empty owner or a successful census.
pub(crate) fn borrow<T>(actual:&Mutex<T>)->Result<Option<MutexGuard<'_,T>>> {
    match actual.try_lock() {
        Ok(held)=>Ok(Some(held)),Err(TryLockError::WouldBlock)=>Ok(None),
        Err(_)=>anyhow::bail!("original producer custody poisoned; census owners retained"),
    }
}
/// Clone under the registration lock, then observe outside it. Never wait for
/// a worker's payload, qualification clock, result, callback or TLS destructor.
pub(crate) fn worker(actual:&Mutex<Option<OwnedWorkerJoin>>)->Result<Option<WorkerCensus>> {
    let handle={let Some(held)=borrow(actual)?else{return Ok(None)};held.clone()};
    Ok(Some(WorkerCensus::observe(handle.as_ref())?))
}
#[derive(Clone,Copy,Debug,Default)]
pub(crate) struct RuntimeProducerCensusData {
    pub command_workers:WorkerCensus,
    pub fresh_attempts:usize,pub pending_fresh_attempts:usize,pub terminal_fresh_attempts_without_coordinator:usize,
    pub kernel_probes:WorkerCensus,pub kernel_retirements:WorkerCensus,
    pub fresh_coordinators:WorkerCensus,pub fresh_flows:WorkerCensus,
    pub environment_workers:WorkerCensus,pub image_importers:WorkerCensus,pub genesis_importers:WorkerCensus,
    pub generation_factories:WorkerCensus,pub generation_bootstraps:WorkerCensus,pub generation_retirements:WorkerCensus,
    pub sdk_cleanups:WorkerCensus,pub canonical_workers:WorkerCensus,pub canonical_sdk_cleanups:WorkerCensus,
    pub sdk_campaigns:usize,pub canonical_campaigns:usize,
    pub unsettled_sdk_campaigns:usize,pub unsettled_canonical_campaigns:usize,
}
impl RuntimeProducerCensusData {
    /// Parent join seals its finite child discovery. Unstarted flow roles are
    /// then absent, but any actually spawned importer must still join itself.
    pub(crate) fn fresh_discovery_quiescent(&self)->bool {
        self.pending_fresh_attempts==0&&self.fresh_coordinators.slots==self.fresh_coordinators.registered
            &&self.fresh_coordinators.all_registered_joined()&&self.fresh_flows.all_registered_joined()
            &&self.environment_workers.all_registered_joined()&&self.image_importers.all_registered_joined()
            &&self.genesis_importers.all_registered_joined()
    }
    pub(crate) fn include(&mut self,other:Self) {
        self.command_workers.include(other.command_workers);
        self.fresh_attempts+=other.fresh_attempts;self.pending_fresh_attempts+=other.pending_fresh_attempts;
        self.terminal_fresh_attempts_without_coordinator+=other.terminal_fresh_attempts_without_coordinator;
        self.kernel_probes.include(other.kernel_probes);self.kernel_retirements.include(other.kernel_retirements);
        self.fresh_coordinators.include(other.fresh_coordinators);self.fresh_flows.include(other.fresh_flows);
        self.environment_workers.include(other.environment_workers);self.image_importers.include(other.image_importers);self.genesis_importers.include(other.genesis_importers);
        self.generation_factories.include(other.generation_factories);self.generation_bootstraps.include(other.generation_bootstraps);self.generation_retirements.include(other.generation_retirements);
        self.sdk_cleanups.include(other.sdk_cleanups);self.canonical_workers.include(other.canonical_workers);self.canonical_sdk_cleanups.include(other.canonical_sdk_cleanups);
        self.sdk_campaigns+=other.sdk_campaigns;self.canonical_campaigns+=other.canonical_campaigns;
        self.unsettled_sdk_campaigns+=other.unsettled_sdk_campaigns;self.unsettled_canonical_campaigns+=other.unsettled_canonical_campaigns;
    }
}

#[cfg(test)]
#[path="managed_runtime_producer_census_tests.rs"]
mod tests;
