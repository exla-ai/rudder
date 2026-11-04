//! Binary-only closure of covered task and generation reservations. This owns
//! no complete runtime census, task completion proof, Root intent or release.
use super::*;
use rudder_native::root_storage_bootstrap::runtime_image::{RuntimeNativeUseObservation,RuntimeNativeUseData};
use crate::managed_app::runtime_task::OriginalTaskRuntimeUse;
use rudder_engine::journal as recovery_journal;

#[path = "runtime_recovery/mod.rs"]
pub(crate) mod restart;

/// Stored INSIDE WorkerCarrier. No Shared/cell/root back-reference: retaining
/// this campaign cannot create Shared -> campaign -> Shared ownership cycles.
pub(super) struct CoveredTaskGenerationAdmission {
    campaign:Option<Arc<CoveredCampaign>>,
    closed:bool,
}
struct CoveredCampaign {original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl CoveredTaskGenerationAdmission {
    pub(super) fn new()->Self{Self{campaign:None,closed:false}}
    fn retain_campaign(&mut self,original:&Arc<CoveredCampaign>)->Result<()> {
        if let Some(actual)=&self.campaign {ensure!(Arc::ptr_eq(actual,original),"different original covered reservation cleanup campaign");}
        else {self.campaign=Some(original.clone());}
        Ok(())
    }
    fn close(&mut self,original:&Arc<CoveredCampaign>)->Result<()> {
        ensure!(self.campaign.as_ref().is_some_and(|actual|Arc::ptr_eq(actual,original)),"original covered reservation campaign absent");
        self.closed=true;Ok(())
    }
    pub(super) fn require_task_reservation_open(&self)->Result<()> {
        ensure!(!self.closed,"covered new task reservations closed; original work retained");Ok(())
    }
    pub(super) fn generation_discovery_closed(&self)->bool{self.closed}
}

/// These obligations are ALWAYS Pending in this slice, including empty covered
/// registries and physically joined workers. No public completion constructor.
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum UncoveredRuntimeProducers {Pending}
#[derive(Clone,Copy,Debug)]
pub(crate) struct CoveredTaskGenerationProgress {
    pub covered_task_and_generation_reservations_closed:bool,
    pub covered_ingress_fresh_roots_closed:bool,
    pub original_ingress_owner_retained:bool,
    pub original_ingress_roots_associated:bool,
    pub retained_command_roots:usize,pub retained_fresh_roots:usize,
    pub retained_command_jobs:usize,pub retained_command_selected_sources:usize,pub retained_command_scopes:usize,
    pub original_command_descendants_associated:bool,
    pub original_boundary_roots_captured:bool,
    pub original_task_cells_captured:bool,
    pub retained_project_roots:usize,
    pub retained_seed_roots:usize,
    pub retained_task_cells:usize,
    pub retained_generation_roots:usize,
    pub retained_source_reservation_roots:usize,
    pub retained_original_runtime_client:bool,
    pub original_task_use_sample_current:bool,
    pub original_task_uses:OriginalTaskUseData,
    pub original_native_uses:RuntimeNativeUseData,
    pub other_runtime_producers:UncoveredRuntimeProducers,
}
/// Last successful bounded task-cell sample only, never an aggregate issuer.
#[derive(Clone,Copy,Debug,Default)]
pub(crate) struct OriginalTaskUseData {
    pub visited_tasks:usize,pub pending_tasks:usize,pub started_image_tasks:usize,
    pub started_legacy_tasks:usize,pub never_started_tasks:usize,
}
struct SeedRoots {original:SeedTicket,tasks:Option<Vec<SeedTaskTicket>>}
/// External-only custody. Job -> selected source is strong; the reverse task
/// continuation remains weak. No node here is stored back into a producer.
struct CommandDescendants {
    root:Arc<IngressCommandRoot>,actual:Option<Arc<crate::managed_machine_ingress::IngressJob>>,
    selected:Option<crate::managed_machine_ingress::OriginalTaskSelectedSource>,scope:Option<SeedTaskTicket>,associated:bool,
}
fn retain_original_task(out:&mut Vec<SeedTaskTicket>,actual:&SeedTaskTicket) {
    if !out.iter().any(|held|held.same_original_task(actual)){out.push(actual.clone());}
}
struct CoveredRoots {
    projects:Vec<ProjectTicket>,seeds:Vec<SeedRoots>,
    generations:Vec<Arc<source_generations::SourceGeneration>>,
    source_reservations:Vec<Arc<source_generations::RootSourceReservation>>,
    within_known_bounds:bool,
}
impl CoveredRoots {
    /// Called once at closure while holding the SAME mutex used by task reserve,
    /// generation discovery and registry removal. Clone original typed roots;
    /// never borrow their payloads, invoke callbacks, do IO or join under it.
    fn capture(c:&WorkerCarrier)->Self {
        let max_seeds=MAX_SEEDS+seeds::MAX_RETIRED_SEEDS;
        let max_generations=source_generations::MAX_GENERATIONS;
        let within_known_bounds=c.projects.len()<=MAX_PROJECTS&&c.seeds.len()<=MAX_SEEDS
            &&c.retired_seeds.len()<=seeds::MAX_RETIRED_SEEDS&&c.generations.len()<=max_generations
            &&c.source_reservations.len()<=max_generations;
        Self {projects:c.projects.values().take(MAX_PROJECTS).cloned().collect(),
            seeds:c.seeds.values().chain(c.retired_seeds.values()).take(max_seeds)
                .map(|seed|SeedRoots{original:seed.clone(),tasks:None}).collect(),
            generations:c.generations.values().take(max_generations).cloned().collect(),
            source_reservations:c.source_reservations.values().take(max_generations).cloned().collect(),
            within_known_bounds}
    }
}

/// EXTERNAL retained owner. The machine stores only the campaign above, never
/// this owner or its roots. Dropping this value grants no drain/release and does
/// not reopen admission or permit a replacement cleanup campaign.
#[must_use="retain the original covered reservation campaign and partial roots"]
pub(crate) struct CoveredTaskGenerationDrain {
    shared:Arc<Shared>,ingress:crate::managed_machine_ingress::ManagedIngressControl,
    campaign:Arc<CoveredCampaign>,roots:Option<CoveredRoots>,closed:bool,
    producer_roots:Option<super::producer_reservations::CoveredProducerRoots>,producer_roots_associated:bool,
    command_descendants:Vec<CommandDescendants>,task_cells:Vec<SeedTaskTicket>,
    runtime_uses:Option<RuntimeNativeUseObservation>,task_uses:OriginalTaskUseData,
    task_use_sample_current:bool,native_uses:RuntimeNativeUseData,
}
impl CoveredTaskGenerationDrain {
    #[cfg(feature="managed-ingress-genuine-fixture")]
    pub(super) fn fixture_contains_original_command_root(&self,actual:&Arc<IngressCommandRoot>)->bool {
        self.producer_roots.as_ref().is_some_and(|roots|roots.fixture_contains_original_command_root(actual))
    }
    /// Cached DATA only, usable after expiry. No locking, waiting, census retry,
    /// completion consumption, IO or authority follows from these counts.
    pub(crate) fn progress_data(&self)->CoveredTaskGenerationProgress {
        let roots=self.roots.as_ref();
        CoveredTaskGenerationProgress {
            covered_task_and_generation_reservations_closed:self.closed,
            covered_ingress_fresh_roots_closed:self.producer_roots.is_some(),original_ingress_owner_retained:true,
            original_ingress_roots_associated:self.producer_roots_associated,
            retained_command_roots:self.producer_roots.as_ref().map_or(0,|r|r.command_count()),
            retained_fresh_roots:self.producer_roots.as_ref().map_or(0,|r|r.fresh_count()),
            retained_command_jobs:self.command_descendants.iter().filter(|c|c.actual.is_some()).count(),
            retained_command_selected_sources:self.command_descendants.iter().filter(|c|c.selected.is_some()).count(),
            retained_command_scopes:self.command_descendants.iter().filter(|c|c.scope.is_some()).count(),
            original_command_descendants_associated:self.producer_roots.is_some()&&self.command_descendants.iter().all(|c|c.associated),
            original_boundary_roots_captured:roots.is_some_and(|roots|roots.within_known_bounds),
            original_task_cells_captured:self.producer_roots.as_ref().is_some_and(|r|r.command_count()==0&&r.fresh_count()==0)&&roots.is_some_and(|roots|roots.within_known_bounds&&roots.seeds.iter().all(|seed|seed.tasks.is_some())),
            retained_project_roots:roots.map_or(0,|roots|roots.projects.len()),
            retained_seed_roots:roots.map_or(0,|roots|roots.seeds.len()),
            retained_task_cells:self.task_cells.len(),
            retained_generation_roots:roots.map_or(0,|roots|roots.generations.len()),
            retained_source_reservation_roots:roots.map_or(0,|roots|roots.source_reservations.len()),
            retained_original_runtime_client:self.runtime_uses.is_some(),
            original_task_use_sample_current:self.task_use_sample_current,
            original_task_uses:self.task_uses,original_native_uses:self.native_uses,
            other_runtime_producers:UncoveredRuntimeProducers::Pending,
        }
    }
    /// Incremental capture outside every carrier lock. Command roots are fixed
    /// at closure, but an already admitted command can select/admit after that.
    /// No poll replaces a captured Job/source/scope or discards a partial cell.
    fn retain_command_descendants(&mut self)->Result<()> {
        let roots=self.producer_roots.as_ref().context("original command roots absent")?;
        for root in roots.command_roots() {
            if !self.command_descendants.iter().any(|held|Arc::ptr_eq(&held.root,root)) {
                self.command_descendants.push(CommandDescendants{root:root.clone(),actual:None,selected:None,scope:None,associated:false});
            }
        }
        for held in &mut self.command_descendants {
            time(&self.campaign.original,&self.campaign.cancelled)?;
            if !held.root.retain_original_job(&mut held.actual)?{continue;}
            let actual=held.actual.as_ref().unwrap();
            // Original Job is retained BEFORE map lookup or payload association.
            if !self.ingress.retains_original_command(actual)?{continue;}
            held.associated=actual.retain_original_task_descendants(&held.root,&mut held.selected,&mut held.scope)?;
            if held.associated {if let Some(scope)=&held.scope{retain_original_task(&mut self.task_cells,scope);}}
        }
        Ok(())
    }
    /// Only original retained task cells, outside carrier/ledger locks. Keep
    /// one cached DATA sample, never a strong per-poll history of completions.
    fn observe_retained_original_uses(&mut self)->Result<()> {
        let Some(roots)=&self.roots else {return Ok(());};
        let mut tasks=OriginalTaskUseData::default();let mut helpers=Vec::new();
        if roots.seeds.iter().any(|seed|seed.tasks.is_none()){return Ok(());}
        for original in &self.task_cells {
            time(&self.campaign.original,&self.campaign.cancelled)?;
            tasks.visited_tasks+=1;
            match original.observe_original_runtime_use()? {
                seeds::TaskRuntimeObservation::Pending=>tasks.pending_tasks+=1,
                seeds::TaskRuntimeObservation::NeverStarted=>tasks.never_started_tasks+=1,
                seeds::TaskRuntimeObservation::Started(OriginalTaskRuntimeUse::Legacy)=>tasks.started_legacy_tasks+=1,
                seeds::TaskRuntimeObservation::Started(OriginalTaskRuntimeUse::Image(actual))=>{
                    tasks.started_image_tasks+=1;helpers.push(actual);
                },
            }
        }
        self.task_uses=tasks;
        self.task_use_sample_current=self.command_descendants.iter().all(|held|held.associated);
        if self.task_use_sample_current {if let Some(actual)=&self.runtime_uses {
            if let Some(data)=actual.sample_original_helpers(&helpers)? {self.native_uses=data;}
        }}
        // A missing actual client, unmatched helper, legacy task, live sibling,
        // read alias or uncovered producer never turns into complete admission.
        Ok(())
    }
}
impl MachineControl {
    /// OFF App; publish original custody BEFORE fallible campaign admission,
    /// clock checks or capture. No effectful worker, replacement clock or join.
    pub(crate) fn begin_covered_task_generation_drain_retaining(&self,out:&mut Option<CoveredTaskGenerationDrain>,
        ingress:crate::managed_machine_ingress::ManagedIngressControl,original_cleanup:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<CoveredTaskGenerationProgress> {
        ensure!(out.is_none(),"original covered reservation drain output occupied");
        *out=Some(CoveredTaskGenerationDrain{shared:self.shared.clone(),ingress,producer_roots:None,producer_roots_associated:false,
            campaign:Arc::new(CoveredCampaign{original:original_cleanup,cancelled}),roots:None,closed:false,
            command_descendants:Vec::new(),task_cells:Vec::new(),
            runtime_uses:None,task_uses:OriginalTaskUseData::default(),task_use_sample_current:false,native_uses:RuntimeNativeUseData::default()});
        self.poll_covered_task_generation_drain_retaining(out.as_mut().unwrap())
    }
    /// Poll takes NO budget, IDs, completion booleans or callbacks from callers.
    /// Busy is Pending; errors retain this same owner and every partial root.
    pub(crate) fn poll_covered_task_generation_drain_retaining(&self,owner:&mut CoveredTaskGenerationDrain)->Result<CoveredTaskGenerationProgress> {
        // Preserve last successful DATA while invalidating freshness BEFORE
        // entry guards, fallible clocks or borrows. Refusal leaves old counts stale.
        owner.task_use_sample_current=false;owner.native_uses.sampled=false;owner.producer_roots_associated=false;
        for held in &mut owner.command_descendants{held.associated=false;}
        ensure!(Arc::ptr_eq(&self.shared,&owner.shared),"foreign original covered reservation machine");
        ensure!(owner.ingress.belongs_to_machine(self),"foreign original ingress owner; retain close candidate");
        ensure!(self.shared.app_thread!=std::thread::current().id(),"covered reservation drain requires off App");
        // First retain the one original campaign in machine storage. Release
        // the mutex before sampling its clock; budget checks never run under it.
        match self.shared.carrier.try_lock() {
            Ok(mut carrier)=>carrier.covered_reservations.retain_campaign(&owner.campaign)?,
            Err(TryLockError::WouldBlock)=>{
                #[cfg(feature="managed-ingress-genuine-fixture")]
                if let Some(gate)=owner.ingress.fixture_gate(){gate.actual_carrier_would_block();}
                return Ok(owner.progress_data())
            },
            Err(TryLockError::Poisoned(_))=>anyhow::bail!("original machine carrier poisoned; covered drain retained"),
        }
        time(&owner.campaign.original,&owner.campaign.cancelled)?;
        if owner.roots.is_none() {
            match self.shared.carrier.try_lock() {
                Ok(mut carrier)=>{
                    carrier.covered_reservations.close(&owner.campaign)?;
                    // Both root gates close in this one authoritative mutex.
                    // No ingress/Job/environment payload lock is taken here.
                    owner.producer_roots=Some(carrier.producer_roots.close_capture());
                    owner.closed=true;
                    // Pin original roots in this same critical section, before
                    // collector/discovery/task-reservation can run again.
                    owner.roots=Some(CoveredRoots::capture(&carrier));
                },
                Err(TryLockError::WouldBlock)=>{
                #[cfg(feature="managed-ingress-genuine-fixture")]
                if let Some(gate)=owner.ingress.fixture_gate(){gate.actual_carrier_would_block();}
                return Ok(owner.progress_data())
            },
                Err(TryLockError::Poisoned(_))=>anyhow::bail!("original machine carrier poisoned; campaign retained"),
            }
        }
        owner.retain_command_descendants()?;
        owner.producer_roots_associated=owner.producer_roots.as_ref().context("original producer roots absent")?.validate_retained(&owner.ingress)?;
        if !owner.producer_roots_associated{return Ok(owner.progress_data());}
        let roots=owner.roots.as_mut().unwrap();
        ensure!(roots.within_known_bounds,"covered registry exceeded known bounds; partial original roots retained");
        // No carrier lock here. Strong seed roots prevent retired collection;
        // task maps never remove individual cells. Busy owners stay Pending.
        for seed in &mut roots.seeds {
            if seed.tasks.is_none() {
                time(&owner.campaign.original,&owner.campaign.cancelled)?;
                seed.original.capture_task_roots_retaining(&mut seed.tasks)?;
            }
            if let Some(tasks)=&seed.tasks {for task in tasks{retain_original_task(&mut owner.task_cells,task);}}
        }
        time(&owner.campaign.original,&owner.campaign.cancelled)?;
        // Capture only an alias here. This method performs no client/ledger
        // lock, bootstrap/currentness IO or admission closure under carrier.
        match self.shared.carrier.try_lock() {
            Ok(carrier)=>carrier.runtime_image.retain_original_use_observation(&mut owner.runtime_uses)?,
            Err(TryLockError::WouldBlock)=>return Ok(owner.progress_data()),
            Err(_)=>anyhow::bail!("original runtime client carrier poisoned; observation retained"),
        }
        if let Err(error)=owner.observe_retained_original_uses().and_then(|()|time(&owner.campaign.original,&owner.campaign.cancelled)) {
            owner.task_use_sample_current=false;owner.native_uses.sampled=false;owner.producer_roots_associated=false;return Err(error);
        }
        Ok(owner.progress_data())
    }
}

#[cfg(test)]
#[path="managed_runtime_drain_tests.rs"]
mod tests;

#[cfg(test)]
#[path="managed_runtime_observation_tests.rs"]
mod observation_tests;
