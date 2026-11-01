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
use rudder_vcs_bridge::{GitCommitId, initial_source::{InitialSourceRequest, PreparedInitialSource, prepare_initial_source}};
use std::{fs::{self, File, OpenOptions}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf, sync::atomic::{AtomicBool, Ordering}};

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
    prepared: Option<PreparedInitialSource>, integration: Option<DirectoryCustody>,
    cas_anchor: DirectoryCustody, project_anchor: DirectoryCustody,
    store: LocalStore, project: ManagedStore, journal: Journal,
    head: GitCommitId, seed_destination: PathBuf, attempted: bool, acknowledged: bool,
    lease: EngineSourceLease,
}
impl ManagedInitialSourceOwner {
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
        ensure!(ManagedStore::open(&journal,project.binding())?==project,"initial source project differs from held Journal");
        let metadata=fs::symlink_metadata(store.root())?;
        ensure!(store.root().is_absolute()&&store.root().canonicalize()?==store.root()
            &&metadata.is_dir()&&metadata.uid()==unsafe{libc::geteuid()}&&metadata.mode()&0o777==0o700,
            "initial source CAS must be the explicit private store owned by this user");
        let cas_anchor=DirectoryCustody::open(store.root().to_path_buf())?;
        let project_anchor=DirectoryCustody::open(project.binding().directory.clone())?;
        let seed_destination=project.binding().directory.join(format!("initial-seed-{}",uuid::Uuid::new_v4()));
        Ok(Self {prepared:None,integration:None,cas_anchor,project_anchor,store,project,journal,head,seed_destination,attempted:false,acknowledged:false,lease})
    }
    fn check(&self, budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        budget.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"initial source preparation cancelled");
        self.cas_anchor.check()?;self.project_anchor.check()?;
        self.lease.check_journal(&self.journal)?;
        let state=self.journal.project_state(&self.lease.owner().project_id)?;
        ensure!(state.owner_active&&state.owner==*self.lease.owner()&&state.integration_head==self.head.0,
            "initial accepted project owner or head changed");
        ensure!(ManagedStore::open(&self.journal,self.project.binding())?==self.project,"initial project namespace changed");
        if let Some(directory)=&self.integration {directory.check()?;}
        budget.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"initial source preparation cancelled");Ok(())
    }
    pub(crate) fn prepare(&mut self,environment:&ObjectRef,settings:&UserSettings,
        budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.prepare_inner(environment,settings,budget,cancelled,||Ok(()))
    }
    fn prepare_inner(&mut self,environment:&ObjectRef,settings:&UserSettings,
        budget:&FinalizationBudget,cancelled:&AtomicBool,after_result:impl FnOnce()->Result<()>)->Result<()> {
        ensure!(!self.attempted,"initial source preparation already attempted; retain and recover original outputs");
        self.attempted=true;self.check(budget,cancelled)?;
        self.integration=Some(DirectoryCustody::create(self.project.binding().directory.join("integration"))?);
        self.check(budget,cancelled)?;
        let result=pollster::block_on(prepare_initial_source(self.store.clone(),InitialSourceRequest {
            source_git:&self.project.binding().repository,private_repository:&self.integration.as_ref().unwrap().path,
            seed_destination:&self.seed_destination,expected_head:&self.head,environment,settings},budget,||self.check(budget,cancelled)))?;
        // Retain the actual returned owners BEFORE any final fallible ACK/check.
        self.prepared=Some(result);after_result()?;self.check(budget,cancelled)?;self.acknowledged=true;Ok(())
    }
    pub(crate) fn prepared_current(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<&PreparedInitialSource> {
        self.check(budget,cancelled)?;
        ensure!(self.acknowledged,"initial source original preparation acknowledgment unavailable");
        self.prepared.as_ref().context("initial source did not receive its original preparation acknowledgment")
    }
}

#[cfg(test)]
#[path="managed_initial_source_tests.rs"]
mod tests;
