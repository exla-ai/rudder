//! Explicit Linux managed-local task entry. No default route, profile lookup,
//! environment/OAuth fallback, source import, acceptance, model call or scheduler.
//! Root wires the returned packet into the EXISTING managed_app launch worker.
#![cfg(target_os = "linux")]
use anyhow::{ensure, Context, Result};
use crate::{App, AgentMode, AgentStatus, Backend, engine_host::{EngineOwnership, EngineSourceLease},
    managed_launch::{ManagedLocalLaunchInputs, ManagedBackendProfile, SelectedCredentials},
    managed_store::ManagedStore};
use rudder_engine::{execution::{ReadOnlyRuntime, finalization_budget::FinalizationBudget},
    journal::{Journal, OwnerFence}, protocol::{ObjectKind, ObjectRef}, store::LocalStore};
use rudder_vcs_bridge::seed::VerifiedMetadataTemplate;
use std::{path::{Component, Path, PathBuf}, sync::{Arc, Weak, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering}, TryLockError}, time::Duration};

const MAX_PREPARATIONS: usize = 2;
const PREPARATION_LIMIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectedTaskCredentialKind { ApiKeyV1, OAuthV1 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedTaskNetworkSelection { OrdinaryNetworkV1 }

/// Intent DATA only; there is no Default/Serde/env constructor. Reject selected
/// OAuth here BEFORE project migration, secret reads, admission or launch effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedTaskOptIn { profile_id: String, backend: Backend }
impl ManagedTaskOptIn {
    pub(crate) fn selected(profile_id: String, backend: Backend, kind: SelectedTaskCredentialKind,
        _network: ManagedTaskNetworkSelection) -> Result<Self> {
        ensure!(!profile_id.is_empty() && profile_id.len() <= 256
            && !profile_id.chars().any(char::is_control), "invalid explicitly selected managed profile");
        ensure!(matches!(backend, Backend::Codex | Backend::Claude), "managed task backend is unsupported");
        if kind == SelectedTaskCredentialKind::OAuthV1 {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported,
                "selected OAuth profile is unsupported for managed task launch; an exclusive refresh and protected writeback adapter must be qualified first").into());
        }
        Ok(Self { profile_id, backend })
    }
    pub(crate) fn profile_id(&self) -> &str { &self.profile_id }
    pub(crate) fn backend(&self) -> Backend { self.backend }
}

/// Actual selected secret owner, never Debug/Clone/Serde. This metadata wrapper
/// proves the selected ID because it constructs the existing zeroizing owner
/// itself. It does not duplicate keys or create OAuth/API-billing fallbacks.
pub(crate) struct SelectedTaskProfile { selected: ManagedTaskOptIn, credentials: SelectedCredentials }
impl SelectedTaskProfile {
    /// OFF-APP trusted private-profile selector only; supply precisely the
    /// explicitly selected API profile's key, never inherited env/login material.
    pub(crate) fn api_key(selected: ManagedTaskOptIn, secret: String) -> Result<Self> {
        let credentials = SelectedCredentials::api_key(selected.profile_id.clone(), selected.backend, secret)?;
        Ok(Self { selected, credentials })
    }
}

/// OFF-APP bootstrap from the ACTUAL current Fabric owner and explicit prepared
/// project. No path/receipt-only constructor, owner takeover or new Journal init.
/// Root retains this Arc outside App and disposes the final owner off-App.
// Fields drop in declaration order: dispose the actual writer/store before the
// final lease can release its held Journal anchors and checkout lock.
pub(crate) struct ManagedTaskProject { journal: Mutex<Journal>, store: ManagedStore, lease: EngineSourceLease }
impl ManagedTaskProject {
    pub(crate) fn capture(owner: &mut EngineOwnership, _selected: &ManagedTaskOptIn) -> Result<Arc<Self>> {
        ensure!(owner.is_fabric(), "managed tasks require the existing Fabric owner");
        owner.assert_current()?;
        let store = owner.managed_recovery_store().context("explicit managed project preparation is required before task entry")?.clone();
        let fence = owner.journal_parts().1.clone();
        let factory = owner.source_journal_factory(&fence)?;
        let journal = factory()?; // Existing writer derived from the held owner, not a supplied path.
        let lease = owner.source_lease(&fence)?;
        lease.check_journal(&journal)?;
        ensure!(store.reopen_current(&journal)? == store, "managed project namespace changed");
        let state = journal.project_state(&fence.project_id)?;
        ensure!(state.owner_active && state.owner == fence, "managed project owner is stale");
        owner.assert_current()?;
        Ok(Arc::new(Self { lease, journal: Mutex::new(journal), store }))
    }
    pub(crate) fn capture_bounded(owner:&mut EngineOwnership,selected:&ManagedTaskOptIn,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<Arc<Self>>{
        loop{owner.assert_current_bounded(budget,cancelled)?;match Self::capture(owner,selected){Ok(value)=>return Ok(value),Err(error) if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()=>std::thread::sleep(budget.phase_limit(Duration::from_millis(2))?),Err(error)=>return Err(error)}}
    }
    fn check_worker(&self, head: &str, budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
        let journal = loop {
            check(budget, cancelled)?;
            match self.journal.try_lock() {
                Ok(journal) => break journal,
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("managed task project guard is poisoned"),
                Err(TryLockError::WouldBlock) => std::thread::sleep(budget.phase_limit(Duration::from_millis(10))?),
            }
        };
        self.lease.check_journal(&journal)?;self.lease.wait_shared_current(budget,cancelled)?;
        let state = journal.project_state(&self.lease.owner().project_id)?;
        ensure!(state.owner_active && state.owner == *self.lease.owner() && state.integration_head == head,
            "managed task selection differs from the current owner/integration head");
        ensure!(self.store.reopen_current(&journal)? == self.store, "managed task private namespace changed");
        check(budget, cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedTaskTarget { run_id: String, task: String, model: String, backend: Backend,
    mode: AgentMode, status: AgentStatus }
impl ManagedTaskTarget {
    pub(crate) fn run_id(&self) -> &str { &self.run_id }
    pub(crate) fn task(&self) -> &str { &self.task }
    pub(crate) fn model(&self) -> &str { &self.model }
    pub(crate) fn backend(&self) -> Backend { self.backend }
    /// App-only, CPU/data checks. Caller creates a NEW stopped Execute row using
    /// the existing row builder; this entry never starts a legacy row first.
    fn capture(app: &App, index: usize) -> Result<Self> {
        let run = app.agents.get(index).context("managed task row is absent")?;
        ensure!(run.terminal.is_none()
            && run.managed_local.is_none() && run.managed_store.is_none() && run.session_id.is_none() && !run.plain_process
            && !run.is_main() && !run.is_orchestrator() && !crate::is_cloud_owned_agent(run)
            && run.workspace_path.is_none() && run.jj_change_id.is_none(), "managed task requires a new explicitly selected stopped row");
        Self::validate(run.id.clone(), run.task.clone(), run.model.clone(), run.backend, run.mode, run.status)
    }
    fn validate(run_id: String, task: String, model: String, backend: Backend,
        mode: AgentMode, status: AgentStatus) -> Result<Self> {
        ensure!(mode == AgentMode::Execute && status == AgentStatus::Stopped,
            "managed task requires an Execute row in Stopped status");
        ensure!(!run_id.is_empty() && run_id.len() <= 256 && !run_id.contains(['/', '\\'])
            && !run_id.chars().any(char::is_control) && !matches!(run_id.as_str(), "." | ".."), "invalid managed task row identity");
        ensure!(!task.trim().is_empty() && task.len() <= 64 * 1024 && !task.contains('\0'), "managed task text exceeds bound");
        ensure!(!model.is_empty() && model.len() <= 256 && !model.chars().any(char::is_control), "explicit managed model is required");
        ensure!(matches!(backend, Backend::Codex | Backend::Claude), "managed task backend is unsupported");
        Ok(Self { run_id, task, model, backend, mode, status })
    }
}

/// Explicit qualified guest runtime selection. Programs/model/task become argv
/// DATA, never a shell string, raw-command readiness marker or PATH lookup.
pub(crate) enum ManagedTaskPrograms {
    Codex { program: String },
    Claude { node: String, rudder_cli: String, claude: String },
}
impl ManagedTaskPrograms {
    fn bind(self, target: &ManagedTaskTarget) -> Result<ManagedBackendProfile> {
        match (self, target.backend) {
            (Self::Codex { program }, Backend::Codex) => Ok(ManagedBackendProfile::CodexAppServer { program,
                arguments: vec!["--model".into(), target.model.clone(), "--".into(), target.task.clone()] }),
            (Self::Claude { node, rudder_cli, claude }, Backend::Claude) => Ok(ManagedBackendProfile::ClaudeSdk {
                node, rudder_cli, claude, arguments: vec!["--model".into(), target.model.clone()], initial_prompt: Some(target.task.clone()) }),
            _ => anyhow::bail!("managed task program/profile backend mismatch"),
        }
    }
}

/// Construct ONLY on the worker from an explicitly retained accepted root/seed
/// pair. This module verifies retention/identity, but cannot invent the upstream
/// accepted-root producer or turn a checkout scan into accepted history.
pub(crate) struct ManagedTaskResources {
    pub backed_inputs:Option<Arc<crate::managed_initial_source::task_backing::ManagedBackedTaskInputs>>,
    pub expected_integration_head: String,
    pub store: LocalStore,
    pub runnable_root: ObjectRef,
    pub seed_source: ObjectRef,
    /// One verified immutable template per retained seed, shared by siblings.
    /// This Arc does not replace the machine's actual Root storage read owner.
    pub seed: Arc<VerifiedMetadataTemplate>,
    pub programs: ManagedTaskPrograms,
    pub runtime: Vec<ReadOnlyRuntime>,
    pub protected_credential_roots: Vec<PathBuf>,
    pub endpoint_parent: PathBuf,
    pub native_program: PathBuf,
    pub native_guest: String,
    pub jj_guest: String,
    pub user_name: String,
    pub user_email: String,
    pub timeout_seconds: u64,
    /// Actual optional project cache, never a bool/hit claim. Root forwards it
    /// and the SAME budget to managed_launch; no cache implementation lives here.
    pub preparation_cache: Option<Arc<crate::managed_preparation_cache::ManagedPreparationCache>>,
}

struct Permit(Arc<PermitCharge>);
struct PermitCharge(Arc<AtomicUsize>);
impl Drop for PermitCharge {fn drop(&mut self){self.0.fetch_sub(1,Ordering::AcqRel);}}
/// Retention only, never a second admission or selected-profile authority.
/// The original factory carrier holds it through actual callback/TLS joins and
/// unused-result settlement, including consuming prepare() errors.
#[derive(Clone)]
pub(crate) struct ManagedTaskFactoryRetention(Arc<PermitCharge>);
pub(crate) struct ManagedTaskController { active: Arc<AtomicUsize> }
impl ManagedTaskController {
    pub(crate) fn new() -> Self { Self { active: Arc::new(AtomicUsize::new(0)) } }
    /// App-only admission; reserve time BEFORE queue/work. Root moves this
    /// non-Clone owner to its bounded worker. Timeout/cancel never free its slot.
    pub(crate) fn admit(&self, app: &App, index: usize, selected: ManagedTaskOptIn) -> Result<ManagedTaskAdmission> {
        self.admit_target(ManagedTaskTarget::capture(app, index)?, selected)
    }
    /// App-only actual task admission for a previously accepted host intent.
    /// Native dispatcher captured this SAME30s before queue/work, independently
    /// of its20s transport admission clock. Never mint another on duplicate.
    pub(crate) fn admit_original(&self,app:&App,index:usize,selected:ManagedTaskOptIn,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<ManagedTaskAdmission> {
        self.admit_original_target(ManagedTaskTarget::capture(app,index)?,selected,original,cancelled)
    }
    fn admit_original_target(&self,target:ManagedTaskTarget,selected:ManagedTaskOptIn,
        original:&FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<ManagedTaskAdmission> {
        check(original,&cancelled)?;
        ensure!(target.backend==selected.backend,"selected profile differs from exact task backend");
        self.active.fetch_update(Ordering::AcqRel,Ordering::Acquire,|n|(n<MAX_PREPARATIONS).then_some(n+1)).map_err(|_|anyhow::anyhow!("managed task preparation is busy"))?;
        Ok(ManagedTaskAdmission {target,selected,budget:original.clone(),cancelled,_permit:Permit(Arc::new(PermitCharge(self.active.clone())))})
    }
    fn admit_target(&self, target: ManagedTaskTarget, selected: ManagedTaskOptIn) -> Result<ManagedTaskAdmission> {
        ensure!(target.backend == selected.backend, "selected profile differs from task backend");
        let budget = FinalizationBudget::begin(PREPARATION_LIMIT)?;
        self.active.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |active| (active < MAX_PREPARATIONS).then_some(active + 1))
            .map_err(|_| anyhow::anyhow!("managed task preparation is busy"))?;
        Ok(ManagedTaskAdmission { target, selected, budget, cancelled: Arc::new(AtomicBool::new(false)), _permit: Permit(Arc::new(PermitCharge(self.active.clone()))) })
    }
}
fn check(budget: &FinalizationBudget, cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Acquire), "managed task preparation cancelled");
    budget.remaining()?; Ok(())
}
#[derive(Clone)]
pub(crate) struct ManagedTaskCancellation { budget: FinalizationBudget, cancelled: Arc<AtomicBool> }
impl ManagedTaskCancellation { pub(crate) fn cancel(&self) { self.cancelled.store(true, Ordering::Release); self.budget.cancel(); } }

#[must_use = "retain admission until its actual preparation call settles"]
pub(crate) struct ManagedTaskAdmission { target: ManagedTaskTarget, selected: ManagedTaskOptIn,
    budget: FinalizationBudget, cancelled: Arc<AtomicBool>, _permit: Permit }
impl ManagedTaskAdmission {
    pub(crate) fn retain_factory_until_join(&self)->ManagedTaskFactoryRetention {ManagedTaskFactoryRetention(self._permit.0.clone())}
    pub(crate) fn source_lifetime(&self)->(FinalizationBudget,Arc<AtomicBool>) {(self.budget.clone(),self.cancelled.clone())}

    #[cfg(test)]
    pub(crate) fn wiring_test_parts(&self) -> (FinalizationBudget,Arc<AtomicBool>) {
        (self.budget.clone(),self.cancelled.clone())
    }
    pub(crate) fn cancellation(&self) -> ManagedTaskCancellation {
        ManagedTaskCancellation { budget: self.budget.clone(), cancelled: self.cancelled.clone() }
    }
    fn load_profile(&self, loader: impl FnOnce(&ManagedTaskOptIn, &AtomicBool, &FinalizationBudget) -> Result<SelectedTaskProfile>) -> Result<SelectedTaskProfile> {
        check(&self.budget, &self.cancelled)?;
        let profile = loader(&self.selected, &self.cancelled, &self.budget)
            .map_err(|_| anyhow::anyhow!("explicit selected managed profile could not be loaded"))?;
        check(&self.budget, &self.cancelled)?;
        ensure!(profile.selected == self.selected, "credential owner differs from the exact selected profile");
        Ok(profile)
    }
    /// OFF-APP blocking preparation, never from the App actor. No thread is
    /// detached here: the caller owns this admitted call through actual return.
    /// Loaders may read ONE selected private API-key profile, never call models,
    /// scan HOME/env, refresh OAuth, migrate source or issue backend authority.
    pub(crate) fn prepare(self, project: &Arc<ManagedTaskProject>,
        loader: impl FnOnce(&ManagedTaskOptIn, &AtomicBool, &FinalizationBudget) -> Result<SelectedTaskProfile>,
        resources: impl FnOnce(&ManagedTaskTarget, &AtomicBool, &FinalizationBudget) -> Result<ManagedTaskResources>) -> Result<PreparedManagedTask> {
        check(&self.budget, &self.cancelled)?;
        project.lease.wait_shared_current(&self.budget,&self.cancelled)?;
        let profile = self.load_profile(loader)?;
        let resources = resources(&self.target, &self.cancelled, &self.budget)?;
        project.check_worker(&resources.expected_integration_head, &self.budget, &self.cancelled)?;
        let backend = resources.programs.bind(&self.target)?;
        profile.credentials.check_backend(&backend)?;
        backend.validate_runtime(&resources.runtime, &[resources.native_guest.as_str(), resources.jj_guest.as_str()])?;
        ensure!(normalized(&resources.native_program) && normalized(&resources.endpoint_parent), "managed task paths must be explicit normalized absolute identities");
        ensure!((1..=24*60*60).contains(&resources.timeout_seconds), "managed task timeout exceeds bound");
        resources.runnable_root.validate()?; resources.seed_source.validate()?;
        ensure!(resources.runnable_root.kind == ObjectKind::RunnableRoot && resources.seed_source.kind == ObjectKind::SourceRoot,
            "managed task requires explicit retained runnable/source roots");
        let seed = resources.seed.retained_seed().context("managed task requires an actually retained private VCS seed")?;
        let retention = resources.store.retention()?;
        if let Some(backed)=&resources.backed_inputs {backed.check_for_new_task(&self.budget,&self.cancelled)?;backed.matches(&resources.store,&resources.runnable_root,&resources.seed_source)?;}
        else {ensure!(retention.pins.values().any(|r|r==&resources.runnable_root),"managed task local runnable retention absent");}
        ensure!(retention.pins.values().any(|r|r==seed),"managed task seed retention absent");
        let runnable_bytes=if let Some(backed)=&resources.backed_inputs{backed.metadata(&resources.runnable_root,&self.budget,&self.cancelled)?}else{resources.store.read_object(&resources.runnable_root)?};
        let runnable=rudder_engine::snapshot::decode_runnable(&runnable_bytes)?;
        if let Some(backed)=&resources.backed_inputs{ensure!(runnable.environment.as_ref()==Some(backed.environment()),"task runtime environment differs from actual backed preparation");}
        ensure!(runnable.source == resources.seed_source
            && resources.seed.manifest().baseline.commit.0 == resources.expected_integration_head,
            "managed task retained source/seed/head selection differs");
        project.check_worker(&resources.expected_integration_head, &self.budget, &self.cancelled)?;
        let binding = project.store.binding();
        let inputs = ManagedLocalLaunchInputs { backed_inputs:resources.backed_inputs, journal: binding.journal.clone(), home: binding.home.clone(), managed_store: project.store.clone(),
            store: resources.store, runnable_root: resources.runnable_root, seed_source: resources.seed_source, seed: resources.seed,
            preparation_parent: binding.directory.join("preparations"), endpoint_parent: resources.endpoint_parent, backend,
            credentials: profile.credentials, protected_credential_roots: resources.protected_credential_roots,
            runtime: resources.runtime, native_guest: resources.native_guest, jj_guest: resources.jj_guest,
            user_name: resources.user_name, user_email: resources.user_email, timeout_seconds: resources.timeout_seconds };
        Ok(PreparedManagedTask { admission: self, project: Arc::downgrade(project), head: resources.expected_integration_head,
            inputs, native_program: resources.native_program, preparation_cache: resources.preparation_cache,seed_fork:None })
    }
}
fn normalized(path: &Path) -> bool {
    path.is_absolute() && path.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().split(|byte| *byte == b'/').any(|part| matches!(part, b"." | b".."))
}

#[must_use = "App must recheck the exact row/owner before moving the launch packet"]
pub(crate) struct PreparedManagedTask { inputs: ManagedLocalLaunchInputs, native_program: PathBuf,
    preparation_cache: Option<Arc<crate::managed_preparation_cache::ManagedPreparationCache>>,
    seed_fork:Option<Arc<crate::managed_seed_fork::ManagedSeedForkCell>>,
    project: Weak<ManagedTaskProject>, head: String,
    // A rejected/stale result must dispose cleanup-bearing inputs/cache before
    // releasing its charged slot. Moving into the packet preserves that order.
    admission: ManagedTaskAdmission }
impl PreparedManagedTask {
    pub(crate) fn target_run_id(&self)->&str {self.admission.target.run_id()}
    pub(crate) fn original_budget(&self)->&FinalizationBudget {&self.admission.budget}
    pub(crate) fn install_seed_fork_retaining(&mut self,fork:&Arc<crate::managed_seed_fork::ManagedSeedForkCell>)->Result<()> {
        check(&self.admission.budget,&self.admission.cancelled)?;ensure!(self.seed_fork.is_none(),"task fork owner already installed");
        fork.matches_inputs(&self.inputs)?;fork.matches_lifetime(&self.admission.budget,&self.admission.cancelled)?;fork.matches_cache(&self.preparation_cache)?;self.seed_fork=Some(fork.clone());check(&self.admission.budget,&self.admission.cancelled)
    }
    pub(crate) fn has_seed_fork(&self,fork:&Arc<crate::managed_seed_fork::ManagedSeedForkCell>)->bool {
        self.seed_fork.as_ref().is_some_and(|actual|Arc::ptr_eq(actual,fork))
    }

    /// App-only adoption. No path reopening, profile IO, materialization, model
    /// call or PTY mutation. Uses the already-held owner Journal, like the
    /// existing start_managed_local admission, to reject a changed accepted head.
    fn check_adoption(&self, app: &App, index: usize, project: &Arc<ManagedTaskProject>, journal: &Journal, owner: &OwnerFence) -> Result<()> {
        check(&self.admission.budget, &self.admission.cancelled)?;
        ensure!(Weak::ptr_eq(&self.project, &Arc::downgrade(project)) && owner == project.lease.owner(), "managed task prepared owner was replaced");
        project.lease.check_active_for_adoption()?;
        ensure!(ManagedTaskTarget::capture(app, index)? == self.admission.target && app.cwd == project.store.binding().repository,
            "managed task row/task/model changed during preparation");
        let current = journal.project_state(&owner.project_id)?;
        ensure!(current.owner_active && current.owner == *owner && current.integration_head == self.head,
            "managed task owner/head advanced before admission");
        check(&self.admission.budget, &self.admission.cancelled)?;
        Ok(())
    }
    /// Retaining App adoption. Every fallible check borrows this original owner;
    /// a stale/error result is returned intact for bounded OFF-App settlement.
    pub(crate) fn adopt_retaining(self, app: &App, index: usize, project: &Arc<ManagedTaskProject>, journal: &Journal,
        owner: &OwnerFence) -> std::result::Result<ManagedTaskLaunchPacket, (Self, anyhow::Error)> {
        if let Err(error) = self.check_adoption(app, index, project, journal, owner) { return Err((self, error)); }
        Ok(ManagedTaskLaunchPacket { inputs: self.inputs, native_program: self.native_program, budget: self.admission.budget.clone(),
            cancelled: self.admission.cancelled.clone(), preparation_cache: self.preparation_cache,seed_fork:self.seed_fork, retention: self.admission })
    }
    /// Existing consuming API preserved for old explicit callers. New App task
    /// wiring must use adopt_retaining and dispose rejected owners off-App.
    pub(crate) fn adopt(self, app: &App, index: usize, project: &Arc<ManagedTaskProject>, journal: &Journal, owner: &OwnerFence) -> Result<ManagedTaskLaunchPacket> {
        self.adopt_retaining(app, index, project, journal, owner).map_err(|(_, error)| error)
    }
}
/// Root must forward the SAME original budget/cancellation/cache into its
/// managed_app/managed_launch wiring, then call existing start_managed_local.
/// No runtime/helper readiness or checkpoint acceptance is returned here.
#[must_use = "forward the whole packet; never replace its original budget"]
pub(crate) struct ManagedTaskLaunchPacket {
    pub inputs: ManagedLocalLaunchInputs,
    pub native_program: PathBuf,
    pub budget: FinalizationBudget,
    pub cancelled: Arc<AtomicBool>,
    pub preparation_cache: Option<Arc<crate::managed_preparation_cache::ManagedPreparationCache>>,
    /// Keep this actual owner through the downstream preparation callback/result
    /// settlement, including timeout/cancel. Its Drop only releases a counter.
    pub seed_fork:Option<Arc<crate::managed_seed_fork::ManagedSeedForkCell>>,
    pub retention: ManagedTaskAdmission,
}
impl ManagedTaskLaunchPacket {
    pub(crate) fn target_run_id(&self)->&str {self.retention.target.run_id()}
    pub(crate) fn matches_target(&self, app: &App, index: usize) -> Result<()> {
        ensure!(ManagedTaskTarget::capture(app, index)? == self.retention.target,
            "managed task row/task/model changed before queue admission");
        ensure!(!self.cancelled.load(Ordering::Acquire), "managed task preparation cancelled");self.budget.remaining()?;Ok(())
    }
}

#[cfg(test)]
#[path = "managed_task_entry_admission_tests.rs"]
mod tests;

#[cfg(test)]
#[path="managed_ingress_admission_tests.rs"]
mod ingress_admission_tests;
