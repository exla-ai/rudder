//! Initial immutable source from the actual current managed project owner.
//! Blocking, off App. Keep this mutable carrier outside worker catch/unwind and
//! rejected-result paths; failed/uncertain preparation is never retried in place.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::{engine_host::{EngineOwnership, EngineSourceLease}, managed_store::ManagedStore,
    managed_task_launch::ManagedTaskOptIn};
use jj_lib::settings::UserSettings;
use rudder_engine::{execution::finalization_budget::FinalizationBudget, journal::Journal,
    protocol::ObjectRef, store::LocalStore};
use rudder_vcs_bridge::{GitCommitId, initial_source::{InitialSourceRequest, PreparedInitialSource}};
use std::{fs::{self, File, OpenOptions}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf, sync::{Arc,atomic::{AtomicBool, Ordering}}};

struct DirectoryCustody { path: PathBuf, held: File }
impl DirectoryCustody {
    fn create(path: PathBuf) -> Result<Self> {
        fs::DirBuilder::new().mode(0o700).create(&path)
            .context("initial integration directory already exists; explicit recovery is required")?;
        Self::open(path)
    }
    fn open(path: PathBuf) -> Result<Self> {
        let held = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path)?;
        let owner = Self {path,held}; owner.check()?; Ok(owner)
    }
    fn clone_held(&self)->Result<Self>{self.check()?;let copy=Self{path:self.path.clone(),held:self.held.try_clone()?};copy.check()?;Ok(copy)}
    fn check(&self) -> Result<()> {
        let a=self.held.metadata()?;let b=fs::symlink_metadata(&self.path)?;
        ensure!(a.is_dir()&&b.is_dir()&&a.dev()==b.dev()&&a.ino()==b.ino()
            && a.uid()==unsafe{libc::geteuid()}&&b.uid()==a.uid()
            && a.mode()&0o777==0o700&&b.mode()&0o777==0o700,
            "initial integration directory custody changed"); Ok(())
    }
}

/// Captures actual owner/Journal provenance; cannot be made from saved root or
/// path DATA. Drop performs no unpin/retry/cleanup. Dispose only off App.
pub(crate) struct ManagedInitialSourceOwner {
    // Actual workspace/verifier/metadata handles close before final source lease.
    paged:Arc<std::sync::Mutex<Option<rudder_vcs_bridge::seed::PagedSeedGeneration>>>,
    git_import:Arc<std::sync::Mutex<Option<rudder_vcs_bridge::accepted_git_import::RetainedGitImport>>>,
    prepared: Option<PreparedInitialSource>, fresh:Option<std::sync::Arc<crate::managed_fresh_genesis::ManagedFreshGenesis>>, integration: Option<DirectoryCustody>,
    accepted_input:Option<Arc<crate::managed_checkpoint_accept::AcceptedSourceInput>>,
    successor:Option<rudder_vcs_bridge::initial_source::AcceptedSourceGeneration>,
    previous:Option<PreviousGeneration>,
    cas_anchor: DirectoryCustody, project_anchor: DirectoryCustody,
    store: LocalStore, project: ManagedStore, journal: Journal,
    head: GitCommitId, seed_destination: PathBuf, attempted: bool, acknowledged: bool,
    lease: EngineSourceLease,
}
struct PreviousGeneration {baseline:rudder_vcs_bridge::CommitIdentity,seed:ObjectRef,
    cache:Arc<rudder_vcs_bridge::seed::RetainedSeedCasCache>}
impl ManagedInitialSourceOwner {
    pub(crate) fn owner(&self)->&rudder_engine::journal::OwnerFence {self.lease.owner()}
    pub(crate) fn cas_root(&self)->&std::path::Path {self.store.root()}
    pub(crate) fn accepted_head(&self)->&str{&self.head.0}
    pub(crate) fn accepted_input(&self)->Option<&Arc<crate::managed_checkpoint_accept::AcceptedSourceInput>>{self.accepted_input.as_ref()}
    pub(crate) fn store_clone(&self)->LocalStore{self.store.clone()}
    pub(crate) fn release_successor_input_retaining(&self,out:&mut Option<crate::managed_checkpoint_accept::AcceptedInputRelease>,acceptance:&mut crate::managed_checkpoint_accept::ManagedCheckpointAcceptance,budget:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        acceptance.release_generation_lock_retaining(out,self,&self.journal,budget,cancel)
    }
    pub(crate) fn capture(owner: &mut EngineOwnership, _selection: &ManagedTaskOptIn, store: LocalStore) -> Result<Self> {
        ensure!(owner.is_fabric(),"initial managed source requires the actual Fabric owner");
        owner.assert_current()?;
        let project=owner.managed_recovery_store().context("managed preparation must finish before initial source capture")?.clone();
        let fence=owner.journal_parts().1.clone();
        let lease=owner.source_lease(&fence)?;
        let journal=lease.open_source_writer()?;
        lease.check_journal(&journal)?;
        let state=journal.project_state(&fence.project_id)?;
        ensure!(state.owner_active&&state.owner==fence,"initial managed source owner is not current");
        let head=GitCommitId(state.integration_head);head.validate()?;
        ensure!(project.reopen_current(&journal)?==project,"initial source project differs from held Journal");
        let metadata=fs::symlink_metadata(store.root())?;
        ensure!(store.root().is_absolute()&&store.root().canonicalize()?==store.root()
            &&metadata.is_dir()&&metadata.uid()==unsafe{libc::geteuid()}&&metadata.mode()&0o777==0o700,
            "initial source CAS must be the explicit private store owned by this user");
        let cas_anchor=DirectoryCustody::open(store.root().to_path_buf())?;
        let project_anchor=DirectoryCustody::open(project.binding().directory.clone())?;
        let seed_destination=project.binding().directory.join(format!("initial-seed-{}",uuid::Uuid::new_v4()));
        Ok(Self {git_import:Arc::new(std::sync::Mutex::new(None)),prepared:None,paged:Arc::new(std::sync::Mutex::new(None)),fresh:None,integration:None,accepted_input:None,successor:None,previous:None,cas_anchor,project_anchor,store,project,journal,head,seed_destination,attempted:false,acknowledged:false,lease})
    }
    fn check(&self, budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        budget.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"initial source preparation cancelled");
        self.lease.wait_shared_current(budget,cancelled)?;
        self.cas_anchor.check()?;self.project_anchor.check()?;
        self.lease.check_journal(&self.journal)?;
        let state=self.journal.project_state(&self.lease.owner().project_id)?;
        ensure!(state.owner_active&&state.owner==*self.lease.owner()&&state.integration_head==self.head.0,
            "initial accepted project owner or head changed");
        ensure!(self.project.reopen_current(&self.journal)?==self.project,"initial project namespace changed");
        if let Some(directory)=&self.integration {directory.check()?;}
        if let Some(input)=&self.accepted_input{input.check_current(&self.lease,&self.journal,budget,cancelled)?;}
        budget.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"initial source preparation cancelled");Ok(())
    }
    pub(crate) fn prepare(&mut self,environment:&ObjectRef,settings:&UserSettings,
        budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.prepare_inner(environment,settings,budget,cancelled,||Ok(()))
    }
    pub(crate) fn prepare_paged_retaining(&mut self,environment:&ObjectRef,settings:&UserSettings,
        budget:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        self.prepare_inner_profile(environment,settings,budget,&cancelled,rudder_vcs_bridge::seed::SeedProfile::PagedCowV2,Some(cancelled.clone()),||Ok(()))
    }
    fn prepare_inner(&mut self,environment:&ObjectRef,settings:&UserSettings,
        budget:&FinalizationBudget,cancelled:&AtomicBool,after_result:impl FnOnce()->Result<()>)->Result<()> {
        self.prepare_inner_profile(environment,settings,budget,cancelled,rudder_vcs_bridge::seed::SeedProfile::SmallV1,None,after_result)
    }
    fn prepare_inner_profile(&mut self,environment:&ObjectRef,settings:&UserSettings,budget:&FinalizationBudget,cancelled:&AtomicBool,
        profile:rudder_vcs_bridge::seed::SeedProfile,actual_cancel:Option<Arc<AtomicBool>>,after_result:impl FnOnce()->Result<()>)->Result<()> {
        ensure!(self.accepted_input.is_none(),"accepted successor requires its original retained generation path");
        ensure!(!self.attempted,"initial source preparation already attempted; retain and recover original outputs");
        self.attempted=true;self.check(budget,cancelled)?;
        self.integration=Some(DirectoryCustody::create(self.project.binding().directory.join("integration"))?);
        self.check(budget,cancelled)?;
        let request=InitialSourceRequest {source_git:&self.project.binding().repository,private_repository:&self.integration.as_ref().unwrap().path,
            seed_destination:&self.seed_destination,expected_head:&self.head,environment,settings};
        let seed_slot=self.paged.clone();let import_slot=self.git_import.clone();
        let mut paged=seed_slot.lock().map_err(|_|anyhow::anyhow!("original paged seed owner poisoned; resources retained"))?;
        let mut import=import_slot.lock().map_err(|_|anyhow::anyhow!("original Git import owner poisoned; resources retained"))?;
        // V2 carries the actual Arc. Legacy cancellation is still checked by
        // the original owner callback at every publication boundary.
        let actual_cancel=actual_cancel.unwrap_or_else(||Arc::new(AtomicBool::new(false)));
        let result=pollster::block_on(rudder_vcs_bridge::initial_source::prepare_initial_source_profile_with_import_retaining(self.store.clone(),request,profile,&mut paged,&mut import,budget,actual_cancel,||self.check(budget,cancelled)))?;
        drop(import);drop(paged);

        // Retain the actual returned owners BEFORE any final fallible ACK/check.
        self.prepared=Some(result);after_result()?;self.check(budget,cancelled)?;self.acknowledged=true;Ok(())
    }
    /// Actual already-created source is moved only after genuine ACTIVE local
    /// adoption; no seed/ref/path/receipt can reconstruct the original verifier.
    pub(crate) fn adopt_fresh_prepared_retaining(&mut self,fresh:std::sync::Arc<crate::managed_fresh_genesis::ManagedFreshGenesis>,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.check(budget,cancelled)?;self.lease.check_shared_current()?;fresh.assert_assigned_retention()?;
        ensure!(!self.attempted&&self.prepared.is_none()&&fresh.preparation().data().project_id==self.lease.owner().project_id,"fresh source already attempted or different actual owner");
        let integration=fresh.integration_directory()?;ensure!(integration==self.project.binding().directory.join("integration"),"actual fresh integration belongs to another namespace");
        self.integration=Some(DirectoryCustody::open(integration)?);self.fresh=Some(fresh.clone());self.attempted=true;
        fresh.take_prepared_retaining(&mut self.prepared,&self.lease,&self.journal,budget,cancelled)?;self.check(budget,cancelled)?;self.lease.check_shared_current()?;self.acknowledged=true;Ok(())
    }
    /// Pure permission checks on the original lease; never source/Root drain.
    pub(crate) fn check_shared_admission(&self)->Result<()> {self.lease.check_active_for_adoption()}
    pub(crate) fn shared_admission_closed(&self)->bool {self.lease.shared_admission_closed()}
    /// Fixed cleanup observation only, no writer and no new head requirement.
    /// Its success can keep original resources/sibling polling alive, never
    /// admit a task or certify unknown Root IPC/seed/helper effects.
    pub(crate) fn assert_original_retained_custody(&self)->Result<()> {
        self.cas_anchor.check()?;self.project_anchor.check()?;self.lease.check_journal(&self.journal)?;
        ensure!(self.project.reopen_current(&self.journal)?==self.project,"original source namespace changed");
        let state=self.journal.project_state(&self.lease.owner().project_id)?;ensure!(state.owner_active&&state.owner==*self.lease.owner(),"original local source owner changed");
        if let Some(directory)=&self.integration{directory.check()?;}if let Some(fresh)=&self.fresh{fresh.assert_assigned_retention()?;}
        if let Some(input)=&self.accepted_input{input.check_physical()?;}Ok(())
    }
    /// NEW original source job, captured by the caller before its queue. Read
    /// only original held predecessor/acceptor owners; no saved HEAD/path/receipt
    /// can construct this new actual source owner or its authority.
    pub(crate) fn capture_successor_retaining(out:&mut Option<Self>,owner:&mut EngineOwnership,
        previous:&Self,acceptance:&mut crate::managed_checkpoint_accept::ManagedCheckpointAcceptance,
        budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(out.is_none(),"original successor output occupied");budget.remaining()?;previous.assert_original_retained_custody()?;
        owner.assert_current_bounded(budget,cancelled)?;ensure!(owner.journal_parts().1==previous.owner(),"successor belongs to another actual owner");
        let old=previous.prepared.as_ref().context("original predecessor source absent")?;ensure!(previous.acknowledged,"predecessor original ACK absent");
        let pins=previous.store.retention()?;
        ensure!(pins.pins.get(&old.runnable_pin)==Some(&old.runnable_root)&&pins.pins.get(&old.seed_pin)==Some(&old.seed_root),"predecessor original retention changed");
        let fence=owner.journal_parts().1.clone();let lease=owner.source_lease(&fence)?;let journal=lease.open_source_writer()?;
        let input=acceptance.capture_source_successor_input(&journal,old,budget,cancelled)?;
        let old_runnable=rudder_engine::snapshot::decode_runnable(&previous.store.read_object(&old.runnable_root)?)?;
        ensure!(old_runnable.environment.as_ref()==Some(&input.integration().proposed_environment_root)
            &&input.owner()==lease.owner(),"successor changed the original pinned environment/owner");
        let head=GitCommitId(input.integration().new_head.clone());head.validate()?;
        let source=Self{git_import:previous.git_import.clone(),prepared:None,paged:Arc::new(std::sync::Mutex::new(None)),fresh:previous.fresh.clone(),integration:Some(previous.integration.as_ref().context("original integration custody absent")?.clone_held()?),
            accepted_input:Some(input),successor:None,previous:Some(PreviousGeneration{baseline:old.baseline.clone(),seed:old.seed_root.clone(),cache:old.seed_cache().clone()}),
            cas_anchor:previous.cas_anchor.clone_held()?,project_anchor:previous.project_anchor.clone_held()?,store:previous.store.clone(),project:previous.project.clone(),journal,head,
            seed_destination:previous.seed_destination.clone(),attempted:false,acknowledged:false,lease};
        *out=Some(source);out.as_ref().unwrap().check(budget,cancelled)
    }
    pub(crate) fn prepare_successor_retaining(&mut self,budget:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(!self.attempted&&self.prepared.is_none(),"successor already attempted; original carrier retained");self.attempted=true;self.check(budget,&cancelled)?;
        let input=self.accepted_input.as_ref().context("genuine Published successor input absent")?.clone();let previous=self.previous.as_ref().context("actual predecessor cache absent")?;
        let pin=format!("accepted-source:{}:{}:{}",self.owner().project_id,self.owner().epoch,input.integration().sequence);
        // The canonical lock keeps the accepted head fixed; every CAS/pin phase
        // additionally checks the SAME actual lease and original clock. Busy
        // renewal waits use that clock and never become a new source attempt.
        let lease=&self.lease;
        rudder_vcs_bridge::initial_source::AcceptedSourceGeneration::prepare_retaining(&mut self.successor,self.store.clone(),input.repository().clone(),input.verified().clone(),previous.cache.clone(),
            input.integration().proposed_source_root.clone(),input.receipt().request.recovery_root.clone(),input.integration().proposed_environment_root.clone(),previous.baseline.clone(),previous.seed.clone(),
            format!("{pin}:runnable"),format!("{pin}:seed"),budget.clone(),cancelled.clone(),||lease.wait_shared_current(budget,&cancelled))?;
        self.successor.as_mut().unwrap().take_prepared_retaining(&mut self.prepared)?;self.check(budget,&cancelled)?;
        self.persist_generation_record()?;self.check(budget,&cancelled)?;self.acknowledged=true;Ok(())
    }
    fn persist_generation_record(&self)->Result<()> {
        use std::io::Write as _;let input=self.accepted_input.as_ref().unwrap();let prepared=self.prepared.as_ref().unwrap();
        let bytes=rudder_engine::protocol::canonical_json(&serde_json::json!({"format_version":1,"data_only":true,"owner":self.owner(),"intent_id":input.receipt().intent_id,
            "integration":input.integration(),"baseline":prepared.baseline,"operation":prepared.operation,"source":prepared.source_root,"runnable":prepared.runnable_root,"seed":prepared.seed_root,
            "runnable_pin":prepared.runnable_pin,"seed_pin":prepared.seed_pin}))?;
        ensure!(bytes.len()<=32*1024,"source generation DATA exceeds bound");self.project_anchor.check()?;
        let path=self.project.binding().directory.join(format!("source-generation-{}.json",input.integration().sequence));
        let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path)?;
        file.write_all(&bytes)?;file.sync_all()?;self.project_anchor.held.sync_all()?;self.project_anchor.check()
    }
    pub(crate) fn check_successor_origin(&self,identity:&Arc<()>,receipt:&rudder_engine::journal::VcsIntentReceipt,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.prepared_current(budget,cancelled)?;let input=self.accepted_input.as_ref().context("original successor provenance absent")?;
        ensure!(input.same_origin(identity)&&input.receipt()==receipt,"source ACK differs from original Published acceptor");Ok(())
    }
    pub(crate) fn prepared_current(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<&PreparedInitialSource> {
        self.check(budget,cancelled)?;
        ensure!(self.acknowledged,"initial source original preparation acknowledgment unavailable");
        let prepared=self.prepared.as_ref().context("initial source did not receive its original preparation acknowledgment")?;
        let pins=self.store.retention()?;
        ensure!(pins.pins.get(&prepared.runnable_pin)==Some(&prepared.runnable_root)&&pins.pins.get(&prepared.seed_pin)==Some(&prepared.seed_root),"original source generation retention changed");
        self.check(budget,cancelled)?;Ok(prepared)
    }
}

#[cfg(test)]
#[path="managed_initial_source_tests.rs"]
mod tests;
